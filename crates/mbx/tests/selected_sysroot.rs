//! Selected compiler sysroots outside HOME and RUSTUP_HOME remain cacheable.
#![cfg(unix)]

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    shim: PathBuf,
}

impl Fixture {
    fn compile(&self, compiler: &Path, sysroot: Option<&Path>) {
        let mut command = Command::new(&self.shim);
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", self.root.join("home"))
            .env("RUSTUP_HOME", self.root.join("home/.rustup"))
            .env("CARGO_HOME", self.root.join("home/.cargo"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("MBX_CACHE_DIR", self.root.join("cache"))
            .env("MBX_LEARNED_INCREMENTAL", "0")
            .env("MBX_SCHEDULER", "0")
            .env("TEST_COMPILE_LOG", self.root.join("compiles"))
            .arg(compiler)
            .args([
                "--crate-name",
                "fixture",
                "--crate-type=lib",
                "--emit=dep-info,metadata",
                "--out-dir=target",
            ]);
        if let Some(sysroot) = sysroot {
            command.arg("--sysroot").arg(sysroot);
        }
        let output = command.arg("lib.rs").output().unwrap();
        assert!(
            output.status.success(),
            "compiler failed for {}: {}",
            compiler.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_artifact(&self, expected: &str, compilations: usize) {
        assert_eq!(
            std::fs::read_to_string(self.workspace.join("target/libfixture.rmeta")).unwrap(),
            expected,
            "the cache must not substitute another compiler or custom sysroot"
        );
        assert!(self.workspace.join("target/fixture.d").is_file());
        assert_eq!(
            std::fs::read_to_string(self.root.join("compiles"))
                .unwrap()
                .lines()
                .count(),
            compilations,
            "selected default sysroot should store and restore instead of compiling again"
        );
    }

    fn wipe_outputs(&self) {
        std::fs::remove_dir_all(self.workspace.join("target")).unwrap();
    }
}

fn compiler(root: &Path, name: &str) -> PathBuf {
    let sysroot = root.join("nix/store").join(name);
    std::fs::create_dir_all(sysroot.join("bin")).unwrap();
    std::fs::create_dir_all(sysroot.join("lib/rustlib")).unwrap();
    std::fs::write(sysroot.join("marker"), name).unwrap();
    let compiler = sysroot.join("bin/rustc");
    std::fs::write(
        &compiler,
        r#"#!/bin/sh
set -eu
default=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
case "$1" in
    -vV)
        printf 'rustc 1.97.0 (abc 2026-08-01)\ncommit-hash: abc\nhost: x86_64-unknown-linux-gnu\nrelease: 1.97.0\nLLVM version: 22.0.0\n'
        exit 0
        ;;
    --print) printf '%s\n' "$default"; exit 0 ;;
esac
sysroot=$default
while [ "$#" -gt 0 ]; do
    case "$1" in
        --sysroot) shift; sysroot=$1 ;;
        --sysroot=*) sysroot=${1#--sysroot=} ;;
    esac
    shift
done
printf 'compile\n' >> "$TEST_COMPILE_LOG"
mkdir -p target
cat "$sysroot/marker" > target/libfixture.rmeta
printf 'target/libfixture.rmeta: lib.rs\n' > target/fixture.d
"#,
    )
    .unwrap();
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755)).unwrap();
    compiler
}

#[test]
fn selected_physical_sysroot_stores_and_restores_without_conflating_roots() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("shims")).unwrap();
    std::fs::write(workspace.join("lib.rs"), "pub fn fixture() {}\n").unwrap();
    let shim = mbx::session::install_shim(
        Path::new(env!("CARGO_BIN_EXE_mbx")),
        &root.join("shims"),
        mbx::session::ShimLink::Tracking,
    )
    .unwrap();
    let first = compiler(&root, "first-rustc");
    let second = compiler(&root, "second-rustc");
    let first_sysroot = first.parent().unwrap().parent().unwrap();
    let second_sysroot = second.parent().unwrap().parent().unwrap();
    let selected = root.join("selected-toolchain");
    symlink(first_sysroot, &selected).unwrap();
    let fixture = Fixture {
        root,
        workspace,
        shim,
    };

    // The selected compiler is lexical, while --print sysroot gives its physical root.
    fixture.compile(&selected.join("bin/rustc"), Some(first_sysroot));
    fixture.assert_artifact("first-rustc", 1);
    fixture.wipe_outputs();
    fixture.compile(&selected.join("bin/rustc"), Some(first_sysroot));
    fixture.assert_artifact("first-rustc", 1);
    fixture.wipe_outputs();
    fixture.compile(&selected.join("bin/rustc"), Some(&selected));
    fixture.assert_artifact("first-rustc", 1);

    // Equal -vV text does not prove two installations have the same standard library.
    fixture.wipe_outputs();
    fixture.compile(&second, Some(second_sysroot));
    fixture.assert_artifact("second-rustc", 2);
    fixture.wipe_outputs();
    fixture.compile(&second, Some(second_sysroot));
    fixture.assert_artifact("second-rustc", 2);

    // Explicit overrides are not aliases for the selected compiler's default root.
    for (index, name) in ["custom-a", "custom-b", "custom-a"].into_iter().enumerate() {
        let custom = fixture.root.join(name);
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("marker"), name).unwrap();
        fixture.wipe_outputs();
        fixture.compile(&first, Some(&custom));
        fixture.assert_artifact(name, 3 + index);
    }
}

