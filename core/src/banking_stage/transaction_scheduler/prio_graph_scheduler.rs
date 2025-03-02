use {
    super::{
        in_flight_tracker::InFlightTracker,
        scheduler::{PreLockFilterAction, Scheduler},
        scheduler_error::SchedulerError,
        thread_aware_account_locks::{ThreadAwareAccountLocks, ThreadId, ThreadSet, TryLockError},
        transaction_state::SanitizedTransactionTTL,
    },
    crate::banking_stage::{
        consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        read_write_account_set::ReadWriteAccountSet,
        scheduler_messages::{
            ConsumeWork, FinishedConsumeWork, MaxAge, TransactionBatchId, TransactionId,
        },
        transaction_scheduler::{
            scheduler::SchedulingSummary, transaction_priority_id::TransactionPriorityId,
            transaction_state::TransactionState, transaction_state_container::StateContainer,
        },
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    itertools::izip,
    prio_graph::{AccessKind, GraphNode, PrioGraph},
    solana_cost_model::block_cost_limits::MAX_BLOCK_UNITS,
    solana_measure::measure_us,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::{pubkey::Pubkey, saturating_add_assign},
    solana_svm_transaction::svm_message::SVMMessage,
};

#[inline(always)]
fn passthrough_priority(
    id: &TransactionPriorityId,
    _graph_node: &GraphNode<TransactionPriorityId>,
) -> TransactionPriorityId {
    *id
}

type SchedulerPrioGraph = PrioGraph<
    TransactionPriorityId,
    Pubkey,
    TransactionPriorityId,
    fn(&TransactionPriorityId, &GraphNode<TransactionPriorityId>) -> TransactionPriorityId,
>;

pub(crate) struct PrioGraphSchedulerConfig {
    pub max_scheduled_cus: u64,
    pub max_scanned_transactions_per_scheduling_pass: usize,
    pub look_ahead_window_size: usize,
    pub target_transactions_per_batch: usize,
}

impl Default for PrioGraphSchedulerConfig {
    fn default() -> Self {
        Self {
            max_scheduled_cus: MAX_BLOCK_UNITS,
            max_scanned_transactions_per_scheduling_pass: 1000,
            look_ahead_window_size: 256,
            target_transactions_per_batch: TARGET_NUM_TRANSACTIONS_PER_BATCH,
        }
    }
}

pub(crate) struct PrioGraphScheduler<Tx> {
    in_flight_tracker: InFlightTracker,
    account_locks: ThreadAwareAccountLocks,
    consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
    finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
    prio_graph: SchedulerPrioGraph,
    config: PrioGraphSchedulerConfig,
}

impl<Tx: TransactionWithMeta> PrioGraphScheduler<Tx> {
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        config: PrioGraphSchedulerConfig,
    ) -> Self {
        let num_threads = consume_work_senders.len();
        Self {
            in_flight_tracker: InFlightTracker::new(num_threads),
            account_locks: ThreadAwareAccountLocks::new(num_threads),
            consume_work_senders,
            finished_consume_work_receiver,
            prio_graph: PrioGraph::new(passthrough_priority),
            config,
        }
    }
}

