//! How fast a background collector unlinks files.
//!
//! Every unlink is a filesystem notification: an FSEvents record that
//! fseventsd holds on macOS, an inotify or fanotify event on Linux. One hourly
//! sweep of a shared cache removed 34 target directories, about 700,000
//! files, at 2,892 unlinks a second, and fseventsd grew by 1-2 MB a second
//! for as long as it ran. A collector nobody waits on arms [`pace`] and keeps
//! its unlinks to [`UNLINKS_PER_SECOND`]; every other process, a build or an
//! explicit `mbx gc`, unlinks at full speed because nothing arms it there.

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Unlinks a paced process issues per second, after a first second's burst.
///
/// A third of the rate measured unpaced, and five times the hourly sweep's
/// average load of about 700,000 files an hour: that sweep takes about twelve
/// minutes of its hour.
pub const UNLINKS_PER_SECOND: u32 = 1_000;

const INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / UNLINKS_PER_SECOND as u64);

/// How far ahead of now the pace may run: a second's worth of unlinks goes
/// at once, so a sweep that removes a few files never waits.
const BURST: Duration = Duration::from_secs(1);

/// The pace while any guard holds it: how many do, and the moment the next
/// unlink is due. One unlink moves that on by [`INTERVAL`]; an unlink due
/// more than [`BURST`] from now waits for the difference.
struct Armed {
    guards: usize,
    due: Instant,
}

static ARMED: Mutex<Option<Armed>> = Mutex::new(None);

fn armed() -> MutexGuard<'static, Option<Armed>> {
    ARMED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// This process's unlinks keep to the pace until the last guard is dropped.
#[must_use = "the pace ends when the guard is dropped"]
pub struct Paced(());

/// Pace every unlink this process makes through this module.
pub fn pace() -> Paced {
    let mut armed = armed();
    if let Some(held) = armed.as_mut() {
        held.guards += 1;
    } else {
        *armed = Some(Armed {
            guards: 1,
            due: Instant::now(),
        });
    }
    Paced(())
}

impl Drop for Paced {
    fn drop(&mut self) {
        let mut armed = armed();
        if let Some(held) = armed.as_mut()
            && held.guards > 1
        {
            held.guards -= 1;
        } else {
            *armed = None;
        }
    }
}

/// Wait until one more unlink keeps to the pace; at once when none is armed.
///
/// The lock is held while sleeping, so threads unlinking together share one
/// pace rather than each keeping its own.
pub fn wait() {
    let mut armed = armed();
    let Some(Armed { due, .. }) = armed.as_mut() else {
        return;
    };
    let now = Instant::now();
    let slot = (*due).max(now);
    if let Some(early) = slot.checked_duration_since(now + BURST) {
        std::thread::sleep(early);
    }
    *due = slot + INTERVAL;
}

/// [`std::fs::remove_file`] at the pace.
pub fn remove_file(path: &Path) -> io::Result<()> {
    wait();
    std::fs::remove_file(path)
}

/// [`std::fs::remove_dir_all`] at the pace: a link at `path` goes as a link,
/// nothing below it is followed, even a link swapped in for a directory while
/// the removal runs, and an entry something else removed meanwhile is not an
/// error.
///
/// Windows removes the tree through std, unpaced: pacing it would need a
/// handle-relative walk of its own, and a paced collector there still keeps
/// every file the store removes one at a time to the pace.
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        tree::remove_dir_all(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::remove_dir_all(path)
    }
}

/// The walk std's removal makes on Unix, every entry opened or unlinked
/// relative to its directory's descriptor, with [`wait`] before each unlink.
#[cfg(unix)]
mod tree {
    use super::wait;
    use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::CStr;
    use std::os::fd::{AsFd as _, BorrowedFd};
    use std::path::Path;

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    pub(super) fn remove_dir_all(path: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(path)?.is_symlink() {
            return super::remove_file(path);
        }
        // Anything but a directory at the root is an error, as it is for std.
        let directory = rustix::fs::openat(CWD, path, DIRECTORY, Mode::empty())?;
        remove_entries(directory.as_fd())?;
        drop(directory);
        wait();
        match rustix::fs::unlinkat(CWD, path, AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove everything in `directory`.
    fn remove_entries(directory: BorrowedFd<'_>) -> Result<(), Errno> {
        for entry in Dir::read_from(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            match remove_entry(directory, name, entry.file_type()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Remove `name` in `directory`: a directory after everything in it,
    /// anything else as itself. A directory replaced by a link since it was
    /// listed fails to open as one and goes as the link.
    fn remove_entry(directory: BorrowedFd<'_>, name: &CStr, kind: FileType) -> Result<(), Errno> {
        if matches!(kind, FileType::Directory | FileType::Unknown) {
            match rustix::fs::openat(directory, name, DIRECTORY, Mode::empty()) {
                Ok(child) => {
                    remove_entries(child.as_fd())?;
                    drop(child);
                    wait();
                    return rustix::fs::unlinkat(directory, name, AtFlags::REMOVEDIR);
                }
                Err(Errno::NOTDIR | Errno::LOOP) => {}
                Err(error) => return Err(error),
            }
        }
        wait();
        rustix::fs::unlinkat(directory, name, AtFlags::empty())
    }
}

#[cfg(test)]
#[path = "unlink_pace_tests.rs"]
mod tests;