#[test]
fn persisted_compiler_identity_follows_selected_sysroot_alias() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("shims")).unwrap();
    std::fs::write(workspace.join("lib.rs"), "pub fn fixture() {}\n").unwrap();
    let shim = mbx::session::install_shim(
        Path::new(env!("CARGO_BIN_EXE_mbx")),
        &root.join("shims"),
        mbx::session::ShimLink::Tracking,
    )
    .unwrap();
    let first = compiler(&root, "first-rustc");
    let second = compiler(&root, "second-rustc");
    let first_sysroot = first.parent().unwrap().parent().unwrap();
    let second_sysroot = second.parent().unwrap().parent().unwrap();

    // Create every hardlink before probing: adding one later changes ctime
    // and would invalidate the compiler pins without testing the alias.
    std::fs::remove_file(&second).unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let driver = first_sysroot.join("lib/librustc_driver-fixture.so");
    std::fs::write(&driver, b"shared compiler driver").unwrap();
    let second_driver = second_sysroot.join("lib/librustc_driver-fixture.so");
    std::fs::hard_link(&driver, second_driver).unwrap();
    for (sysroot, contents) in [
        (first_sysroot, "first backend"),
        (second_sysroot, "second backend"),
    ] {
        let backends = sysroot.join("lib/rustlib/x86_64-unknown-linux-gnu/codegen-backends");
        std::fs::create_dir_all(&backends).unwrap();
        std::fs::write(backends.join("librustc_codegen_fixture.so"), contents).unwrap();
    }

    let selected = root.join("selected-toolchain");
    symlink(first_sysroot, &selected).unwrap();
    let fixture = Fixture {
        root,
        workspace,
        shim,
    };
    let selected_compiler = selected.join("bin/rustc");
    fixture.compile(&selected_compiler, None);
    fixture.assert_artifact("first-rustc", 1);
    fixture.wipe_outputs();
    fixture.compile(&selected_compiler, None);
    fixture.assert_artifact("first-rustc", 1);

    // Leave the old physical backend pins intact. An explicit --sysroot
    // could merely bypass a stale mapping, hiding a wrong implicit-root hit.
    fixture.wipe_outputs();
    std::fs::remove_file(&selected).unwrap();
    symlink(second_sysroot, &selected).unwrap();
    fixture.compile(&selected_compiler, None);
    fixture.assert_artifact("second-rustc", 2);
    fixture.wipe_outputs();
    fixture.compile(&selected_compiler, None);
    fixture.assert_artifact("second-rustc", 2);
}

