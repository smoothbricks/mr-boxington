//! A signal sent to a whole process group must not release a reservation
//! while the reserved command is still stopping.

#[cfg(unix)]
mod support;

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use super::support;

    /// Traps the signals, announces it is stopping, and only exits once told.
    const SLOW_STOP: &str = r#"
trap 'touch "$1/stopping"; while [ ! -e "$1/finish" ]; do sleep 0.05; done; exit 0' HUP INT QUIT TERM
touch "$1/ready"
while :; do sleep 0.05; done
"#;

    fn mbx(root: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mbx"));
        command.env_clear();
        for name in ["PATH", "TMPDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        support::isolate_host(&mut command);
        command
            .current_dir(root)
            .env("MBX_CACHE_DIR", root.join("cache"))
            .env("MBX_SCHEDULER", "1")
            .env("MBX_SCHEDULER_CPUS", "2")
            .env("MBX_SCHEDULER_MEMORY", "none")
            .env("MBX_SCHEDULER_PRESSURE", "0")
            .env("MBX_EVENTS", "1")
            .env("MBX_GC_AUTO", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// A child in its own process group. Dropping it kills every process left
    /// in the group, including a worker and command that outlive the invoker,
    /// and reaps the invoker, so a failed assertion leaves nothing running.
    struct Group(Child);

    impl Group {
        fn spawn(command: &mut Command) -> Self {
            Self(command.process_group(0).spawn().unwrap())
        }

        fn signal(&self, signal: i32) {
            unsafe { libc::kill(-(self.0.id() as i32), signal) };
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            self.signal(libc::SIGKILL);
            let _ = self.0.wait();
        }
    }

    impl std::ops::Deref for Group {
        type Target = Child;
        fn deref(&self) -> &Child {
            &self.0
        }
    }

    impl std::ops::DerefMut for Group {
        fn deref_mut(&mut self) -> &mut Child {
            &mut self.0
        }
    }

    /// Receive the named probe's actual queue and grant decisions. Keeping the
    /// FIFO open lets separate appenders close between records without an EOF.
    fn probe_events(root: &Path) -> (std::sync::mpsc::Receiver<String>, fs::File) {
        use std::io::{BufRead, BufReader};
        let dir = root.join("cache/scheduler");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.jsonl");
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let (send, receive) = std::sync::mpsc::channel();
        let reader_path = path.clone();
        thread::spawn(move || {
            let reader = BufReader::new(fs::File::open(reader_path).unwrap());
            for line in reader.lines() {
                let event: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
                if event["name"] == "probe"
                    && let Some(kind @ ("enqueued" | "admitted")) = event["event"].as_str()
                    && send.send(kind.to_owned()).is_err()
                {
                    break;
                }
            }
        });
        let writer = fs::OpenOptions::new().write(true).open(path).unwrap();
        (receive, writer)
    }

    #[test]
    fn group_signals_keep_the_lease_until_the_command_exits() {
        for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let (events, _writer) = probe_events(root);
            let mut job = Group::spawn(
                mbx(root)
                    .args(["reserve", "--cpus", "2", "--", "sh", "-c", SLOW_STOP, "sh"])
                    .arg(root),
            );
            support::wait_for_path(&root.join("ready"));

            job.signal(signal);
            let status = job.wait().unwrap();
            assert_eq!(
                status.signal(),
                Some(signal),
                "signal {signal}: the invoker ends by the signal, got {status}"
            );
            support::wait_for_path(&root.join("stopping"));
            let mut probe = Group::spawn(
                mbx(root).args(["reserve", "--name", "probe", "--cpus", "2", "--", "true"]),
            );
            assert_eq!(
                events.recv_timeout(Duration::from_secs(10)).unwrap(),
                "enqueued"
            );
            assert_eq!(
                events.recv_timeout(Duration::from_millis(500)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout),
                "signal {signal}: the lease was released while the command was stopping"
            );

            fs::write(root.join("finish"), "").unwrap();
            assert_eq!(
                events.recv_timeout(Duration::from_secs(10)).unwrap(),
                "admitted"
            );
            assert!(probe.wait().unwrap().success());
        }
    }

    #[test]
    fn a_signal_the_command_sends_itself_still_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let status = mbx(dir.path())
            .args(["reserve", "--cpus", "1", "--", "sh", "-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(143));
    }

    #[test]
    fn an_ignored_signal_stays_ignored_in_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // `nohup`-style: HUP is ignored before mbx starts, so it must stay
        // ignored in the command.
        let status = Command::new("sh")
            .arg("-c")
            .arg("trap '' HUP; exec \"$@\"")
            .arg("sh")
            .arg(env!("CARGO_BIN_EXE_mbx"))
            .args(["reserve", "--cpus", "1", "--", "sh", "-c"])
            .arg("kill -HUP $$; touch \"$0/survived\"")
            .arg(root)
            .env_clear()
            .envs(
                mbx(root)
                    .get_envs()
                    .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned()))),
            )
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "got {status}");
        assert!(root.join("survived").exists());
    }

    /// An inherited-ignored HUP can queue while the launch check has it
    /// blocked; that must not be mistaken for a cancellation. HUP is sent to
    /// the group continuously while reservations start.
    #[test]
    fn an_ignored_signal_during_startup_does_not_cancel_the_launch() {
        for round in 0..15 {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let marker = root.join("ran");
            let mut program = mbx(root);
            program
                .args(["reserve", "--cpus", "1", "--", "touch"])
                .arg(&marker);
            // Ignored from the first instruction, as under `nohup`.
            unsafe {
                program.pre_exec(|| {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                    Ok(())
                });
            }
            let mut job = Group::spawn(&mut program);
            let group = job.id() as i32;
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let sender = {
                let done = done.clone();
                thread::spawn(move || {
                    while !done.load(std::sync::atomic::Ordering::SeqCst) {
                        unsafe { libc::kill(-group, libc::SIGHUP) };
                        thread::sleep(Duration::from_micros(200));
                    }
                })
            };
            let status = job.wait().unwrap();
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            sender.join().unwrap();
            assert!(status.success(), "round {round}: got {status}");
            assert!(marker.exists(), "round {round}: the command never ran");
        }
    }

    #[test]
    fn a_signal_while_waiting_for_capacity_ends_at_once_without_running() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (events, _writer) = probe_events(root);
        let holder = Group::spawn(
            mbx(root)
                .args(["reserve", "--cpus", "2", "--", "sh", "-c", SLOW_STOP, "sh"])
                .arg(root),
        );
        support::wait_for_path(&root.join("ready"));

        let mut waiter = Group::spawn(
            mbx(root)
                .args(["reserve", "--name", "probe", "--cpus", "2", "--", "touch"])
                .arg(root.join("ran")),
        );
        assert_eq!(
            events.recv_timeout(Duration::from_secs(10)).unwrap(),
            "enqueued"
        );
        waiter.signal(libc::SIGTERM);
        let status = waiter.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "got {status}");
        // The waiter's own worker must be gone too, or it would run the
        // command once the holder lets go.
        drop(holder);
        thread::sleep(Duration::from_millis(500));
        assert!(!root.join("ran").exists());
    }
}