impl<Tx: TransactionWithMeta> Scheduler<Tx> for PrioGraphScheduler<Tx> {
    /// Schedule transactions from the given `StateContainer` to be
    /// consumed by the worker threads. Returns summary of scheduling, or an
    /// error.
    /// `pre_graph_filter` is used to filter out transactions that should be
    /// skipped and dropped before insertion to the prio-graph. This fn should
    /// set `false` for transactions that should be dropped, and `true`
    /// otherwise.
    /// `pre_lock_filter` is used to filter out transactions after they have
    /// made it to the top of the prio-graph, and immediately before locks are
    /// checked and taken. This fn should return `true` for transactions that
    /// should be scheduled, and `false` otherwise.
    ///
    /// Uses a `PrioGraph` to perform look-ahead during the scheduling of transactions.
    /// This, combined with internal tracking of threads' in-flight transactions, allows
    /// for load-balancing while prioritizing scheduling transactions onto threads that will
    /// not cause conflicts in the near future.
    fn schedule<S: StateContainer<Tx>>(
        &mut self,
        container: &mut S,
        pre_graph_filter: impl Fn(&[&Tx], &mut [bool]),
        pre_lock_filter: impl Fn(&TransactionState<Tx>) -> PreLockFilterAction,
    ) -> Result<SchedulingSummary, SchedulerError> {
        let num_threads = self.consume_work_senders.len();
        let max_cu_per_thread = self.config.max_scheduled_cus / num_threads as u64;

        let mut schedulable_threads = ThreadSet::any(num_threads);
        for thread_id in 0..num_threads {
            if self.in_flight_tracker.cus_in_flight_per_thread()[thread_id] >= max_cu_per_thread {
                schedulable_threads.remove(thread_id);
            }
        }
        if schedulable_threads.is_empty() {
            return Ok(SchedulingSummary {
                num_scheduled: 0,
                num_unschedulable: 0,
                num_filtered_out: 0,
                filter_time_us: 0,
            });
        }

        let mut batches = Batches::new(num_threads, self.config.target_transactions_per_batch);
        // Some transactions may be unschedulable due to multi-thread conflicts.
        // These transactions cannot be scheduled until some conflicting work is completed.
        // However, the scheduler should not allow other transactions that conflict with
        // these transactions to be scheduled before them.
        let mut unschedulable_ids = Vec::new();
        let mut blocking_locks = ReadWriteAccountSet::default();

        // Track metrics on filter.
        let mut num_filtered_out: usize = 0;
        let mut total_filter_time_us: u64 = 0;

        let mut window_budget = self.config.look_ahead_window_size;
        let mut chunked_pops = |container: &mut S,
                                prio_graph: &mut PrioGraph<_, _, _, _>,
                                window_budget: &mut usize| {
            while *window_budget > 0 {
                const MAX_FILTER_CHUNK_SIZE: usize = 128;
                let mut filter_array = [true; MAX_FILTER_CHUNK_SIZE];
                let mut ids = Vec::with_capacity(MAX_FILTER_CHUNK_SIZE);
                let mut txs = Vec::with_capacity(MAX_FILTER_CHUNK_SIZE);

                let chunk_size = (*window_budget).min(MAX_FILTER_CHUNK_SIZE);
                for _ in 0..chunk_size {
                    if let Some(id) = container.pop() {
                        ids.push(id);
                    } else {
                        break;
                    }
                }
                *window_budget = window_budget.saturating_sub(chunk_size);

                ids.iter().for_each(|id| {
                    let transaction = container.get_transaction_ttl(id.id).unwrap();
                    txs.push(&transaction.transaction);
                });

                let (_, filter_us) =
                    measure_us!(pre_graph_filter(&txs, &mut filter_array[..chunk_size]));
                saturating_add_assign!(total_filter_time_us, filter_us);

                for (id, filter_result) in ids.iter().zip(&filter_array[..chunk_size]) {
                    if *filter_result {
                        let transaction = container.get_transaction_ttl(id.id).unwrap();
                        prio_graph.insert_transaction(
                            *id,
                            Self::get_transaction_account_access(transaction),
                        );
                    } else {
                        saturating_add_assign!(num_filtered_out, 1);
                        container.remove_by_id(id.id);
                    }
                }

                if ids.len() != chunk_size {
                    break;
                }
            }
        };

        // Create the initial look-ahead window.
        // Check transactions against filter, remove from container if it fails.
        chunked_pops(container, &mut self.prio_graph, &mut window_budget);

        let mut unblock_this_batch = Vec::with_capacity(
            self.consume_work_senders.len() * self.config.target_transactions_per_batch,
        );
        let mut num_scanned: usize = 0;
        let mut num_scheduled: usize = 0;
        let mut num_sent: usize = 0;
        let mut num_unschedulable: usize = 0;
        while num_scanned < self.config.max_scanned_transactions_per_scheduling_pass {
            // If nothing is in the main-queue of the `PrioGraph` then there's nothing left to schedule.
            if self.prio_graph.is_empty() {
                break;
            }

            while let Some(id) = self.prio_graph.pop() {
                num_scanned += 1;
                unblock_this_batch.push(id);

                // Should always be in the container, during initial testing phase panic.
                // Later, we can replace with a continue in case this does happen.
                let Some(transaction_state) = container.get_mut_transaction_state(id.id) else {
                    panic!("transaction state must exist")
                };

                let maybe_schedule_info = try_schedule_transaction(
                    transaction_state,
                    &pre_lock_filter,
                    &mut blocking_locks,
                    &mut self.account_locks,
                    num_threads,
                    |thread_set| {
                        Self::select_thread(
                            thread_set,
                            &batches.total_cus,
                            self.in_flight_tracker.cus_in_flight_per_thread(),
                            &batches.transactions,
                            self.in_flight_tracker.num_in_flight_per_thread(),
                        )
                    },
                );

                match maybe_schedule_info {
                    Err(TransactionSchedulingError::UnschedulableConflicts)
                    | Err(TransactionSchedulingError::UnschedulableThread) => {
                        unschedulable_ids.push(id);
                        saturating_add_assign!(num_unschedulable, 1);
                    }
                    Ok(TransactionSchedulingInfo {
                        thread_id,
                        transaction,
                        max_age,
                        cost,
                    }) => {
                        saturating_add_assign!(num_scheduled, 1);
                        batches.transactions[thread_id].push(transaction);
                        batches.ids[thread_id].push(id.id);
                        batches.max_ages[thread_id].push(max_age);
                        saturating_add_assign!(batches.total_cus[thread_id], cost);

                        // If target batch size is reached, send only this batch.
                        if batches.ids[thread_id].len() >= self.config.target_transactions_per_batch
                        {
                            saturating_add_assign!(
                                num_sent,
                                self.send_batch(&mut batches, thread_id)?
                            );
                        }

                        // if the thread is at max_cu_per_thread, remove it from the schedulable threads
                        // if there are no more schedulable threads, stop scheduling.
                        if self.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                            + batches.total_cus[thread_id]
                            >= max_cu_per_thread
                        {
                            schedulable_threads.remove(thread_id);
                            if schedulable_threads.is_empty() {
                                break;
                            }
                        }
                    }
                }

                if num_scanned >= self.config.max_scanned_transactions_per_scheduling_pass {
                    break;
                }
            }

            // Send all non-empty batches
            saturating_add_assign!(num_sent, self.send_batches(&mut batches)?);

            // Refresh window budget and do chunked pops
            saturating_add_assign!(window_budget, unblock_this_batch.len());
            chunked_pops(container, &mut self.prio_graph, &mut window_budget);

            // Unblock all transactions that were blocked by the transactions that were just sent.
            for id in unblock_this_batch.drain(..) {
                self.prio_graph.unblock(&id);
            }
        }

        // Send batches for any remaining transactions
        saturating_add_assign!(num_sent, self.send_batches(&mut batches)?);

        // Push unschedulable ids back into the container
        container.push_ids_into_queue(unschedulable_ids.into_iter());

        // Push remaining transactions back into the container
        container.push_ids_into_queue(std::iter::from_fn(|| {
            self.prio_graph.pop_and_unblock().map(|(id, _)| id)
        }));

        // No more remaining items in the queue.
        // Clear here to make sure the next scheduling pass starts fresh
        // without detecting any conflicts.
        self.prio_graph.clear();

        assert_eq!(
            num_scheduled, num_sent,
            "number of scheduled and sent transactions must match"
        );

        Ok(SchedulingSummary {
            num_scheduled,
            num_unschedulable,
            num_filtered_out,
            filter_time_us: total_filter_time_us,
        })
    }