#[test]
fn identity_probe_failure_preserves_execution_only_build_script_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let output_dir = workspace.join("target/debug/build/fixture");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("shims")).unwrap();
    std::fs::write(workspace.join("build.rs"), "fn main() {}\n").unwrap();
    let shim = mbx::session::install_shim(
        Path::new(env!("CARGO_BIN_EXE_mbx")),
        &root.join("shims"),
        mbx::session::ShimLink::Tracking,
    )
    .unwrap();
    let compiler = root.join("compiler");
    std::fs::write(
        &compiler,
        r#"#!/bin/sh
set -eu
if [ "$1" = '-vV' ]; then
    printf 'identity\n' >> "$TEST_COMPILER_LOG"
    printf 'identity reporting is unavailable\n' >&2
    exit 9
fi
printf 'compile\n' >> "$TEST_COMPILER_LOG"
mkdir -p "$TEST_OUTPUT_DIR"
printf '#!/bin/sh\nprintf "ran-build-script\\n"\n' > "$TEST_OUTPUT_DIR/build_script_build"
chmod +x "$TEST_OUTPUT_DIR/build_script_build"
printf '%s/build_script_build: build.rs\n' "$TEST_OUTPUT_DIR" > "$TEST_OUTPUT_DIR/build_script_build.d"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(&shim)
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root.join("home"))
        .env("MBX_CACHE_DIR", root.join("cache"))
        .env("MBX_SCHEDULER", "0")
        .env("MBX_CACHE_LINKS", "0")
        .env("MBX_BUILD_SCRIPT_EXECUTION", "1")
        .env("TEST_COMPILER_LOG", root.join("compiler.log"))
        .env("TEST_OUTPUT_DIR", &output_dir)
        .arg(&compiler)
        .args([
            "--crate-name=build_script_build",
            "--crate-type=bin",
            "--emit=dep-info,link",
            "--out-dir",
        ])
        .arg(&output_dir)
        .arg("build.rs")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the compiler's successful result must survive identity failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_dir.join("build_script_build").is_file());
    let calls = std::fs::read_to_string(root.join("compiler.log")).unwrap();
    assert_eq!(
        calls.lines().next(),
        Some("compile"),
        "execution-only compilation must precede optional modeled-key probing: {calls}"
    );
    assert!(
        output_dir.join("build_script_build.mbx-real").is_file(),
        "identity failure must retain the exact-binary execution-cache launcher fallback"
    );
    let run = Command::new(output_dir.join("build_script_build"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root.join("home"))
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "the installed launcher must remain transparent"
    );
    assert_eq!(run.stdout, b"ran-build-script\n");
}

#[test]
fn identity_probe_failure_warns_without_discarding_successful_compilation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("shims")).unwrap();
    std::fs::write(workspace.join("lib.rs"), "pub fn fixture() {}\n").unwrap();
    let shim = mbx::session::install_shim(
        Path::new(env!("CARGO_BIN_EXE_mbx")),
        &root.join("shims"),
        mbx::session::ShimLink::Tracking,
    )
    .unwrap();
    let compiler = root.join("compiler");
    std::fs::write(
        &compiler,
        r#"#!/bin/sh
set -eu
if [ "$1" = '-vV' ]; then
    printf 'identity reporting is unavailable\n' >&2
    exit 9
fi
mkdir -p target
printf 'successful metadata' > target/libfixture.rmeta
printf 'target/libfixture.rmeta: lib.rs\n' > target/fixture.d
"#,
    )
    .unwrap();
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mbx"))
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root.join("home"))
        .env("MBX_CACHE_DIR", root.join("cache"))
        .env("MBX_SCHEDULER", "0")
        // Deliberately use exec's standalone path; no C compiler/session is needed.
        .env("MBX_CC", "0")
        .env("MBX_LEARNED_INCREMENTAL", "0")
        .args(["exec", "--"])
        .arg(&shim)
        .arg(&compiler)
        .args([
            "--crate-name=fixture",
            "--crate-type=lib",
            "--emit=dep-info,metadata",
            "--out-dir=target",
            "lib.rs",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "successful compilation was lost: {stderr}"
    );
    assert_eq!(
        std::fs::read(workspace.join("target/libfixture.rmeta")).unwrap(),
        b"successful metadata"
    );
    assert!(
        stderr.contains("prediction was not restored: rustc identity command failed")
            && stderr.contains("identity reporting is unavailable"),
        "the existing key-lookup identity-failure warning must remain visible: {stderr}"
    );
}

/// A `-Zbuild-std` unit of a symlink-joined toolchain, as a Nix toolchain is:
/// `rustc --print sysroot` names the join, whose directories are its own and
/// whose leaves link into the store paths it joins. Cargo runs the unit in the
/// join's `rust-src` and names its sources as the join spells them, and a build
/// that remaps the sysroot passes the join itself. Every one of those sources
/// resolves into `rust-src`'s own store path, outside the join.
#[test]
fn a_build_std_unit_of_a_joined_toolchain_stores_and_restores() {
    build_std_unit_of_a_joined_toolchain_stores_and_restores(
        Leaves::Directories,
        Reached::Directly,
    );
}

/// The join as Nix lays it out on the host whose wasm build-std units were not
/// stored, 2026-10-31: the join's directories are its own down to each file,
/// and every file links into `rust-src`'s store path, so Cargo names
/// `/nix/store/<hash>-rust-nightly-<version>-<version>/lib/rustlib/src/rust/library/<crate>/src/lib.rs`.
#[test]
fn a_build_std_unit_of_a_nix_join_whose_files_are_links_stores_and_restores() {
    build_std_unit_of_a_joined_toolchain_stores_and_restores(Leaves::Files, Reached::Directly);
}

