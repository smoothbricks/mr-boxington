#![cfg(unix)]

//! The persistent Cargo shim hands a command to the toolchain's Cargo, never
//! to another mbx Cargo shim that is earlier on PATH or named by `CARGO`.
//!
//! Nothing here builds a crate. Every command is one that all shims pass
//! straight through (`--version`, or any subcommand inside a session), run
//! against a toolchain Cargo that records the calls it receives.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MBX: &str = env!("CARGO_BIN_EXE_mbx");
const VERSION: &str = "cargo 0.0.0 (fixture toolchain)";
/// What `MBX_SOCKET` holds for a command that runs inside a build session. A
/// shim only asks whether it is set, and a command it passes through never
/// connects to it.
const ENCLOSING_SESSION: &str = "enclosing-session";
/// The loop guard's record, which a shim exports to the Cargo it runs: how many
/// shim entries led there, and the Cargo each of them selected, one per line.
const DEPTH: &str = "MBX_CARGO_SHIM_DEPTH";
const CHAIN: &str = "MBX_CARGO_SHIM_CHAIN";

/// A project's own Cargo wrapper. It knows nothing of mbx and runs whatever
/// `cargo` PATH names. It also stops itself, so that a guard that fails to end a
/// loop fails the test with a message instead of leaving a process chain that
/// never ends.
const PLAIN_WRAPPER: &str = r#"#!/bin/sh
runs=${WRAPPER_RUNS-0}
if [ "$runs" -ge 64 ]; then
  echo "wrapper: run $runs, and nothing stopped the loop" >&2
  exit 99
fi
WRAPPER_RUNS=$((runs + 1))
export WRAPPER_RUNS
exec cargo "$@"
"#;

/// The two shapes of mbx Cargo shim that can share one PATH.
#[derive(Clone, Copy, Debug)]
enum Shim {
    /// What `mbx setup` installs: a launcher beside its `mbx-target` sidecar.
    Setup,
    /// What the flake's `makeWrapper` writes: a shell wrapper whose header
    /// exports `MBX_CARGO_SHIM_MODE`, in a directory that holds other tools.
    Nix,
}

impl Shim {
    const BOTH: [Shim; 2] = [Shim::Setup, Shim::Nix];
    const ORDERS: [[Shim; 2]; 2] = [[Shim::Setup, Shim::Nix], [Shim::Nix, Shim::Setup]];

    fn other(self) -> Shim {
        match self {
            Shim::Setup => Shim::Nix,
            Shim::Nix => Shim::Setup,
        }
    }
}

/// One command started through a shim.
#[derive(Debug)]
struct Launch<'a> {
    /// The shim the command starts through, named by absolute path.
    entry: Shim,
    /// The shim directories that lead PATH, in order, ahead of the toolchain's.
    path: [Shim; 2],
    /// What `CARGO` names, when it is set.
    cargo: Option<&'a Path>,
    arguments: &'a [&'a str],
    /// Whether the command runs inside a session, as a build's subprocess does.
    session: bool,
}

/// One call the toolchain's Cargo received, as it recorded it.
#[derive(Debug)]
struct Call {
    /// The path the shim ran: `$0` of the Cargo.
    program: PathBuf,
    arguments: Vec<String>,
    /// Whether the tool beside the Nix shim was on the Cargo's PATH.
    peer_reachable: bool,
    environment: BTreeMap<String, String>,
}

impl Call {
    fn variable(&self, name: &str) -> Option<&str> {
        self.environment.get(name).map(String::as_str)
    }

    fn path(&self) -> Vec<PathBuf> {
        self.variable("PATH")
            .map(|path| std::env::split_paths(path).collect())
            .unwrap_or_default()
    }
}

