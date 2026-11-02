//! Clippy's driver under one name for every installation.
//!
//! Cargo mixes the path of `RUSTC_WORKSPACE_WRAPPER` into the `-C metadata`
//! and `-C extra-filename` of every workspace member, and `cargo-clippy` names
//! that wrapper `clippy-driver` beside its own executable path. On macOS that
//! path is the one Cargo ran, not the file behind it. Every installation of a
//! toolchain therefore set every member's arguments apart, and with them the
//! invocation a prediction is recorded under: a Nix or devenv profile's link,
//! the next build of that profile, and a second derivation of the same
//! toolchain that only adds targets, such as the profile a land gate runs in.
//! Each linted every member as though nothing had run before, with nothing to
//! look up, although the driver it ran was the same file byte for byte.
//!
//! Cargo keeps a bare program name as it is given, hashes that name, and runs
//! it from `PATH`. Before Cargo starts a `clippy`, mbx places a `cargo-clippy`
//! first on Cargo's `PATH` that runs the one Cargo would have run, with `CARGO`
//! naming a second script. `cargo-clippy` starts Cargo through `CARGO` with the
//! driver it chose in `RUSTC_WORKSPACE_WRAPPER`. When the `clippy-driver` on
//! `PATH` is that same file, the script hands Cargo the bare name instead, so
//! every installation hashes the same name while each runs its own driver. Any
//! other driver, such as a rustup toolchain's behind a proxy on `PATH`, keeps
//! the path `cargo-clippy` gave it. mbx keys the driver by its identity and the
//! toolchain parts a compilation uses, never by where it is installed.
//!
//! The route is POSIX shell, so it exists only on Unix.

use eyre::{Result, WrapErr as _};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const CARGO_CLIPPY: &str = "cargo-clippy";
/// The Cargo that started `cargo-clippy`, held while `CARGO` names the route.
const REAL_CARGO_ENV: &str = "MBX_CLIPPY_CARGO";

/// The script `cargo-clippy` starts as Cargo. A variable already in the
/// environment stays exported when it is assigned.
const CARGO_SCRIPT: &str = "#!/bin/sh
if driver=$(command -v clippy-driver) && [ \"$driver\" -ef \"$RUSTC_WORKSPACE_WRAPPER\" ]; then
  RUSTC_WORKSPACE_WRAPPER=clippy-driver
fi
CARGO=$MBX_CLIPPY_CARGO
unset MBX_CLIPPY_CARGO
exec \"$CARGO\" \"$@\"
";

/// A `cargo-clippy` ahead of Cargo's search that hands Cargo the driver by its
/// bare name.
pub(super) struct ClippyRoute {
    bin: PathBuf,
}

impl ClippyRoute {
    /// Install the route in `directory` when `arguments` run Clippy; `None`
    /// for any other command, and when Cargo would find no `cargo-clippy`.
    pub(super) fn prepare(arguments: &[String], directory: &Path) -> Result<Option<Self>> {
        if super::launch::cargo_subcommand(arguments) != Some("clippy") {
            return Ok(None);
        }
        let cargo_home = super::launch::cargo_home(
            &std::env::current_dir()?,
            &super::launch::current_environment(),
        );
        let Some(installed) =
            cargo_clippy(std::env::var_os("PATH").as_deref(), cargo_home.as_deref())
        else {
            return Ok(None);
        };
        let directory = directory.join("clippy");
        install(&directory, &installed)
            .wrap_err_with(|| format!("failed to route Clippy to {}", installed.display()))?;
        Ok(Some(Self {
            bin: directory.join("bin"),
        }))
    }

    /// Put the route first on the `PATH` Cargo gets.
    pub(super) fn environment(&self, environment: &mut BTreeMap<String, String>) -> Result<()> {
        let path = environment
            .get("PATH")
            .map(OsString::from)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(std::env::split_paths(&path)),
        )?;
        environment.insert("PATH".into(), path.to_string_lossy().into_owned());
        Ok(())
    }
}

/// The `cargo-clippy` Cargo would run: from `cargo_home`'s `bin` first when
/// `PATH` does not list that directory, then from `PATH` in order.
fn cargo_clippy(path: Option<&OsStr>, cargo_home: Option<&Path>) -> Option<PathBuf> {
    let listed = path.map_or_else(Vec::new, |path| std::env::split_paths(path).collect());
    let unlisted_home = cargo_home
        .map(|home| home.join("bin"))
        .filter(|bin| !listed.contains(bin));
    unlisted_home
        .into_iter()
        .chain(listed)
        .map(|directory| directory.join(CARGO_CLIPPY))
        .find(|candidate| super::blaze::is_executable(candidate))
}

