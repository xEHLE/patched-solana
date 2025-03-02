use {
    crate::{admin_rpc_service, cli::DefaultArgs},
    clap::{App, Arg, ArgGroup, ArgMatches, SubCommand},
    std::{net::SocketAddr, path::Path},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("set-public-address")
        .about("Specify addresses to advertise in gossip")
        .arg(
            Arg::with_name("tpu_addr")
                .long("tpu")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("TPU address to advertise in gossip"),
        )
        .arg(
            Arg::with_name("tpu_forwards_addr")
                .long("tpu-forwards")
                .value_name("HOST:PORT")
                .takes_value(true)
                .validator(solana_net_utils::is_host_port)
                .help("TPU Forwards address to advertise in gossip"),
        )
        .group(
            ArgGroup::with_name("set_public_address_details")
                .args(&["tpu_addr", "tpu_forwards_addr"])
                .required(true)
                .multiple(true),
        )
        .after_help("Note: At least one arg must be used. Using multiple is ok")
}

pub fn execute(matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    let parse_arg_addr = |arg_name: &str, arg_long: &str| -> Result<Option<SocketAddr>, String> {
        matches.value_of(arg_name).map(|host_port| {
            solana_net_utils::parse_host_port(host_port).map_err(|err| {
                format!(
                    "failed to parse --{arg_long} address. It must be in the HOST:PORT format. {err}"
                )
            })
        })
        .transpose()
    };
    let tpu_addr = parse_arg_addr("tpu_addr", "tpu")?;
    let tpu_forwards_addr = parse_arg_addr("tpu_forwards_addr", "tpu-forwards")?;

    macro_rules! set_public_address {
        ($public_addr:expr, $set_public_address:ident, $request:literal) => {
            if let Some(public_addr) = $public_addr {
                let admin_client = admin_rpc_service::connect(ledger_path);
                admin_rpc_service::runtime()
                    .block_on(
                        async move { admin_client.await?.$set_public_address(public_addr).await },
                    )
                    .map_err(|err| format!("{} request failed: {err}", $request))
            } else {
                Ok(())
            }
        };
    }
    set_public_address!(tpu_addr, set_public_tpu_address, "set public tpu address")?;
    set_public_address!(
        tpu_forwards_addr,
        set_public_tpu_forwards_address,
        "set public tpu forwards address"
    )
}