/// What a command printed and the calls its toolchain Cargo recorded.
struct Run {
    launch: String,
    output: Output,
    log: String,
    calls: Vec<Call>,
}

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn report(&self) -> String {
        format!(
            "{}\nstatus: {}\nstdout:\n{}\nstderr:\n{}\ncalls to the toolchain's Cargo:\n{}",
            self.launch,
            self.output.status,
            self.stdout(),
            self.stderr(),
            self.log,
        )
    }

    fn succeeded(&self) {
        assert!(self.output.status.success(), "{}", self.report());
    }

    fn only_call(&self) -> &Call {
        assert_eq!(
            self.calls.len(),
            1,
            "the toolchain's Cargo should run once:\n{}",
            self.report()
        );
        &self.calls[0]
    }
}

/// Two mbx Cargo shims and the toolchain's Cargo behind them.
struct Toolchain {
    root: tempfile::TempDir,
    /// Where `mbx setup` put its launcher and `mbx-target` sidecar.
    setup_dir: PathBuf,
    /// The Nix shim, and an unrelated tool that lives in the same directory.
    nix_dir: PathBuf,
    /// The toolchain's Cargo, which records the calls it receives.
    real_dir: PathBuf,
    /// A plain Cargo wrapper in a directory that is on no PATH.
    wrapper: PathBuf,
    calls: PathBuf,
}

impl Toolchain {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();

        // The launcher and its sidecar come from the setup command itself.
        let setup = isolated(MBX, base).arg("setup").output().unwrap();
        let stdout = String::from_utf8_lossy(&setup.stdout).into_owned();
        assert!(
            setup.status.success(),
            "mbx setup failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&setup.stderr)
        );
        let installed = stdout
            .lines()
            .find_map(|line| line.strip_prefix("installed the Cargo shim at "))
            .unwrap_or_else(|| panic!("mbx setup did not say where it put the shim:\n{stdout}"));
        let setup_dir = Path::new(installed).parent().unwrap().to_path_buf();
        assert!(
            setup_dir.starts_with(base),
            "mbx setup wrote outside the fixture: {installed}"
        );
        assert!(setup_dir.join("mbx-target").is_file(), "{installed}");

        let nix_dir = base.join("nix/libexec/mbx");
        write_executable(&nix_dir.join("cargo"), &nix_wrapper(&nix_dir.join("cargo")));
        write_executable(&nix_dir.join("peer-tool"), "#!/bin/sh\necho peer\n");

        let calls = base.join("calls.log");
        let real_dir = base.join("toolchain/bin");
        write_executable(&real_dir.join("cargo"), &toolchain_cargo(&calls));

        let wrapper = base.join("project-wrapper/cargo");
        write_executable(&wrapper, PLAIN_WRAPPER);

