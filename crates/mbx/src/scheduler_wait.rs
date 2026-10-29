//! Lease changes wake admission; timed rechecks recover silent process death.

use super::{LEASES_DIR, jittered};
use std::io;
use std::path::Path;
use std::time::Duration;

/// Recheck gates and dead-holder locks without millisecond filesystem polling.
const RECHECK: Duration = Duration::from_secs(1);

pub(super) struct Wait {
    watch: Option<Watch>,
}

impl Wait {
    pub(super) fn new(dir: &Path) -> Self {
        let watch = Watch::new(&dir.join(LEASES_DIR))
            .map_err(|error| {
                log::debug!("scheduler notifications unavailable; using timed rechecks: {error}")
            })
            .ok();
        Self { watch }
    }

    /// Whether this was a periodic recheck rather than a lease change.
    pub(super) fn pause(&mut self) -> bool {
        let timeout = jittered(RECHECK);
        if let Some(watch) = &mut self.watch {
            match watch.changed(timeout) {
                Ok(changed) => return !changed,
                Err(error) => {
                    log::debug!("scheduler notifications failed; using timed rechecks: {error}");
                    self.watch = None;
                }
            }
        }
        std::thread::sleep(timeout);
        true
    }
}

#[cfg(target_os = "macos")]
struct Watch {
    queue: std::os::fd::OwnedFd,
    _leases: std::fs::File,
}

#[cfg(target_os = "macos")]
impl Watch {
    fn new(path: &Path) -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let leases = std::fs::File::open(path)?;
        // SAFETY: kqueue creates a new descriptor, owned below on success.
        let fd = unsafe { libc::kqueue() };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is the newly created descriptor and has no other owner.
        let queue = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        // SAFETY: F_SETFD updates only the valid queue descriptor's flags.
        if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: usize::try_from(leases.as_raw_fd()).map_err(io::Error::other)?,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: change is one initialized registration; no event buffer or
        // timeout is needed. The watched file stays open for the watch's life.
        if unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            queue,
            _leases: leases,
        })
    }

    fn changed(&mut self, timeout: Duration) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let timeout = libc::timespec {
            tv_sec: timeout.as_secs().try_into().map_err(io::Error::other)?,
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        // SAFETY: the queue is open, event has space for the one result, and
        // timeout is initialized. kevent retains neither pointer.
        let count = unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                std::ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                &timeout,
            )
        };
        if count == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(false);
        }
        // SAFETY: a positive count initialized the sole event slot.
        let event = unsafe { event.assume_init() };
        if event.flags & libc::EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(
                event.data.try_into().map_err(io::Error::other)?,
            ));
        }
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
struct Watch(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl Watch {
    fn new(path: &Path) -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: inotify_init1 creates a new, nonblocking close-on-exec fd.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just created and has no other owner.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        // Watch directory entries, not opens/reads, so admission scans cannot
        // generate their own wakeups. Queue grants arrive by rename.
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        // SAFETY: path is NUL terminated and fd is an open inotify instance.
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }

    fn changed(&mut self, timeout: Duration) -> io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = timeout.as_millis().try_into().map_err(io::Error::other)?;
        // SAFETY: poll names one initialized entry, retained only for this call.
        let count = unsafe { libc::poll(&mut poll, 1, milliseconds) };
        if count == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(false);
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("inotify descriptor is no longer readable"));
        }
        let mut events = [0_u8; 4096];
        loop {
            // SAFETY: events is a writable buffer of the declared length.
            // Event contents are irrelevant: overflow also requires a scan.
            let count =
                unsafe { libc::read(self.0.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) };
            if count >= 0 {
                if count == 0 {
                    return Ok(true);
                }
                continue;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => return Ok(true),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(error),
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
struct Watch;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl Watch {
    fn new(_: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "lease notifications are not supported on this platform",
        ))
    }

    fn changed(&mut self, _: Duration) -> io::Result<bool> {
        unreachable!("unsupported platforms never construct a watch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn changes_between_registration_and_wait_are_not_lost() {
        let directory = tempfile::tempdir().unwrap();
        let mut watch = Watch::new(directory.path()).unwrap();
        std::fs::write(directory.path().join("lease"), b"held").unwrap();
        assert!(
            watch.changed(Duration::from_millis(100)).unwrap(),
            "an earlier change must already be pending"
        );
        assert!(
            !watch.changed(Duration::from_millis(20)).unwrap(),
            "the notification must be drained"
        );
        std::fs::remove_file(directory.path().join("lease")).unwrap();
        assert!(
            watch.changed(Duration::from_millis(100)).unwrap(),
            "removing a lease wakes the watcher"
        );
    }

    #[test]
    fn unavailable_notifications_use_a_bounded_sleep() {
        let mut wait = Wait { watch: None };
        let started = std::time::Instant::now();
        assert!(wait.pause(), "without a watch the wake is periodic");
        assert!(started.elapsed() >= RECHECK, "fallback must not spin");
        assert!(
            started.elapsed() < RECHECK * 3,
            "fallback must stay bounded"
        );
    }
}