/// Write `directory/bin/cargo-clippy`, which runs `installed` with `CARGO`
/// naming `directory/cargo`, and that [`CARGO_SCRIPT`].
fn install(directory: &Path, installed: &Path) -> Result<()> {
    let cargo = directory.join("cargo");
    let mut route =
        format!("#!/bin/sh\nexport {REAL_CARGO_ENV}=\"${{CARGO:-cargo}}\" CARGO=").into_bytes();
    push_quoted(&mut route, &cargo);
    route.extend_from_slice(b"\nexec ");
    push_quoted(&mut route, installed);
    route.extend_from_slice(b" \"$@\"\n");
    std::fs::create_dir_all(directory.join("bin"))?;
    write_executable(&directory.join("bin").join(CARGO_CLIPPY), &route)?;
    write_executable(&cargo, CARGO_SCRIPT.as_bytes())
}

/// `path` as one single-quoted shell word.
fn push_quoted(script: &mut Vec<u8>, path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;

    script.push(b'\'');
    for &byte in path.as_os_str().as_bytes() {
        if byte == b'\'' {
            script.extend_from_slice(b"'\\''");
        } else {
            script.push(byte);
        }
    }
    script.push(b'\'');
}

fn write_executable(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)?
        .write_all(contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn joined(directories: &[&Path]) -> OsString {
        std::env::join_paths(directories).unwrap()
    }

    #[test]
    fn cargo_searches_an_unlisted_home_first_and_path_in_order() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile/bin");
        let other = root.path().join("other/bin");
        let home = root.path().join("cargo-home");
        for directory in [&profile, &other, &home.join("bin")] {
            executable(&directory.join(CARGO_CLIPPY), "#!/bin/sh\n");
        }

        assert_eq!(
            cargo_clippy(Some(&joined(&[&profile, &other])), Some(&home)),
            Some(home.join("bin").join(CARGO_CLIPPY))
        );
        assert_eq!(
            cargo_clippy(
                Some(&joined(&[&profile, &home.join("bin"), &other])),
                Some(&home)
            ),
            Some(profile.join(CARGO_CLIPPY))
        );
        assert_eq!(
            cargo_clippy(Some(&joined(&[&other, &profile])), None),
            Some(other.join(CARGO_CLIPPY))
        );
        assert_eq!(cargo_clippy(Some(&joined(&[root.path()])), None), None);
    }

    /// Run the route as Cargo would, with a `cargo-clippy` that hands Cargo the
    /// `clippy-driver` beside itself, and report what the real Cargo received.
    fn routed(path_driver: impl Fn(&Path, &Path)) -> String {
        let root = tempfile::tempdir().unwrap();
        let toolchain = root.path().join("it's a toolchain/bin");
        executable(
            &toolchain.join(CARGO_CLIPPY),
            "#!/bin/sh\nRUSTC_WORKSPACE_WRAPPER=\"$(dirname \"$0\")/clippy-driver\" exec \"$CARGO\" check \"$@\"\n",
        );
        executable(&toolchain.join("clippy-driver"), "#!/bin/sh\n");
        let profile = root.path().join("profile/bin");
        std::fs::create_dir_all(&profile).unwrap();
        path_driver(&toolchain, &profile);
        let real_cargo = root.path().join("real/cargo");
        executable(
            &real_cargo,
            "#!/bin/sh\nprintf '%s|%s|%s|' \"$RUSTC_WORKSPACE_WRAPPER\" \"$CARGO\" \"${MBX_CLIPPY_CARGO-unset}\"\nprintf '%s ' \"$@\"\n",
        );
        let route = root.path().join("session/clippy");
        install(&route, &toolchain.join(CARGO_CLIPPY)).unwrap();

        let output = std::process::Command::new(route.join("bin").join(CARGO_CLIPPY))
            .args(["--workspace", "a b"])
            .env("CARGO", &real_cargo)
            .env(
                "PATH",
                joined(&[
                    &route.join("bin"),
                    &profile,
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                ]),
            )
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .replace(&real_cargo.display().to_string(), "<cargo>")
            .replace(&toolchain.display().to_string(), "<toolchain>")
    }

    #[test]
    fn the_driver_on_path_is_handed_to_cargo_by_its_bare_name() {
        let linked = routed(|toolchain, profile| {
            std::os::unix::fs::symlink(
                toolchain.join("clippy-driver"),
                profile.join("clippy-driver"),
            )
            .unwrap();
        });
        assert_eq!(linked, "clippy-driver|<cargo>|unset|check --workspace a b ");
    }

    #[test]
    fn another_driver_on_path_leaves_cargo_clippy_s_choice() {
        let other = routed(|_, profile| {
            executable(&profile.join("clippy-driver"), "#!/bin/sh\n");
        });
        assert_eq!(
            other,
            "<toolchain>/clippy-driver|<cargo>|unset|check --workspace a b "
        );
        let absent = routed(|_, _| {});
        assert_eq!(
            absent,
            "<toolchain>/clippy-driver|<cargo>|unset|check --workspace a b "
        );
    }
}