    /// Receive completed batches of transactions without blocking.
    /// Returns (num_transactions, num_retryable_transactions) on success.
    fn receive_completed(
        &mut self,
        container: &mut impl StateContainer<Tx>,
    ) -> Result<(usize, usize), SchedulerError> {
        let mut total_num_transactions: usize = 0;
        let mut total_num_retryable: usize = 0;
        loop {
            let (num_transactions, num_retryable) = self.try_receive_completed(container)?;
            if num_transactions == 0 {
                break;
            }
            saturating_add_assign!(total_num_transactions, num_transactions);
            saturating_add_assign!(total_num_retryable, num_retryable);
        }
        Ok((total_num_transactions, total_num_retryable))
    }
}

impl<Tx: TransactionWithMeta> PrioGraphScheduler<Tx> {
    /// Receive completed batches of transactions.
    /// Returns `Ok((num_transactions, num_retryable))` if a batch was received, `Ok((0, 0))` if no batch was received.
    fn try_receive_completed(
        &mut self,
        container: &mut impl StateContainer<Tx>,
    ) -> Result<(usize, usize), SchedulerError> {
        match self.finished_consume_work_receiver.try_recv() {
            Ok(FinishedConsumeWork {
                work:
                    ConsumeWork {
                        batch_id,
                        ids,
                        transactions,
                        max_ages,
                    },
                retryable_indexes,
            }) => {
                let num_transactions = ids.len();
                let num_retryable = retryable_indexes.len();

                // Free the locks
                self.complete_batch(batch_id, &transactions);

                // Retryable transactions should be inserted back into the container
                let mut retryable_iter = retryable_indexes.into_iter().peekable();
                for (index, (id, transaction, max_age)) in
                    izip!(ids, transactions, max_ages).enumerate()
                {
                    if let Some(retryable_index) = retryable_iter.peek() {
                        if *retryable_index == index {
                            container.retry_transaction(
                                id,
                                SanitizedTransactionTTL {
                                    transaction,
                                    max_age,
                                },
                            );
                            retryable_iter.next();
                            continue;
                        }
                    }
                    container.remove_by_id(id);
                }

                Ok((num_transactions, num_retryable))
            }
            Err(TryRecvError::Empty) => Ok((0, 0)),
            Err(TryRecvError::Disconnected) => Err(SchedulerError::DisconnectedRecvChannel(
                "finished consume work",
            )),
        }
    }

