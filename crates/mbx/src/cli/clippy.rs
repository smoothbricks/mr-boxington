//! Clippy run by the path of its own installation.
//!
//! Cargo mixes the path of `RUSTC_WORKSPACE_WRAPPER` into the `-C metadata`
//! and `-C extra-filename` of every workspace member, and `cargo-clippy` names
//! that wrapper `clippy-driver` beside its own executable path. On macOS that
//! path is the one Cargo ran, not the file behind it, so a `cargo-clippy`
//! reached through a link -- a Nix or devenv profile, a Homebrew `bin` -- puts
//! the link's directory into every member's arguments, and with them into the
//! invocation a prediction is recorded under. A profile is rebuilt whenever
//! any package in it changes, and a second profile of the same shell links the
//! same toolchain from a path of its own: each then linted every member as if
//! nothing had run before, with no prediction to look up.
//!
//! Before Cargo starts a `clippy`, mbx finds the `cargo-clippy` Cargo would run.
//! When that is a link to a `cargo-clippy` elsewhere, a `cargo-clippy` placed
//! first on Cargo's `PATH` runs the linked file by its resolved path, so Clippy
//! names its own toolchain's `clippy-driver`: one path for every profile that
//! links that toolchain, and a new one only when the toolchain changes.
//!
//! The route is a POSIX shell script, so it exists only on Unix.

use eyre::{Result, WrapErr as _};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const CARGO_CLIPPY: &str = "cargo-clippy";

/// A `cargo-clippy` ahead of Cargo's search that runs the installed one by its
/// resolved path.
pub(super) struct ClippyRoute {
    directory: PathBuf,
}

impl ClippyRoute {
    /// Install the route in `directory` when `arguments` run Clippy through a
    /// link; `None` for any other command, and for a `cargo-clippy` Cargo
    /// already runs by its own path.
    pub(super) fn prepare(arguments: &[String], directory: &Path) -> Result<Option<Self>> {
        if super::launch::cargo_subcommand(arguments) != Some("clippy") {
            return Ok(None);
        }
        let cargo_home = super::launch::cargo_home(
            &std::env::current_dir()?,
            &super::launch::current_environment(),
        );
        let Some(installed) =
            linked_cargo_clippy(std::env::var_os("PATH").as_deref(), cargo_home.as_deref())
        else {
            return Ok(None);
        };
        let directory = directory.join("clippy");
        install(&directory, &installed)
            .wrap_err_with(|| format!("failed to route Clippy to {}", installed.display()))?;
        Ok(Some(Self { directory }))
    }

    /// Put the route first on the `PATH` Cargo gets.
    pub(super) fn environment(&self, environment: &mut BTreeMap<String, String>) -> Result<()> {
        let path = environment
            .get("PATH")
            .map(OsString::from)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.directory.clone()).chain(std::env::split_paths(&path)),
        )?;
        environment.insert("PATH".into(), path.to_string_lossy().into_owned());
        Ok(())
    }
}

/// The file behind the `cargo-clippy` Cargo would run, when Cargo would run it
/// through a link.
///
/// Cargo searches `PATH` in order, after `cargo_home`'s `bin` when `PATH` does
/// not list that directory; a `cargo-clippy` found there comes ahead of the
/// route as well, so it is left alone. So is a link to a file of another name,
/// such as a rustup proxy, which picks its toolchain when it runs.
fn linked_cargo_clippy(path: Option<&OsStr>, cargo_home: Option<&Path>) -> Option<PathBuf> {
    let listed = path.map_or_else(Vec::new, |path| std::env::split_paths(path).collect());
    let unlisted_home = cargo_home
        .map(|home| home.join("bin"))
        .filter(|bin| !listed.contains(bin));
    let found = unlisted_home
        .iter()
        .chain(&listed)
        .map(|directory| directory.join(CARGO_CLIPPY))
        .find(|candidate| super::blaze::is_executable(candidate))?;
    if unlisted_home
        .as_deref()
        .is_some_and(|bin| found.parent() == Some(bin))
    {
        return None;
    }
    let resolved = std::fs::canonicalize(&found).ok()?;
    (resolved != found && resolved.file_name() == Some(OsStr::new(CARGO_CLIPPY)))
        .then_some(resolved)
}

/// Write `directory/cargo-clippy`: `exec` of `installed`, which Clippy then
/// reports as its own path.
fn install(directory: &Path, installed: &Path) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut script = b"#!/bin/sh\nexec '".to_vec();
    for &byte in installed.as_os_str().as_bytes() {
        if byte == b'\'' {
            script.extend_from_slice(b"'\\''");
        } else {
            script.push(byte);
        }
    }
    script.extend_from_slice(b"' \"$@\"\n");
    std::fs::create_dir_all(directory)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(directory.join(CARGO_CLIPPY))?
        .write_all(&script)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn joined(directories: &[&Path]) -> OsString {
        std::env::join_paths(directories).unwrap()
    }

    #[test]
    fn a_profile_link_resolves_to_its_toolchain() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let toolchain = root.join("toolchain/bin");
        let profile = root.join("profile/bin");
        std::fs::create_dir_all(&toolchain).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        executable(&toolchain.join(CARGO_CLIPPY));
        std::os::unix::fs::symlink(toolchain.join(CARGO_CLIPPY), profile.join(CARGO_CLIPPY))
            .unwrap();

        assert_eq!(
            linked_cargo_clippy(Some(&joined(&[&profile, &toolchain])), None),
            Some(toolchain.join(CARGO_CLIPPY))
        );
        // Run by its own path already: nothing to route.
        assert_eq!(
            linked_cargo_clippy(Some(&joined(&[&toolchain, &profile])), None),
            None
        );
    }

    #[test]
    fn a_proxy_and_an_unlisted_cargo_home_keep_their_own_choice() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let toolchain = root.join("toolchain/bin");
        let profile = root.join("profile/bin");
        let home = root.join("cargo-home");
        for directory in [&toolchain, &profile, &home.join("bin")] {
            std::fs::create_dir_all(directory).unwrap();
        }
        executable(&toolchain.join(CARGO_CLIPPY));
        std::os::unix::fs::symlink(toolchain.join(CARGO_CLIPPY), profile.join(CARGO_CLIPPY))
            .unwrap();
        executable(&home.join("bin/rustup"));
        std::os::unix::fs::symlink(home.join("bin/rustup"), home.join("bin").join(CARGO_CLIPPY))
            .unwrap();

        // Cargo searches an unlisted `$CARGO_HOME/bin` first, ahead of any route.
        assert_eq!(
            linked_cargo_clippy(Some(&joined(&[&profile])), Some(&home)),
            None
        );
        // Listed first, its proxy is found and picks its own toolchain.
        assert_eq!(
            linked_cargo_clippy(Some(&joined(&[&home.join("bin"), &profile])), Some(&home)),
            None
        );
        // Listed after the profile, the profile's link is what Cargo runs.
        assert_eq!(
            linked_cargo_clippy(Some(&joined(&[&profile, &home.join("bin")])), Some(&home)),
            Some(toolchain.join(CARGO_CLIPPY))
        );
    }

    #[test]
    fn the_route_runs_the_installed_file_by_its_resolved_path() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("it's here/cargo-clippy");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "#!/bin/sh\nprintf '%s|' \"$0\" \"$@\"\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let route = root.path().join("route");
        install(&route, &installed).unwrap();

        let output = std::process::Command::new(route.join(CARGO_CLIPPY))
            .args(["clippy", "--workspace", "a b"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}|clippy|--workspace|a b|", installed.display())
        );
    }
}
