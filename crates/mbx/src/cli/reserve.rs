//! Hold machine-wide scheduler capacity while an external command runs.

use super::cargo::exit_code;
use crate::config::{Config, SchedulerPriority};
use crate::scheduler::grant::Descriptor;
use crate::scheduler::{Pool, SCHED_DISABLE_ENV};
use bytesize::ByteSize;
use eyre::{Result, WrapErr};
use std::ffi::OsStr;
use std::process::{Command, ExitCode};

/// Marks the worker process that holds one reservation's lease.
const WORKER_ENV: &str = "MBX_RESERVE_WORKER";

#[derive(usage_rs::Args)]
pub(super) struct ReserveArgs {
    /// Most CPU cores to hold while the command runs. The pool may grant
    /// fewer, at least one, as soon as one is free; the command reads the
    /// count it was granted from `MBX_RESERVE_CPUS`.
    #[usage(long, value_name = "N")]
    cpus: Option<u64>,
    /// Memory to hold from the configured scheduler budget.
    #[usage(long, value_name = "SIZE")]
    memory: Option<ByteSize>,
    /// Priority in the machine-wide pool: `low` goes after waiting
    /// normal-priority work.
    #[usage(long, default = "normal")]
    priority: SchedulerPriority,
    /// Descriptor to announce the reservation on: one byte, `g`, once
    /// capacity is granted, and `r` once it is released after the command.
    ///
    /// mbx closes the descriptor after `r`, and the command never inherits
    /// it, so a reader sees end of file after `r`, or with no `g` when mbx
    /// exited before capacity was granted. The command starts only once `g`
    /// is written. A reservation nested in another is granted at once. For
    /// this reservation, the descriptor takes the place of an inherited
    /// `MBX_GRANT_FD`.
    #[usage(long, value_name = "FD")]
    grant_fd: Option<i32>,
    /// Command and arguments to run while capacity is reserved.
    #[usage(value_name = "COMMAND", required = true, double_dash = "automatic")]
    pub(super) command: Vec<String>,
}

/// Where one `mbx reserve` gets its capacity.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Start the worker that holds the lease for the command's life.
    Spawn,
    /// This process is that worker: reserve, then run the command.
    Reserve,
    /// An enclosing reservation already covers this process tree: run the
    /// command under that grant. Reserving again would queue behind the
    /// capacity the enclosing reservation itself holds, and wait forever once
    /// that is the whole pool.
    Enclosed,
}

/// The route the inherited environment selects: whether an enclosing
/// reservation covers this process, and whether it is a reservation's worker.
pub(super) fn route(enclosing: Option<&OsStr>, worker: Option<&OsStr>) -> Route {
    match (enclosing, worker) {
        (Some(_), _) => Route::Enclosed,
        (None, Some(_)) => Route::Reserve,
        (None, None) => Route::Spawn,
    }
}

pub(super) fn run(config: &Config, args: ReserveArgs) -> Result<ExitCode> {
    // Refused before any capacity is reserved, whichever route runs.
    let grant = args.grant_fd.map(Descriptor::claim).transpose()?;
    match route(
        std::env::var_os(SCHED_DISABLE_ENV).as_deref(),
        std::env::var_os(WORKER_ENV).as_deref(),
    ) {
        Route::Spawn => spawn_worker(&args, grant),
        Route::Reserve => run_direct(config, &args, grant),
        Route::Enclosed => run_enclosed(config, &args, grant),
    }
}

fn spawn_worker(args: &ReserveArgs, grant: Option<Descriptor>) -> Result<ExitCode> {
    let mut worker = Command::new(std::env::current_exe()?);
    worker.arg("reserve");
    if let Some(cpus) = args.cpus {
        worker.args(["--cpus", &cpus.to_string()]);
    }
    if let Some(memory) = args.memory {
        worker.args(["--memory", &format!("{}B", memory.as_u64())]);
    }
    // The worker inherits the descriptor and announces on it.
    if let Some(fd) = args.grant_fd {
        worker.args(["--grant-fd", &fd.to_string()]);
    }
    worker.args(["--priority", args.priority.as_str(), "--"]);
    worker.args(&args.command);
    worker.env(WORKER_ENV, "1");
    let mut worker = worker.spawn().wrap_err("failed to start reserve worker")?;
    // Only the worker holds the descriptor now, so its reader sees end of
    // file once the worker exits.
    drop(grant);
    Ok(exit_code(
        worker
            .wait()
            .wrap_err("failed to wait for reserve worker")?,
    ))
}