/// The host whose wasm build-std units were still not stored on 2026-10-31:
/// Cargo runs a bare `rustc` that `PATH` finds in a devenv profile, whose
/// `bin/rustc` and `lib/rustlib` link into the join. rustc finds its sysroot
/// in the join and Cargo names the sources by it, while the profile holds a
/// `lib/rustlib` of its own. A toolchain update relinks the profile into
/// another join, whose unit compiles again and is then restored: equal version
/// text and sources do not make it the first installation.
#[test]
fn a_build_std_unit_of_a_nix_join_found_through_a_profile_stores_and_restores() {
    build_std_unit_of_a_joined_toolchain_stores_and_restores(
        Leaves::Files,
        Reached::ThroughProfile,
    );
}

/// The same host when Cargo runs the profile's own link by its path, as
/// `RUSTC=$(command -v rustc)` makes it: rustc still finds its sysroot in the
/// join, and Cargo names the sources by it, so the unit is keyed beneath the
/// join, not the profile.
#[test]
fn a_build_std_unit_of_a_nix_join_run_by_a_profile_link_stores_and_restores() {
    build_std_unit_of_a_joined_toolchain_stores_and_restores(
        Leaves::Files,
        Reached::ThroughProfileLink,
    );
}

/// The store names of that host's toolchain join and the `rust-src` it joins,
/// and of the join the AxE shell's profile linked next.
const RUST_NIGHTLY: &str = "dh0b8lj8avd02zizqd7xb452z5fp13i0-rust-nightly-1.101.0-nightly-2026-10-04-1.101.0-nightly-2026-10-04";
const NEXT_RUST_NIGHTLY: &str = "1kjmckrlwgq5202zbjb4gbn1sbr9ixdw-rust-nightly-1.101.0-nightly-2026-10-12-1.101.0-nightly-2026-10-12";
const RUST_SRC: &str =
    "1p78ifxxkym8ad99nw8x4x0mhzy0v25q-rust-src-1.101.0-nightly-2026-10-04-aarch64-apple-darwin";

/// How a toolchain join links what it joins.
#[derive(Clone, Copy)]
enum Leaves {
    /// A directory of the join is a link into `rust-src`.
    Directories,
    /// The join's directories are its own, and each file is a link.
    Files,
}

/// How Cargo reaches the toolchain's compiler.
#[derive(Clone, Copy)]
enum Reached {
    /// By the join's own `bin/rustc`.
    Directly,
    /// As a bare `rustc` that `PATH` finds in a profile linking into the join.
    ThroughProfile,
    /// By the path of a profile's `bin/rustc`, a link into the join.
    ThroughProfileLink,
}

