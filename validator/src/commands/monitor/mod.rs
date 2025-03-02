use {
    crate::{cli::DefaultArgs, dashboard::Dashboard},
    clap::{App, ArgMatches, SubCommand},
    std::{path::Path, time::Duration},
};

pub fn command(_default_args: &DefaultArgs) -> App<'_, '_> {
    SubCommand::with_name("monitor").about("Monitor the validator")
}

pub fn execute(_matches: &ArgMatches, ledger_path: &Path) -> Result<(), String> {
    monitor_validator(ledger_path)
}

pub fn monitor_validator(ledger_path: &Path) -> Result<(), String> {
    let dashboard = Dashboard::new(ledger_path, None, None);
    dashboard.run(Duration::from_secs(2));

    Ok(())
}