fn run_direct(config: &Config, args: &ReserveArgs, grant: Option<Descriptor>) -> Result<ExitCode> {
    let (program, arguments, pool) = reservation(config, args)?;
    let (mut permit, admission) =
        pool.reserve_observed(args.cpus, args.memory.map(|memory| memory.as_u64()))?;
    if grant.is_none() {
        // Without --grant-fd, an inherited grant descriptor hears of this hold.
        permit.announce();
    }
    // The hold on --grant-fd ends, announcing its release, once the command has.
    let status = grant
        .map(Descriptor::granted)
        .transpose()
        .and_then(|_hold| {
            let mut command = external_command(program, arguments);
            // `--cpus` is a maximum: the pool grants as many as are free,
            // down to one, rather than hold the command until all are.
            if args.cpus.is_some() {
                command.env(GRANTED_CPUS_ENV, admission.cpus().to_string());
            }
            command
                .status()
                .wrap_err_with(|| format!("failed to run {program}"))
        });
    drop(permit);
    pool.observe_reservation(admission, status.as_ref().ok());
    let status = status?;
    Ok(exit_code(status))
}

fn run_enclosed(
    config: &Config,
    args: &ReserveArgs,
    grant: Option<Descriptor>,
) -> Result<ExitCode> {
    let (program, arguments, pool) = reservation(config, args)?;
    // The refusals of a reservation of its own: nesting never turns a
    // malformed request into a silently accepted one.
    pool.reservation_weight(args.cpus, args.memory.map(|memory| memory.as_u64()))?;
    log::debug!("an enclosing reservation covers {program}; it runs under that grant");
    // That grant covers the command already, so --grant-fd hears of it now,
    // and of its release once the command has run.
    let _hold = grant.map(Descriptor::granted).transpose()?;
    let mut command = external_command(program, arguments);
    // Its share of the enclosing grant is at most what it asked for.
    if let Some(cpus) = args.cpus {
        let enclosing = std::env::var(GRANTED_CPUS_ENV)
            .ok()
            .and_then(|count| count.parse::<u64>().ok());
        command.env(
            GRANTED_CPUS_ENV,
            enclosing
                .map_or(cpus, |enclosing| enclosing.min(cpus))
                .to_string(),
        );
    }
    let status = command
        .status()
        .wrap_err_with(|| format!("failed to run {program}"))?;
    Ok(exit_code(status))
}

/// The command one reservation runs and the pool it reserves from.
fn reservation<'a>(
    config: &Config,
    args: &'a ReserveArgs,
) -> Result<(&'a str, &'a [String], Pool)> {
    let Some((program, arguments)) = args.command.split_first() else {
        eyre::bail!("reserve needs a command to run")
    };
    let Some(pool) = crate::scheduler::reservation_pool(config, args.priority) else {
        eyre::bail!("reserve needs scheduler.enabled")
    };
    Ok((program, arguments, pool))
}

/// The cores a reservation's command was granted, at most its `--cpus`: the
/// command sizes its own parallelism from it.
const GRANTED_CPUS_ENV: &str = "MBX_RESERVE_CPUS";

pub(super) fn external_command(program: &str, arguments: &[String]) -> Command {
    let mut command = Command::new(program);
    command
        .args(arguments)
        // Nested mbx work is covered by this reservation. Letting its compiler
        // shims take new permits would deadlock when this lease fills the pool.
        .env(crate::scheduler::SCHED_DIR_ENV, "")
        .env(crate::scheduler::SCHED_DISABLE_ENV, "1")
        // The worker marker applies only to this reservation. A nested
        // `mbx reserve` sees the enclosing grant through the scheduler marker
        // above and runs under it.
        .env_remove(WORKER_ENV);
    command
}
