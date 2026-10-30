use crate::config::{Config, SavingsStyle};
use eyre::Result;
use std::process::ExitCode;

#[derive(usage_rs::Args)]
pub(super) struct StatsArgs {
    /// Print a stable machine-readable report, with byte counts and nanoseconds.
    #[usage(long)]
    json: bool,
    /// Show compile memory estimates and a read-only scheduler activity snapshot.
    #[usage(long)]
    scheduler: bool,
    /// Print only scheduler activity as version 2 JSON; requires --scheduler and --json.
    #[usage(long)]
    activity: bool,
}

pub(super) fn run(config: &Config, args: StatsArgs, style: SavingsStyle) -> Result<ExitCode> {
    if args.activity {
        eyre::ensure!(
            args.scheduler && args.json,
            "usage: --activity requires --scheduler and --json"
        );
        super::cache::print_json(&crate::scheduler::scheduler_activity_report(config))?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut report = crate::stats::collect(&config.store_dir(), &config.target.root)?;
    if args.scheduler {
        report.scheduler = Some(crate::scheduler::scheduler_report(config));
    }
    if args.json {
        super::cache::print_json(&report)?;
    } else {
        println!("{}", report.text(style == SavingsStyle::Quips));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_mode_requires_scheduler_json_and_skips_store_collection() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::for_test(directory.path());
        let objects = config.store_dir().join("cas/v1");
        std::fs::create_dir_all(objects.parent().unwrap()).unwrap();
        std::fs::write(&objects, b"not a directory").unwrap();
        assert!(
            run(
                &config,
                StatsArgs {
                    json: true,
                    scheduler: false,
                    activity: false,
                },
                SavingsStyle::Plain,
            )
            .is_err(),
            "ordinary statistics must reject the invalid store fixture"
        );
        for (json, scheduler) in [(false, false), (false, true), (true, false)] {
            let error = run(
                &config,
                StatsArgs {
                    json,
                    scheduler,
                    activity: true,
                },
                SavingsStyle::Plain,
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "usage: --activity requires --scheduler and --json"
            );
        }
        assert_eq!(
            run(
                &config,
                StatsArgs {
                    json: true,
                    scheduler: true,
                    activity: true,
                },
                SavingsStyle::Plain,
            )
            .unwrap(),
            ExitCode::SUCCESS,
            "activity inspection must not collect the invalid store"
        );
    }
}