        Self {
            root,
            setup_dir,
            nix_dir,
            real_dir,
            wrapper,
            calls,
        }
    }

    fn dir(&self, shim: Shim) -> &Path {
        match shim {
            Shim::Setup => self.setup_dir.as_path(),
            Shim::Nix => self.nix_dir.as_path(),
        }
    }

    fn shim(&self, shim: Shim) -> PathBuf {
        self.dir(shim).join("cargo")
    }

    fn real_cargo(&self) -> PathBuf {
        self.real_dir.join("cargo")
    }

    /// PATH with the given shim directories first and the toolchain's directory
    /// after them. The caller's own PATH follows, for `sh` and `dirname`.
    fn path(&self, leading: [Shim; 2]) -> OsString {
        let inherited = std::env::var_os("PATH").unwrap();
        std::env::join_paths(
            leading
                .into_iter()
                .map(|shim| self.dir(shim).to_path_buf())
                .chain([self.real_dir.clone()])
                .chain(std::env::split_paths(&inherited)),
        )
        .unwrap()
    }

    fn run(&self, launch: &Launch<'_>) -> Run {
        std::fs::write(&self.calls, "").unwrap();
        let mut command = isolated(self.shim(launch.entry), self.root.path());
        command
            .env("PATH", self.path(launch.path))
            .args(launch.arguments);
        if let Some(cargo) = launch.cargo {
            command.env("CARGO", cargo);
        }
        if launch.session {
            command.env("MBX_SOCKET", ENCLOSING_SESSION);
        }
        let output = command.output().unwrap();
        let log = std::fs::read_to_string(&self.calls).unwrap();
        Run {
            launch: format!("{launch:?}"),
            calls: parse_calls(&log),
            output,
            log,
        }
    }

    /// `call` is the toolchain's own Cargo, reached through exactly one shim
    /// entry that selected nothing else on the way.
    fn assert_reached_directly(&self, run: &Run, call: &Call) {
        let real = self.real_cargo();
        assert!(
            same_file(&call.program, &real),
            "the Cargo that ran is not the toolchain's:\n{}",
            run.report()
        );
        assert_eq!(
            call.variable(DEPTH),
            Some("1"),
            "the command went through more than one mbx shim:\n{}",
            run.report()
        );
        let chain = call
            .variable(CHAIN)
            .unwrap_or_default()
            .lines()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        assert!(
            matches!(chain.as_slice(), [only] if same_file(only, &real)),
            "the shims selected more than the toolchain's Cargo: {chain:?}\n{}",
            run.report()
        );
    }

    /// Run `--version` from each shim, with each shim leading PATH, and with
    /// each `CARGO` value `cargo_for` gives that shim, and require the
    /// toolchain's Cargo to run once and directly every time.
    fn assert_direct_everywhere(&self, cargo_for: impl Fn(Shim) -> Vec<Option<PathBuf>>) {
        for entry in Shim::BOTH {
            for path in Shim::ORDERS {
                for cargo in cargo_for(entry) {
                    let run = self.run(&Launch {
                        entry,
                        path,
                        cargo: cargo.as_deref(),
                        arguments: &["--version"],
                        session: false,
                    });
                    run.succeeded();
                    self.assert_reached_directly(&run, run.only_call());
                    assert!(run.stdout().contains(VERSION), "{}", run.report());
                }
            }
        }
    }
}

/// A command that runs in the fixture alone: nothing of the caller's
/// environment reaches it, and what it would write outside the fixture goes
/// inside it.
fn isolated(program: impl AsRef<OsStr>, base: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(base)
        .env_clear()
        .env("HOME", base.join("home"))
        .env("XDG_DATA_HOME", base.join("xdg/data"))
        .env("XDG_CONFIG_HOME", base.join("xdg/config"))
        .env("MBX_CACHE_DIR", base.join("cache"))
        // Debug builds of `mbx setup` honor these two. The directories above
        // keep every other build inside the fixture.
        .env("MBX_TEST_SHIM_DIR", base.join("setup-shim"))
        .env(
            "MBX_TEST_RUST_ANALYZER_CONFIG",
            base.join("rust-analyzer.toml"),
        );
    command
}

fn write_executable(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The same file under either spelling: a temporary directory is often reached
/// through a symlink.
fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// What `makeWrapper` writes for `flake.nix`'s `--set MBX_CARGO_SHIM_MODE 1
/// --set MBX_CARGO_SHIM_PATH "$out/libexec/mbx/cargo"`, with the host's shell
/// in place of the store's.
fn nix_wrapper(shim: &Path) -> String {
    format!(
        "#! /bin/sh -e\nexport MBX_CARGO_SHIM_MODE='1'\nexport MBX_CARGO_SHIM_PATH='{shim}'\nexec \"{MBX}\"  \"$@\" \n",
        shim = shim.display(),
    )
}

/// The toolchain's Cargo. It records each call, then answers the two commands
/// the tests send it. `fake-subcommand` stands for a Cargo plugin that starts
/// Cargo again through PATH, as `cargo nextest` and `cargo watch` do.
///
/// Its text names no mbx variable: a Cargo that looked like a shim would be
/// skipped as one.
fn toolchain_cargo(calls: &Path) -> String {
    format!(
        r#"#!/bin/sh
{{
  echo "call $0"
  for argument do printf 'argument %s\n' "$argument"; done
  if peer-tool; then echo "peer reachable"; else echo "peer missing"; fi
  echo environment
  env
  echo end
}} >>'{calls}'
case "$1" in
  --version) echo '{VERSION}' ;;
  fake-subcommand) cargo --version ;;
esac
"#,
        calls = calls.display(),
    )
}