/// The toolchain join `name` in `root`'s `nix/store`: `core`'s sources link
/// into `source` as `leaves` lays them out, and its `bin/rustc` compiles a
/// build-std unit by copying the source into `libcore.rmeta` and, as rustc
/// does, reports the join its own file sits in as its sysroot.
fn joined_toolchain(root: &Path, name: &str, source: &Path, leaves: Leaves) -> PathBuf {
    let join = root.join(format!("nix/store/{name}"));
    let core = join.join("lib/rustlib/src/rust/library/core");
    match leaves {
        Leaves::Directories => {
            std::fs::create_dir_all(&core).unwrap();
            symlink(source, core.join("src")).unwrap();
        }
        Leaves::Files => {
            std::fs::create_dir_all(core.join("src")).unwrap();
            symlink(source.join("lib.rs"), core.join("src/lib.rs")).unwrap();
        }
    }
    std::fs::create_dir_all(join.join("bin")).unwrap();
    let compiler = join.join("bin/rustc");
    std::fs::write(
        &compiler,
        r#"#!/bin/sh
set -eu
executable=$0
while [ -L "$executable" ]; do
    link=$(readlink "$executable")
    case $link in
        /*) executable=$link ;;
        *) executable=$(dirname -- "$executable")/$link ;;
    esac
done
default=$(CDPATH= cd -- "$(dirname -- "$executable")/.." && pwd -P)
case "$1" in
    -vV)
        printf 'rustc 1.101.0-nightly (abc 2026-10-12)\ncommit-hash: abc\nhost: x86_64-unknown-linux-gnu\nrelease: 1.101.0-nightly\nLLVM version: 22.0.0\n'
        exit 0
        ;;
    --print|--print=*) printf '%s\n' "$default"; exit 0 ;;
esac
out=
source=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --out-dir=*) out=${1#--out-dir=} ;;
        *.rs) source=$1 ;;
    esac
    shift
done
printf 'compile\n' >> "$TEST_COMPILE_LOG"
mkdir -p "$out"
cat "$source" > "$out/libcore.rmeta"
printf '%s: %s\n%s: %s\n\n%s:\n' "$out/core.d" "$source" "$out/libcore.rmeta" "$source" "$source" > "$out/core.d"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o755)).unwrap();
    join
}

/// Links a profile's `bin/rustc` and `lib/rustlib/src` into `join`, as Nix
/// links a profile's leaves into the toolchain it holds, replacing the links
/// to the toolchain it held before.
fn link_profile(profile: &Path, join: &Path) {
    for leaf in ["bin/rustc", "lib/rustlib/src"] {
        let link = profile.join(leaf);
        if link.is_symlink() {
            std::fs::remove_file(&link).unwrap();
        }
        symlink(join.join(leaf), link).unwrap();
    }
}

fn build_std_unit_of_a_joined_toolchain_stores_and_restores(leaves: Leaves, reached: Reached) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let deps = workspace.join("target/debug/deps");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("shims")).unwrap();
    let shim = mbx::session::install_shim(
        Path::new(env!("CARGO_BIN_EXE_mbx")),
        &root.join("shims"),
        mbx::session::ShimLink::Tracking,
    )
    .unwrap();
    let source = root.join(format!(
        "nix/store/{RUST_SRC}/lib/rustlib/src/rust/library/core/src"
    ));
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("lib.rs"), "#![no_std]\n").unwrap();
    let join = joined_toolchain(&root, RUST_NIGHTLY, &source, leaves);
    let profile = root.join("nix/store/devenv-profile");
    if !matches!(reached, Reached::Directly) {
        std::fs::create_dir_all(profile.join("bin")).unwrap();
        std::fs::create_dir_all(profile.join("lib/rustlib")).unwrap();
        link_profile(&profile, &join);
    }
    let (spelled, path) = match reached {
        Reached::Directly => (
            join.join("bin/rustc").into_os_string(),
            std::env::var_os("PATH").unwrap(),
        ),
        Reached::ThroughProfile => {
            let mut path = profile.join("bin").into_os_string();
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap());
            ("rustc".into(), path)
        }
        Reached::ThroughProfileLink => (
            profile.join("bin/rustc").into_os_string(),
            std::env::var_os("PATH").unwrap(),
        ),
    };
    let compiles = root.join("compiles");
    let compiled = || std::fs::read_to_string(&compiles).unwrap().lines().count();
    let compile = |join: &Path| {
        let core = join.join("lib/rustlib/src/rust/library/core");
        let output = Command::new(&shim)
            .current_dir(&core)
            .env_clear()
            .env("PATH", path.clone())
            .env("HOME", root.join("home"))
            .env("RUSTUP_HOME", root.join("home/.rustup"))
            .env("CARGO_HOME", root.join("home/.cargo"))
            .env("XDG_CONFIG_HOME", root.join("home/.config"))
            .env("MBX_CACHE_DIR", root.join("cache"))
            .env("MBX_LEARNED_INCREMENTAL", "0")
            .env("MBX_SCHEDULER", "0")
            .env("TEST_COMPILE_LOG", &compiles)
            .env("CARGO_MANIFEST_DIR", &core)
            .arg(&spelled)
            .args([
                "--crate-name",
                "core",
                "--crate-type=lib",
                "--emit=dep-info,metadata",
                "-Zforce-unstable-if-unmarked",
            ])
            .arg(format!("--out-dir={}", deps.display()))
            .arg(format!("--remap-path-prefix={}=/rust", join.display()))
            .arg(core.join("src/lib.rs"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the build-std unit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    let stored = compile(&join);
    assert!(!stored.contains("not stored"), "{stored}");
    std::fs::remove_dir_all(workspace.join("target")).unwrap();
    let restored = compile(&join);
    assert_eq!(
        std::fs::read_to_string(deps.join("libcore.rmeta")).unwrap(),
        "#![no_std]\n"
    );
    assert_eq!(
        compiled(),
        1,
        "the build-std unit should be stored once and restored, not compiled again:\n{stored}\n{restored}"
    );
    if matches!(reached, Reached::Directly) {
        return;
    }

    let next = joined_toolchain(&root, NEXT_RUST_NIGHTLY, &source, leaves);
    link_profile(&profile, &next);
    std::fs::remove_dir_all(workspace.join("target")).unwrap();
    let switched = compile(&next);
    assert!(!switched.contains("not stored"), "{switched}");
    assert_eq!(
        compiled(),
        2,
        "the next toolchain's build-std unit must compile, not restore the first toolchain's:\n{switched}"
    );
    std::fs::remove_dir_all(workspace.join("target")).unwrap();
    let restored = compile(&next);
    assert_eq!(
        compiled(),
        2,
        "the next toolchain's build-std unit should be stored once and restored:\n{switched}\n{restored}"
    );
}