    /// Mark a given `TransactionBatchId` as completed.
    /// This will update the internal tracking, including account locks.
    fn complete_batch(&mut self, batch_id: TransactionBatchId, transactions: &[Tx]) {
        let thread_id = self.in_flight_tracker.complete_batch(batch_id);
        for transaction in transactions {
            let account_keys = transaction.account_keys();
            let write_account_locks = account_keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
            let read_account_locks = account_keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));
            self.account_locks
                .unlock_accounts(write_account_locks, read_account_locks, thread_id);
        }
    }

    /// Send all batches of transactions to the worker threads.
    /// Returns the number of transactions sent.
    fn send_batches(&mut self, batches: &mut Batches<Tx>) -> Result<usize, SchedulerError> {
        (0..self.consume_work_senders.len())
            .map(|thread_index| self.send_batch(batches, thread_index))
            .sum()
    }

    /// Send a batch of transactions to the given thread's `ConsumeWork` channel.
    /// Returns the number of transactions sent.
    fn send_batch(
        &mut self,
        batches: &mut Batches<Tx>,
        thread_index: usize,
    ) -> Result<usize, SchedulerError> {
        if batches.ids[thread_index].is_empty() {
            return Ok(0);
        }

        let (ids, transactions, max_ages, total_cus) =
            batches.take_batch(thread_index, self.config.target_transactions_per_batch);

        let batch_id = self
            .in_flight_tracker
            .track_batch(ids.len(), total_cus, thread_index);

        let num_scheduled = ids.len();
        let work = ConsumeWork {
            batch_id,
            ids,
            transactions,
            max_ages,
        };
        self.consume_work_senders[thread_index]
            .send(work)
            .map_err(|_| SchedulerError::DisconnectedSendChannel("consume work sender"))?;

        Ok(num_scheduled)
    }

    /// Given the schedulable `thread_set`, select the thread with the least amount
    /// of work queued up.
    /// Currently, "work" is just defined as the number of transactions.
    ///
    /// If the `chain_thread` is available, this thread will be selected, regardless of
    /// load-balancing.
    ///
    /// Panics if the `thread_set` is empty. This should never happen, see comment
    /// on `ThreadAwareAccountLocks::try_lock_accounts`.
    pub(crate) fn select_thread(
        thread_set: ThreadSet,
        batch_cus_per_thread: &[u64],
        in_flight_cus_per_thread: &[u64],
        batches_per_thread: &[Vec<Tx>],
        in_flight_per_thread: &[usize],
    ) -> ThreadId {
        thread_set
            .contained_threads_iter()
            .map(|thread_id| {
                (
                    thread_id,
                    batch_cus_per_thread[thread_id] + in_flight_cus_per_thread[thread_id],
                    batches_per_thread[thread_id].len() + in_flight_per_thread[thread_id],
                )
            })
            .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)))
            .map(|(thread_id, _, _)| thread_id)
            .unwrap()
    }

    /// Gets accessed accounts (resources) for use in `PrioGraph`.
    fn get_transaction_account_access(
        transaction: &SanitizedTransactionTTL<impl SVMMessage>,
    ) -> impl Iterator<Item = (Pubkey, AccessKind)> + '_ {
        let message = &transaction.transaction;
        message
            .account_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| {
                if message.is_writable(index) {
                    (*key, AccessKind::Write)
                } else {
                    (*key, AccessKind::Read)
                }
            })
    }
}