fn is_variable_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Read what [`toolchain_cargo`] wrote: per call, its program, its arguments,
/// whether the peer tool was reachable, and its environment.
fn parse_calls(log: &str) -> Vec<Call> {
    let mut calls = Vec::new();
    let mut lines = log.lines();
    while let Some(line) = lines.next() {
        let program = line
            .strip_prefix("call ")
            .unwrap_or_else(|| panic!("the call log should open a record here: {line:?}\n{log}"));
        let mut call = Call {
            program: PathBuf::from(program),
            arguments: Vec::new(),
            peer_reachable: false,
            environment: BTreeMap::new(),
        };
        for line in lines.by_ref().take_while(|line| *line != "environment") {
            match line.strip_prefix("argument ") {
                Some(argument) => call.arguments.push(argument.to_owned()),
                None => call.peer_reachable = line == "peer reachable",
            }
        }
        let mut last = None;
        for line in lines.by_ref().take_while(|line| *line != "end") {
            match line
                .split_once('=')
                .filter(|(name, _)| is_variable_name(name))
            {
                Some((name, value)) => {
                    call.environment.insert(name.to_owned(), value.to_owned());
                    last = Some(name);
                }
                // The rest of a value that holds a line break, as the loop
                // guard's chain does.
                None => {
                    let value = call
                        .environment
                        .get_mut(last.expect("an environment dump opens with a variable"))
                        .expect("the last variable was inserted");
                    value.push('\n');
                    value.push_str(line);
                }
            }
        }
        calls.push(call);
    }
    calls
}

/// Whichever of two mbx Cargo shims a command starts through, and whichever
/// leads PATH, the toolchain's Cargo is the one that runs, and no other shim
/// runs in between. A shim that took the other one for the real Cargo would
/// hand the command down a chain of mbx processes instead.
#[test]
fn a_shim_runs_the_toolchain_cargo_when_another_mbx_shim_is_on_path() {
    Toolchain::new().assert_direct_everywhere(|_| vec![None]);
}

/// `CARGO` naming an mbx shim, the other one or the entry shim itself, selects
/// nothing: a shim is never the Cargo that another shim runs.
#[test]
fn a_cargo_variable_naming_an_mbx_shim_selects_the_toolchain_cargo_instead() {
    let toolchain = Toolchain::new();
    toolchain.assert_direct_everywhere(|entry| {
        vec![
            Some(toolchain.shim(entry.other())),
            Some(toolchain.shim(entry)),
        ]
    });
}

/// The control for the two cases above: `CARGO` naming the toolchain's Cargo is
/// used as given, with two shims on PATH.
#[test]
fn a_cargo_variable_naming_the_toolchain_cargo_is_honored_between_two_shims() {
    let toolchain = Toolchain::new();
    toolchain.assert_direct_everywhere(|_| vec![Some(toolchain.real_cargo())]);
}

/// The PATH a shim hands down drops the directory of an `mbx-target` shim. The
/// Nix shim's directory can hold tools other than the shim, so it stays.
#[test]
fn a_shim_hands_down_a_path_without_the_setup_shim_and_with_the_nix_shim_directory() {
    let toolchain = Toolchain::new();
    for entry in Shim::BOTH {
        for path in Shim::ORDERS {
            let run = toolchain.run(&Launch {
                entry,
                path,
                cargo: None,
                arguments: &["--version"],
                session: false,
            });
            run.succeeded();
            let call = run.only_call();
            assert!(
                call.path()
                    .iter()
                    .all(|dir| !same_file(dir, &toolchain.setup_dir)),
                "the `mbx-target` shim's directory is still on PATH:\n{}",
                run.report()
            );
            assert!(
                call.peer_reachable,
                "the tool beside the Nix shim left PATH with the shim:\n{}",
                run.report()
            );
        }
    }
}

