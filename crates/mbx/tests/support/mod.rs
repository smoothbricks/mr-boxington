//! Keep the developer's own mbx setup out of the integration tests.

use std::process::Command;

/// Hide the developer's own mbx setup from the child: the settings it reads
/// from the environment, and on Unix the directories it reads a global
/// `config.toml` from. `test/test_helper/common_setup.bash` does the same for
/// the Bats suites. Cargo and rustup keep their real homes so the toolchain
/// still resolves.
///
/// Every test names the settings it needs after this, so a scrubbed variable
/// that a test cares about comes back with the test's own value.
pub fn isolate_host(command: &mut Command) -> &mut Command {
    scrub_settings(command);
    #[cfg(unix)]
    {
        let real_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        for (name, default) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
            let value = std::env::var_os(name)
                .map(std::path::PathBuf::from)
                .or_else(|| real_home.as_ref().map(|home| home.join(default)));
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        // Keep the host or sandbox's short session root (such as cowshed's
        // runtime alias beside a long TMPDIR) so session sockets stay within
        // SUN_LEN, including after `Command::env_clear`.
        if let Some(root) = std::env::var_os("MBX_SESSION_ROOT").filter(|root| !root.is_empty()) {
            command.env("MBX_SESSION_ROOT", root);
        }
        let home = isolated_home();
        command
            .env("HOME", home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"));
    }
    command
}

/// A setting in the environment outranks the same setting in a configuration
/// file, so hiding the file is not enough on its own: an exported
/// `MBX_TARGET_ROOT` moves managed targets out of a test's store exactly as
/// `[target] root` does. Drop the whole namespace rather than name the
/// settings that bite today, since the next one to bite would arrive silently.
///
/// `MBX_LOG` stays. It only raises mbx's own diagnostics, and running one of
/// these tests under it is how a developer sees what mbx did. The extra stderr
/// it produces can fail a test that asserts on stderr, which is a deliberate
/// act rather than something the machine decides.
///
/// `MBX_SESSION_ROOT` stays too. It is a host/sandbox runtime-root alias paired
/// with a long `TMPDIR` so session Unix sockets fit inside `SUN_LEN`, not a
/// build or cache setting. Tests that assert on `MBX_SESSION_ROOT` itself
/// override or remove it explicitly.
///
/// This applies on every platform. Windows resolves the configuration
/// directory through known-folder APIs rather than the environment, so a
/// global `config.toml` on a Windows developer's machine is still visible
/// here; isolating it needs a way to point mbx at a different file.
fn scrub_settings(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy().to_ascii_uppercase();
        if key.starts_with("MBX_") && key != "MBX_LOG" && key != "MBX_SESSION_ROOT" {
            command.env_remove(&name);
        }
    }
}

/// Shared by every test in the run, as the real home was. No test writes mbx
/// configuration, so a global configuration file never exists here.
#[cfg(unix)]
fn isolated_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("isolated-home");
        std::fs::create_dir_all(&home).unwrap();
        home
    })
}

/// Nearest existing directory ancestor of `path`.
#[allow(dead_code)]
pub fn existing_ancestor_dir(path: &std::path::Path) -> std::path::PathBuf {
    let mut current = path.parent().unwrap_or(path);
    while !current.is_dir() {
        current = current
            .parent()
            .expect("path must have an existing ancestor directory");
    }
    current.to_path_buf()
}

#[allow(dead_code)]
fn watch_target(path: &std::path::Path, watch_self: bool) -> std::path::PathBuf {
    if watch_self && path.exists() {
        path.to_path_buf()
    } else {
        existing_ancestor_dir(path)
    }
}

#[allow(dead_code)]
fn watch_until(path: &std::path::Path, watch_self: bool, ready: impl Fn() -> bool) {
    use notify::{RecursiveMode, Watcher as _};

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |_| {
        let _ = tx.send(());
    })
    .expect("filesystem watcher should start");

    let mut watched: Option<std::path::PathBuf> = None;
    loop {
        let next = watch_target(path, watch_self);
        if watched.as_ref() != Some(&next) {
            if let Some(previous) = watched.take() {
                let _ = watcher.unwatch(&previous);
            }
            if let Err(error) = watcher.watch(&next, RecursiveMode::NonRecursive) {
                if !next.exists() {
                    continue;
                }
                panic!("watching {} failed: {error}", next.display());
            }
            watched = Some(next);
        }

        if ready() {
            return;
        }
        if watched.as_ref() != Some(&watch_target(path, watch_self)) {
            continue;
        }

        rx.recv().expect("filesystem watcher stopped unexpectedly");
    }
}

/// Block until `path` exists using `notify` on its deepest existing ancestor
/// directory, advancing into subdirectories as they appear. No fixed timeout.
#[allow(dead_code)]
pub fn wait_for_path(path: &std::path::Path) {
    watch_until(path, false, || path.exists());
}

/// Block until `dir` exists and contains at least one entry, using `notify`.
/// No fixed timeout.
#[allow(dead_code)]
pub fn wait_for_dir_entry(dir: &std::path::Path) {
    watch_until(dir, true, || {
        std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
    });
}

/// Block until `path` exists and its contents satisfy `predicate`, using
/// `notify` on `path` itself once it exists. No fixed timeout.
#[allow(dead_code)]
pub fn wait_for_file_content(path: &std::path::Path, predicate: impl Fn(&str) -> bool) {
    watch_until(path, true, || {
        std::fs::read_to_string(path).is_ok_and(|text| predicate(&text))
    });
}