pub(crate) struct Batches<Tx> {
    pub ids: Vec<Vec<TransactionId>>,
    pub transactions: Vec<Vec<Tx>>,
    pub max_ages: Vec<Vec<MaxAge>>,
    pub total_cus: Vec<u64>,
}

impl<Tx> Batches<Tx> {
    pub(crate) fn new(num_threads: usize, target_num_transactions_per_batch: usize) -> Self {
        Self {
            ids: vec![Vec::with_capacity(target_num_transactions_per_batch); num_threads],

            transactions: (0..num_threads)
                .map(|_| Vec::with_capacity(target_num_transactions_per_batch))
                .collect(),
            max_ages: vec![Vec::with_capacity(target_num_transactions_per_batch); num_threads],
            total_cus: vec![0; num_threads],
        }
    }

    pub(crate) fn take_batch(
        &mut self,
        thread_id: ThreadId,
        target_num_transactions_per_batch: usize,
    ) -> (Vec<TransactionId>, Vec<Tx>, Vec<MaxAge>, u64) {
        (
            core::mem::replace(
                &mut self.ids[thread_id],
                Vec::with_capacity(target_num_transactions_per_batch),
            ),
            core::mem::replace(
                &mut self.transactions[thread_id],
                Vec::with_capacity(target_num_transactions_per_batch),
            ),
            core::mem::replace(
                &mut self.max_ages[thread_id],
                Vec::with_capacity(target_num_transactions_per_batch),
            ),
            core::mem::replace(&mut self.total_cus[thread_id], 0),
        )
    }
}

/// A transaction has been scheduled to a thread.
pub(crate) struct TransactionSchedulingInfo<Tx> {
    pub thread_id: ThreadId,
    pub transaction: Tx,
    pub max_age: MaxAge,
    pub cost: u64,
}

/// Error type for reasons a transaction could not be scheduled.
pub(crate) enum TransactionSchedulingError {
    /// Transaction cannot be scheduled due to conflicts, or
    /// higher priority conflicting transactions are unschedulable.
    UnschedulableConflicts,
    /// Thread is not allowed to be scheduled on at this time.
    UnschedulableThread,
}

