use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    solana_clap_utils::input_validators::is_keypair,
    solana_sdk::signature::{read_keypair, Signer},
    std::{fs, path::Path},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("set-identity")
        .about("Set the validator identity")
        .arg(
            Arg::with_name("identity")
                .index(1)
                .value_name("KEYPAIR")
                .required(false)
                .takes_value(true)
                .validator(is_keypair)
                .help("Path to validator identity keypair [default: read JSON keypair from stdin]"),
        )
        .arg(
            clap::Arg::with_name("require_tower")
                .long("require-tower")
                .takes_value(false)
                .help("Refuse to set the validator identity if saved tower state is not found"),
        )
        .after_help(
            "Note: the new identity only applies to the currently running validator instance",
        )
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    let require_tower = matches.is_present("require_tower");

    if let Ok(identity_keypair) = value_t!(matches, "identity", String) {
        let identity_keypair = fs::canonicalize(&identity_keypair)
            .map_err(|err| format!("unable to access path {identity_keypair}: {err:?}"))?;

        println!(
            "New validator identity path: {}",
            identity_keypair.display()
        );

        let admin_client = admin_rpc_service::connect(ledger_path);
        admin_rpc_service::runtime()
            .block_on(async move {
                admin_client
                    .await?
                    .set_identity(identity_keypair.display().to_string(), require_tower)
                    .await
            })
            .map_err(|err| format!("set identity request failed: {err}"))
    } else {
        let mut stdin = std::io::stdin();
        let identity_keypair = read_keypair(&mut stdin)
            .map_err(|err| format!("unable to read json keypair from stdin: {err:?}"))?;

        println!("New validator identity: {}", identity_keypair.pubkey());

        let admin_client = admin_rpc_service::connect(ledger_path);
        admin_rpc_service::runtime()
            .block_on(async move {
                admin_client
                    .await?
                    .set_identity_from_bytes(Vec::from(identity_keypair.to_bytes()), require_tower)
                    .await
            })
            .map_err(|err| format!("set identity request failed: {err}"))
    }
}