/// A Cargo plugin inside a session starts Cargo again through PATH. That call
/// reaches a shim, which passes it through as the session's own, and it ends at
/// the toolchain's Cargo.
#[test]
fn a_cargo_plugin_inside_a_session_reaches_the_toolchain_cargo_again() {
    let toolchain = Toolchain::new();
    for entry in Shim::BOTH {
        for path in Shim::ORDERS {
            let run = toolchain.run(&Launch {
                entry,
                path,
                cargo: None,
                arguments: &["fake-subcommand"],
                session: true,
            });
            run.succeeded();
            let [plugin, nested] = run.calls.as_slice() else {
                panic!(
                    "expected the plugin's call and the Cargo it started:\n{}",
                    run.report()
                );
            };
            toolchain.assert_reached_directly(&run, plugin);
            assert_eq!(plugin.arguments, ["fake-subcommand"], "{}", run.report());
            assert_eq!(nested.arguments, ["--version"], "{}", run.report());
            assert!(
                same_file(&nested.program, &toolchain.real_cargo()),
                "{}",
                run.report()
            );
            assert!(run.stdout().contains(VERSION), "{}", run.report());
        }
    }
}

/// A wrapper that no shim recognizes and that runs `cargo` from PATH reaches
/// the Nix shim that stays on PATH, and that shim selects the wrapper again
/// through `CARGO`. Skipping shims when a Cargo is selected must not blind the
/// loop guard to a wrapper of this kind.
#[test]
fn a_plain_wrapper_that_reaches_a_nix_shim_again_trips_the_loop_guard() {
    let toolchain = Toolchain::new();
    let run = toolchain.run(&Launch {
        entry: Shim::Setup,
        path: [Shim::Setup, Shim::Nix],
        cargo: Some(toolchain.wrapper.as_path()),
        arguments: &["--version"],
        session: false,
    });
    let stderr = run.stderr();
    let wrapper = toolchain.wrapper.to_string_lossy().into_owned();
    assert!(
        !run.output.status.success(),
        "the loop ended without an error:\n{}",
        run.report()
    );
    assert!(
        stderr.contains("re-entered its Cargo shim"),
        "{}",
        run.report()
    );
    assert!(
        stderr.contains("the CARGO environment variable"),
        "{}",
        run.report()
    );
    assert!(stderr.contains(&wrapper), "{}", run.report());
    assert!(
        !stderr.contains("nothing stopped the loop"),
        "{}",
        run.report()
    );
    assert!(
        run.calls.is_empty(),
        "the loop reached the toolchain's Cargo:\n{}",
        run.report()
    );
}

/// The `run --config` path restores the caller's environment but must keep the
/// loop guard's current state. Real Cargo serves metadata only; no compiler runs.
#[test]
fn an_opaque_wrapper_loop_through_plain_launch_keeps_the_guard_state() {
    let toolchain = Toolchain::new();
    let base = toolchain.root.path();
    std::fs::create_dir(base.join("src")).unwrap();
    std::fs::write(
        base.join("Cargo.toml"),
        "[package]\nname='loop-fixture'\nversion='0.0.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(base.join("src/main.rs"), "fn main() {}\n").unwrap();
    let cargo = std::env::var("CARGO").expect("Cargo sets its executable for integration tests");
    let wrapper = format!(
        "#!/bin/sh\ncase \" $* \" in *\" metadata \"*) exec '{}' \"$@\";; esac\n{}",
        cargo.replace('\'', "'\\''"),
        PLAIN_WRAPPER.strip_prefix("#!/bin/sh\n").unwrap(),
    );
    write_executable(&toolchain.wrapper, &wrapper);
    let run = toolchain.run(&Launch {
        entry: Shim::Setup,
        path: [Shim::Setup, Shim::Nix],
        cargo: Some(toolchain.wrapper.as_path()),
        arguments: &["run", "--offline", "--config", "build.jobs=1"],
        session: false,
    });
    let stderr = run.stderr();
    assert!(!run.output.status.success(), "{}", run.report());
    assert!(
        stderr.contains("re-entered its Cargo shim"),
        "{}",
        run.report()
    );
    assert!(
        !stderr.contains("nothing stopped the loop"),
        "{}",
        run.report()
    );
    assert!(run.calls.is_empty(), "{}", run.report());
}
