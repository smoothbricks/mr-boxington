//! Hold machine-wide scheduler capacity while an external command runs.

use super::cargo::exit_code;
use crate::config::{Config, SchedulerPriority};
use crate::scheduler::grant::Descriptor;
use crate::scheduler::{Pool, SCHED_DISABLE_ENV};
use bytesize::ByteSize;
use eyre::{Result, WrapErr};
use std::ffi::OsStr;
use std::process::{Command, ExitCode};
use std::sync::atomic::AtomicBool;

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
    /// The class this reservation's run time is remembered and predicted by,
    /// such as `nx:<workspace>:<task>`: within a priority, the pool admits
    /// the requests predicted shortest first. Without one, the reservation is
    /// named `mbx reserve` and predicted by the median of the named classes.
    #[usage(long, value_name = "CLASS")]
    name: Option<String>,
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
    if let Some(name) = &args.name {
        worker.args(["--name", name]);
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
    // This process waits for its own command's grant, so nothing withdraws it:
    // it ends granted, or with the process.
    let Some((mut permit, admission)) = pool.reserve_observed(
        args.cpus,
        args.memory.map(|memory| memory.as_u64()),
        args.name.as_deref(),
        &AtomicBool::new(false),
    )?
    else {
        unreachable!("nothing withdraws a reservation its own command waits on")
    };
    // A terminal's ^C or hangup, or a signal sent to the whole process group,
    // reaches this worker as well as the command. The command handles it
    // itself, so the worker only has to outlive it; dying first would release
    // the lease while the command is still shutting down.
    let guard = HoldThroughSignals::install();
    if let Some(signal) = guard.pending() {
        // Signalled after admission and before the command exists: there is
        // nothing to wait for, so end the way the signal would have.
        drop(permit);
        guard.end_by(signal);
    }
    if grant.is_none() {
        // Without --grant-fd, an inherited grant descriptor hears of this hold.
        permit.announce();
    }
    // The hold on --grant-fd ends, announcing its release, once the command has.
    let hold = grant.map(Descriptor::granted).transpose()?;
    let mut command = external_command(program, arguments, args.cpus.map(|_| admission.cpus()));
    // A signal during startup reaches the new process too, so it checks for one
    // itself before exec and refuses to start the command.
    guard.refuse_start_if_signalled(&mut command);
    let status = match command.spawn() {
        Ok(mut child) => child.wait().map_err(eyre::Report::from),
        Err(error) => match guard.pending() {
            Some(signal) if error.kind() == std::io::ErrorKind::Interrupted => {
                drop(permit);
                guard.end_by(signal);
            }
            _ => Err(eyre::Report::from(error)),
        },
    }
    .wrap_err_with(|| format!("failed to run {program}"));
    drop(hold);
    drop(guard);
    drop(permit);
    pool.observe_reservation(args.name.as_deref(), admission, status.as_ref().ok());
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
    // Its share of the enclosing grant is at most what it asked for.
    let cpus = args.cpus.map(|cpus| {
        let enclosing = std::env::var(GRANTED_CPUS_ENV)
            .or_else(|_| std::env::var("MBX_RESERVED_CPUS"))
            .ok()
            .and_then(|count| count.parse::<u64>().ok());
        enclosing.map_or(cpus, |enclosing| enclosing.min(cpus))
    });
    let status = external_command(program, arguments, cpus)
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

pub(super) fn external_command(program: &str, arguments: &[String], cpus: Option<u64>) -> Command {
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
    // Tell the command how many cores it holds so a runner can size its own
    // workers from the lease. A reservation that holds no cores clears the
    // variable rather than passing on an enclosing reservation's value.
    match cpus.filter(|cpus| *cpus > 0) {
        Some(cpus) => {
            let width = cpus.to_string();
            command.env(GRANTED_CPUS_ENV, &width).env("MBX_RESERVED_CPUS", width)
        }
        None => command.env_remove(GRANTED_CPUS_ENV).env_remove("MBX_RESERVED_CPUS"),
    };
    command
}

/// Keeps the worker alive through HUP, INT, QUIT and TERM while the command
/// runs, so the lease it holds lasts until the command has exited.
///
/// The handler only records the signal. It is a handler and not `SIG_IGN`
/// because exec resets handlers, so the command keeps default dispositions,
/// and nothing is blocked in the worker, because a blocked mask would be
/// inherited. A signal
/// that was already ignored (`nohup`, a background job's INT) is left alone so
/// it stays ignored in the command too.
#[cfg(unix)]
struct HoldThroughSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
static RECEIVED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn record_signal(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
impl HoldThroughSignals {
    const SIGNALS: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

    fn install() -> Self {
        RECEIVED_SIGNAL.store(0, std::sync::atomic::Ordering::SeqCst);
        let mut previous = Vec::new();
        for signal in Self::SIGNALS {
            // SAFETY: plain sigaction calls on zeroed structs; the handler
            // only stores to an atomic, which is async-signal-safe.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, std::ptr::null(), &mut old) != 0
                    || old.sa_sigaction == libc::SIG_IGN
                {
                    continue;
                }
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction =
                    record_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut new.sa_mask);
                if libc::sigaction(signal, &new, std::ptr::null_mut()) == 0 {
                    previous.push((signal, old));
                }
            }
        }
        Self { previous }
    }

    fn set_of(signals: impl Iterator<Item = libc::c_int>) -> libc::sigset_t {
        // SAFETY: builds a signal set in zeroed storage.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in signals {
                libc::sigaddset(&mut set, signal);
            }
            set
        }
    }

    /// A signal the handler has recorded.
    fn pending(&self) -> Option<libc::c_int> {
        match RECEIVED_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// Make the new process, between fork and exec, check for a signal that
    /// arrived while it was being created and fail the launch if there was one.
    ///
    /// A group signal sent after the fork reaches the new process, which still
    /// has the recording handler. It blocks the signals, looks at both what the
    /// handler recorded and what is pending, and only then puts back the
    /// original dispositions and mask, so a signal that lands afterwards takes
    /// the command's own default action. The worker never replays a signal, so
    /// the command cannot receive one twice.
    fn refuse_start_if_signalled(&self, command: &mut Command) {
        use std::os::unix::process::CommandExt as _;
        let previous = self.previous.clone();
        // Only signals the guard took over. One that was inherited as ignored
        // can still queue while blocked, and is not a cancellation.
        let set = Self::set_of(previous.iter().map(|(signal, _)| *signal));
        // SAFETY: the closure runs between fork and exec and only calls
        // async-signal-safe functions on data it already owns.
        unsafe {
            command.pre_exec(move || {
                let mut original: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut original);
                let mut pending: libc::sigset_t = std::mem::zeroed();
                libc::sigpending(&mut pending);
                let arrived = RECEIVED_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) != 0
                    || previous
                        .iter()
                        .any(|(signal, _)| libc::sigismember(&pending, *signal) == 1);
                if arrived {
                    // A raw OS error, because only that number crosses back to the
                    // parent; a bare error kind would arrive as EINVAL.
                    return Err(std::io::Error::from_raw_os_error(libc::EINTR));
                }
                for (signal, old) in &previous {
                    libc::sigaction(*signal, old, std::ptr::null_mut());
                }
                libc::pthread_sigmask(libc::SIG_SETMASK, &original, std::ptr::null_mut());
                Ok(())
            });
        }
    }

    fn restore(&mut self) {
        for (signal, old) in self.previous.drain(..) {
            // SAFETY: reinstalls the disposition install replaced.
            unsafe { libc::sigaction(signal, &old, std::ptr::null_mut()) };
        }
    }

    /// End the process by `signal` with its default action.
    fn end_by(mut self, signal: libc::c_int) -> ! {
        self.restore();
        // SAFETY: resets one signal to its default and raises it. Unblocking
        // covers a signal that was blocked when mbx started.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(signal);
        }
        std::process::exit(128 + signal)
    }
}

#[cfg(unix)]
impl Drop for HoldThroughSignals {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(not(unix))]
struct HoldThroughSignals;

#[cfg(not(unix))]
impl Drop for HoldThroughSignals {
    /// Matches the Unix guard, whose drop restores the signal dispositions.
    fn drop(&mut self) {}
}

#[cfg(not(unix))]
impl HoldThroughSignals {
    fn install() -> Self {
        Self
    }

    fn refuse_start_if_signalled(&self, _command: &mut Command) {}

    fn pending(&self) -> Option<i32> {
        None
    }

    fn end_by(self, _signal: i32) -> ! {
        unreachable!("no signal is ever pending")
    }
}