/// Create a named pipe at `path` so a wrapper script can block on
/// `read _ < "$FIFO"` until the test releases it by writing a line.
#[cfg(unix)]
#[allow(dead_code)]
pub fn create_fifo(path: &std::path::Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
    // SAFETY: c_path is a valid NUL-terminated path passed to mkfifo(3).
    assert_eq!(
        unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) },
        0,
        "mkfifo({}) failed: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

#[cfg(unix)]
#[allow(dead_code)]
enum WatchEvent {
    FsChanged,
    Exited(std::io::Result<std::process::Output>),
}

/// Child process in its own process group whose exit is delivered on a shared
/// `mpsc` channel by one blocking waiter thread, letting `wait_for_path_or_exit`
/// wait on filesystem notifications and child exit together without polling.
#[cfg(unix)]
#[allow(dead_code)]
pub struct WatchedChild {
    group: libc::pid_t,
    tx: std::sync::mpsc::Sender<WatchEvent>,
    rx: std::sync::mpsc::Receiver<WatchEvent>,
    waiter: Option<std::thread::JoinHandle<()>>,
    output: Option<std::io::Result<std::process::Output>>,
}

#[cfg(unix)]
#[allow(dead_code)]
impl WatchedChild {
    pub fn spawn_group(command: &mut Command) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt as _;
        let child = command.process_group(0).spawn()?;
        let group = child.id() as libc::pid_t;
        let (tx, rx) = std::sync::mpsc::channel();
        let exit_tx = tx.clone();
        let waiter = std::thread::spawn(move || {
            let _ = exit_tx.send(WatchEvent::Exited(child.wait_with_output()));
        });
        Ok(Self {
            group,
            tx,
            rx,
            waiter: Some(waiter),
            output: None,
        })
    }

    pub fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        if let Some(output) = self.output.take() {
            if let Some(waiter) = self.waiter.take() {
                let _ = waiter.join();
            }
            return output;
        }
        loop {
            match self.rx.recv().expect("waiter thread exited without result") {
                WatchEvent::FsChanged => {}
                WatchEvent::Exited(output) => {
                    if let Some(waiter) = self.waiter.take() {
                        let _ = waiter.join();
                    }
                    return output;
                }
            }
        }
    }
}

#[cfg(unix)]
impl Drop for WatchedChild {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            if self.output.is_none() && !waiter.is_finished() {
                // SAFETY: spawn_group created a distinct process group whose id
                // is the child's pid, and the child is reaped by `waiter`.
                unsafe { libc::kill(-self.group, libc::SIGKILL) };
            }
            let _ = waiter.join();
        }
    }
}

#[cfg(unix)]
#[allow(dead_code)]
fn watch_until_or_exit(
    path: &std::path::Path,
    watch_self: bool,
    child: &mut WatchedChild,
    ready: impl Fn() -> bool,
    early_exit_message: impl FnOnce() -> String,
) {
    use notify::{RecursiveMode, Watcher as _};

    let fs_tx = child.tx.clone();
    let mut watcher = notify::recommended_watcher(move |_| {
        let _ = fs_tx.send(WatchEvent::FsChanged);
    })
    .expect("filesystem watcher should start");

    let mut watched: Option<std::path::PathBuf> = None;
    loop {
        let next = watch_target(path, watch_self);
        if watched.as_ref() != Some(&next) {
            if let Some(previous) = watched.take() {
                let _ = watcher.unwatch(&previous);
            }
            if let Err(error) = watcher.watch(&next, RecursiveMode::NonRecursive) {
                if !next.exists() {
                    continue;
                }
                panic!("watching {} failed: {error}", next.display());
            }
            watched = Some(next);
        }

        if ready() {
            return;
        }
        if watched.as_ref() != Some(&watch_target(path, watch_self)) {
            continue;
        }
        if child.output.is_some() {
            panic!("{}", early_exit_message());
        }

        match child
            .rx
            .recv()
            .expect("waiter thread exited without result")
        {
            WatchEvent::FsChanged => {}
            WatchEvent::Exited(output) => {
                child.output = Some(output);
                if ready() {
                    return;
                }
                panic!("{}", early_exit_message());
            }
        }
    }
}

/// Block until `path` exists or `child` exits, using `notify` on the deepest
/// existing ancestor directory and the child's exit thread on the same `mpsc`
/// channel. No polling and no fixed timeout.
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_for_path_or_exit(
    path: &std::path::Path,
    child: &mut WatchedChild,
    early_exit_message: &str,
) {
    watch_until_or_exit(
        path,
        false,
        child,
        || path.exists(),
        || early_exit_message.to_owned(),
    );
}

/// Block until `path` exists and its contents satisfy `predicate`, or `child`
/// exits early. No polling and no fixed timeout.
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_for_file_content_or_exit(
    path: &std::path::Path,
    child: &mut WatchedChild,
    predicate: impl Fn(&str) -> bool,
    early_exit_message: impl FnOnce() -> String,
) {
    watch_until_or_exit(
        path,
        true,
        child,
        || std::fs::read_to_string(path).is_ok_and(|text| predicate(&text)),
        early_exit_message,
    );
}