fn try_schedule_transaction<Tx: TransactionWithMeta>(
    transaction_state: &mut TransactionState<Tx>,
    pre_lock_filter: impl Fn(&TransactionState<Tx>) -> PreLockFilterAction,
    blocking_locks: &mut ReadWriteAccountSet,
    account_locks: &mut ThreadAwareAccountLocks,
    num_threads: usize,
    thread_selector: impl Fn(ThreadSet) -> ThreadId,
) -> Result<TransactionSchedulingInfo<Tx>, TransactionSchedulingError> {
    match pre_lock_filter(transaction_state) {
        PreLockFilterAction::AttemptToSchedule => {}
    }

    // Check if this transaction conflicts with any blocked transactions
    let transaction = &transaction_state.transaction_ttl().transaction;
    if !blocking_locks.check_locks(transaction) {
        blocking_locks.take_locks(transaction);
        return Err(TransactionSchedulingError::UnschedulableConflicts);
    }

    // Schedule the transaction if it can be.
    let account_keys = transaction.account_keys();
    let write_account_locks = account_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
    let read_account_locks = account_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));

    let thread_id = match account_locks.try_lock_accounts(
        write_account_locks,
        read_account_locks,
        ThreadSet::any(num_threads),
        thread_selector,
    ) {
        Ok(thread_id) => thread_id,
        Err(TryLockError::MultipleConflicts) => {
            blocking_locks.take_locks(transaction);
            return Err(TransactionSchedulingError::UnschedulableConflicts);
        }
        Err(TryLockError::ThreadNotAllowed) => {
            blocking_locks.take_locks(transaction);
            return Err(TransactionSchedulingError::UnschedulableThread);
        }
    };

    let sanitized_transaction_ttl = transaction_state.transition_to_pending();
    let cost = transaction_state.cost();

    Ok(TransactionSchedulingInfo {
        thread_id,
        transaction: sanitized_transaction_ttl.transaction,
        max_age: sanitized_transaction_ttl.max_age,
        cost,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::transaction_scheduler::transaction_state_container::TransactionStateContainer,
        crossbeam_channel::{unbounded, Receiver},
        itertools::Itertools,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk::{
            compute_budget::ComputeBudgetInstruction,
            hash::Hash,
            message::Message,
            pubkey::Pubkey,
            signature::Keypair,
            signer::Signer,
            system_instruction,
            transaction::{SanitizedTransaction, Transaction},
        },
        std::borrow::Borrow,
    };

    #[allow(clippy::type_complexity)]
    fn create_test_frame(
        num_threads: usize,
    ) -> (
        PrioGraphScheduler<RuntimeTransaction<SanitizedTransaction>>,
        Vec<Receiver<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>>,
        Sender<FinishedConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    ) {
        let (consume_work_senders, consume_work_receivers) =
            (0..num_threads).map(|_| unbounded()).unzip();
        let (finished_consume_work_sender, finished_consume_work_receiver) = unbounded();
        let scheduler = PrioGraphScheduler::new(
            consume_work_senders,
            finished_consume_work_receiver,
            PrioGraphSchedulerConfig::default(),
        );
        (
            scheduler,
            consume_work_receivers,
            finished_consume_work_sender,
        )
    }

    fn prioritized_tranfers(
        from_keypair: &Keypair,
        to_pubkeys: impl IntoIterator<Item = impl Borrow<Pubkey>>,
        lamports: u64,
        priority: u64,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        let to_pubkeys_lamports = to_pubkeys
            .into_iter()
            .map(|pubkey| *pubkey.borrow())
            .zip(std::iter::repeat(lamports))
            .collect_vec();
        let mut ixs =
            system_instruction::transfer_many(&from_keypair.pubkey(), &to_pubkeys_lamports);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(priority);
        ixs.push(prioritization);
        let message = Message::new(&ixs, Some(&from_keypair.pubkey()));
        let tx = Transaction::new(&[from_keypair], message, Hash::default());
        RuntimeTransaction::from_transaction_for_tests(tx)
    }

    fn create_container(
        tx_infos: impl IntoIterator<
            Item = (
                impl Borrow<Keypair>,
                impl IntoIterator<Item = impl Borrow<Pubkey>>,
                u64,
                u64,
            ),
        >,
    ) -> TransactionStateContainer<RuntimeTransaction<SanitizedTransaction>> {
        create_container_with_capacity(100 * 1024, tx_infos)
    }

    fn create_container_with_capacity(
        capacity: usize,
        tx_infos: impl IntoIterator<
            Item = (
                impl Borrow<Keypair>,
                impl IntoIterator<Item = impl Borrow<Pubkey>>,
                u64,
                u64,
            ),
        >,
    ) -> TransactionStateContainer<RuntimeTransaction<SanitizedTransaction>> {
        let mut container = TransactionStateContainer::with_capacity(capacity);
        for (from_keypair, to_pubkeys, lamports, compute_unit_price) in tx_infos.into_iter() {
            let transaction = prioritized_tranfers(
                from_keypair.borrow(),
                to_pubkeys,
                lamports,
                compute_unit_price,
            );
            let transaction_ttl = SanitizedTransactionTTL {
                transaction,
                max_age: MaxAge::MAX,
            };
            const TEST_TRANSACTION_COST: u64 = 5000;
            container.insert_new_transaction(
                transaction_ttl,
                compute_unit_price,
                TEST_TRANSACTION_COST,
            );
        }

        container
    }

    fn collect_work(
        receiver: &Receiver<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    ) -> (
        Vec<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
        Vec<Vec<TransactionId>>,
    ) {
        receiver
            .try_iter()
            .map(|work| {
                let ids = work.ids.clone();
                (work, ids)
            })
            .unzip()
    }

    fn test_pre_graph_filter(
        _txs: &[&RuntimeTransaction<SanitizedTransaction>],
        results: &mut [bool],
    ) {
        results.fill(true);
    }

    fn test_pre_lock_filter(
        _tx: &TransactionState<RuntimeTransaction<SanitizedTransaction>>,
    ) -> PreLockFilterAction {
        PreLockFilterAction::AttemptToSchedule
    }

    #[test]
    fn test_schedule_disconnected_channel() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container([(&Keypair::new(), &[Pubkey::new_unique()], 1, 1)]);

        drop(work_receivers); // explicitly drop receivers
        assert_matches!(
            scheduler.schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter),
            Err(SchedulerError::DisconnectedSendChannel(_))
        );
    }

    #[test]
    fn test_schedule_single_threaded_no_conflicts() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container([
            (&Keypair::new(), &[Pubkey::new_unique()], 1, 1),
            (&Keypair::new(), &[Pubkey::new_unique()], 2, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1, 0]]);
    }

    #[test]
    fn test_schedule_single_threaded_conflict() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let pubkey = Pubkey::new_unique();
        let mut container = create_container([
            (&Keypair::new(), &[pubkey], 1, 1),
            (&Keypair::new(), &[pubkey], 1, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1], vec![0]]);
    }

    #[test]
    fn test_schedule_consume_single_threaded_multi_batch() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container(
            (0..4 * TARGET_NUM_TRANSACTIONS_PER_BATCH)
                .map(|i| (Keypair::new(), [Pubkey::new_unique()], i as u64, 1)),
        );

        // expect 4 full batches to be scheduled
        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(
            scheduling_summary.num_scheduled,
            4 * TARGET_NUM_TRANSACTIONS_PER_BATCH
        );
        assert_eq!(scheduling_summary.num_unschedulable, 0);

        let thread0_work_counts: Vec<_> = work_receivers[0]
            .try_iter()
            .map(|work| work.ids.len())
            .collect();
        assert_eq!(thread0_work_counts, [TARGET_NUM_TRANSACTIONS_PER_BATCH; 4]);
    }

    #[test]
    fn test_schedule_simple_thread_selection() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(2);
        let mut container =
            create_container((0..4).map(|i| (Keypair::new(), [Pubkey::new_unique()], 1, i)));

        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 4);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, [vec![3, 1]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![2, 0]]);
    }

    #[test]
    fn test_schedule_priority_guard() {
        let (mut scheduler, work_receivers, finished_work_sender) = create_test_frame(2);
        // intentionally shorten the look-ahead window to cause unschedulable conflicts
        scheduler.config.look_ahead_window_size = 2;

        let accounts = (0..8).map(|_| Keypair::new()).collect_vec();
        let mut container = create_container([
            (&accounts[0], &[accounts[1].pubkey()], 1, 6),
            (&accounts[2], &[accounts[3].pubkey()], 1, 5),
            (&accounts[4], &[accounts[5].pubkey()], 1, 4),
            (&accounts[6], &[accounts[7].pubkey()], 1, 3),
            (&accounts[1], &[accounts[2].pubkey()], 1, 2),
            (&accounts[2], &[accounts[3].pubkey()], 1, 1),
        ]);

        // The look-ahead window is intentionally shortened, high priority transactions
        // [0, 1, 2, 3] do not conflict, and are scheduled onto threads in a
        // round-robin fashion. This leads to transaction [4] being unschedulable due
        // to conflicts with [0] and [1], which were scheduled to different threads.
        // Transaction [5] is technically schedulable, onto thread 1 since it only
        // conflicts with transaction [1]. However, [5] will not be scheduled because
        // it conflicts with a higher-priority transaction [4] that is unschedulable.
        // The full prio-graph can be visualized as:
        // [0] \
        //      -> [4] -> [5]
        // [1] / ------/
        // [2]
        // [3]
        // Because the look-ahead window is shortened to a size of 4, the scheduler does
        // not have knowledge of the joining at transaction [4] until after [0] and [1]
        // have been scheduled.
        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 4);
        assert_eq!(scheduling_summary.num_unschedulable, 2);
        let (thread_0_work, thread_0_ids) = collect_work(&work_receivers[0]);
        assert_eq!(thread_0_ids, [vec![0], vec![2]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![1], vec![3]]);

        // Cannot schedule even on next pass because of lock conflicts
        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 0);
        assert_eq!(scheduling_summary.num_unschedulable, 2);

        // Complete batch on thread 0. Remaining txs can be scheduled onto thread 1
        finished_work_sender
            .send(FinishedConsumeWork {
                work: thread_0_work.into_iter().next().unwrap(),
                retryable_indexes: vec![],
            })
            .unwrap();
        scheduler.receive_completed(&mut container).unwrap();
        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable, 0);

        assert_eq!(collect_work(&work_receivers[1]).1, [vec![4], vec![5]]);
    }

    #[test]
    fn test_schedule_over_full_container() {
        let (mut scheduler, _work_receivers, _finished_work_sender) = create_test_frame(1);

        // set up a container is larger enough that single pass of schedulling will not deplete it.
        let capacity = scheduler
            .config
            .max_scanned_transactions_per_scheduling_pass
            + 2;
        let txs = (0..capacity)
            .map(|_| (Keypair::new(), [Pubkey::new_unique()], 1, 1))
            .collect_vec();
        let mut container = create_container_with_capacity(capacity, txs);

        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        // for each pass, it'd schedule no more than configured max_scanned_transactions_per_scheduling_pass
        let expected_num_scheduled = std::cmp::min(
            capacity,
            scheduler
                .config
                .max_scanned_transactions_per_scheduling_pass,
        );
        assert_eq!(scheduling_summary.num_scheduled, expected_num_scheduled);
        assert_eq!(scheduling_summary.num_unschedulable, 0);

        let mut post_schedule_remaining_ids = 0;
        while let Some(_p) = container.pop() {
            post_schedule_remaining_ids += 1;
        }

        // unscheduled ids should remain in the container
        assert_eq!(
            post_schedule_remaining_ids,
            capacity - expected_num_scheduled
        );
    }
}
