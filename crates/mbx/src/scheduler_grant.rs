//! Tell a supervisor when this process holds scheduler capacity.
//!
//! A grant descriptor hears one byte per event: `g` when a hold of capacity
//! begins and `r` when it is released. A supervisor that counts them knows
//! whether any process it started holds capacity or is still queued for it,
//! and can run a no-progress clock only while one holds it.
//!
//! `mbx reserve --grant-fd N` announces that one reservation on N. An
//! inherited `MBX_GRANT_FD=N` makes every admission mbx makes in the process
//! tree announce itself on N: compilers, each test binary or nextest test,
//! and a reservation without `--grant-fd`. A holder killed by a signal
//! writes no `r`.

use eyre::{Result, WrapErr};
use std::fs::File;
use std::io::Write;
use std::sync::OnceLock;

/// Names the descriptor every admission in a process tree announces on.
pub(crate) const GRANT_FD_ENV: &str = "MBX_GRANT_FD";

/// One byte on a grant descriptor.
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    /// `g`: a hold of scheduler capacity began.
    Granted = b'g',
    /// `r`: that hold was released.
    Released = b'r',
}

impl Event {
    fn write(self, mut file: &File) -> std::io::Result<()> {
        file.write_all(&[self as u8])
    }
}

/// The descriptor `--grant-fd` names, owned by this process once claimed.
pub(crate) struct Descriptor {
    fd: i32,
    file: File,
}

impl Descriptor {
    /// Claim `--grant-fd`'s descriptor, refusing one that cannot carry the
    /// announcements before any capacity is reserved.
    pub(crate) fn claim(fd: i32) -> Result<Self> {
        Ok(Self {
            fd,
            file: own(fd, &format!("--grant-fd {fd}"))?,
        })
    }

    /// Announce the grant. mbx announces the release on this descriptor too,
    /// so from now on the command it starts must not inherit it.
    pub(crate) fn granted(self) -> Result<Hold> {
        #[cfg(unix)]
        keep_from_children(&self.file)
            .wrap_err_with(|| format!("failed to keep --grant-fd {} from the command", self.fd))?;
        Event::Granted
            .write(&self.file)
            .wrap_err_with(|| format!("failed to announce the grant on --grant-fd {}", self.fd))?;
        Ok(Hold(self))
    }
}

/// A `--grant-fd` that announced its grant. Dropping it announces the
/// release and closes the descriptor.
pub(crate) struct Hold(Descriptor);

impl Drop for Hold {
    fn drop(&mut self) {
        // The command has already run; a reader that went away does not
        // change how it ended.
        if let Err(error) = Event::Released.write(&self.0.file) {
            log::warn!(
                "failed to announce the release on --grant-fd {}: {error}",
                self.0.fd
            );
        }
    }
}

/// Announce `event` on the descriptor `MBX_GRANT_FD` names, when this process
/// inherited one. Whether it was written: a release is announced only for a
/// grant that was.
pub(crate) fn announce(event: Event) -> bool {
    // Never dropped, so the inherited descriptor stays open for the
    // process's children and every later admission.
    static INHERITED: OnceLock<Option<(i32, File)>> = OnceLock::new();
    let Some((fd, file)) = INHERITED.get_or_init(inherited) else {
        return false;
    };
    event
        .write(file)
        .inspect_err(|error| {
            log::debug!("{GRANT_FD_ENV}={fd}: failed to announce {event:?}: {error}");
        })
        .is_ok()
}

/// The descriptor `MBX_GRANT_FD` names, when it is set and can carry the
/// announcements. The variable asks for announcements only where they can be
/// made, so one that cannot is logged and otherwise ignored.
fn inherited() -> Option<(i32, File)> {
    let value = std::env::var_os(GRANT_FD_ENV)?;
    let Some(fd) = value.to_str().and_then(|value| value.parse().ok()) else {
        log::debug!("{GRANT_FD_ENV}={value:?} is not a descriptor number");
        return None;
    };
    own(fd, &format!("{GRANT_FD_ENV}={fd}"))
        .inspect_err(|error| log::debug!("{error:#}"))
        .ok()
        .map(|file| (fd, file))
}

/// Take ownership of `fd`, refusing a descriptor that cannot carry the
/// announcements: a standard stream, one not open for writing, or one mbx
/// opened itself rather than inherited. `named` spells it as it was given.
#[cfg(unix)]
fn own(fd: i32, named: &str) -> Result<File> {
    use std::os::fd::FromRawFd as _;
    if (0..=libc::STDERR_FILENO).contains(&fd) {
        eyre::bail!("{named} is standard input, output or error, which mbx and its commands keep");
    }
    let query = |command| {
        // SAFETY: F_GETFD and F_GETFL only read the descriptor's flags.
        match unsafe { libc::fcntl(fd, command) } {
            -1 => Err(std::io::Error::last_os_error())
                .wrap_err_with(|| format!("{named} is not an open file descriptor")),
            flags => Ok(flags),
        }
    };
    let descriptor = query(libc::F_GETFD)?;
    let status = query(libc::F_GETFL)?;
    // Exec closed every close-on-exec descriptor mbx's parent held, and std
    // opens all of mbx's own close-on-exec: one with the flag is mbx's own.
    if descriptor & libc::FD_CLOEXEC != 0 {
        eyre::bail!("{named} was not inherited: mbx opened that descriptor itself");
    }
    if status & libc::O_ACCMODE == libc::O_RDONLY {
        eyre::bail!("{named} is open only for reading");
    }
    // SAFETY: the descriptor is open and was inherited, so nothing else in
    // this process owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn own(_fd: i32, named: &str) -> Result<File> {
    eyre::bail!("{named}: inherited file descriptors are a Unix interface")
}

/// Mark the descriptor close-on-exec, so no command mbx starts inherits it.
#[cfg(unix)]
fn keep_from_children(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: F_SETFD only sets the descriptor's flags.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
