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
    fn compile(&self, compiler: &Path, sysroot: &Path) {
        let output = Command::new(&self.shim)
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
                "--sysroot",
            ])
            .arg(sysroot)
            .arg("lib.rs")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "compiler failed for {}: {}",
            sysroot.display(),
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
    fixture.compile(&selected.join("bin/rustc"), first_sysroot);
    fixture.assert_artifact("first-rustc", 1);
    fixture.wipe_outputs();
    fixture.compile(&selected.join("bin/rustc"), first_sysroot);
    fixture.assert_artifact("first-rustc", 1);
    fixture.wipe_outputs();
    fixture.compile(&selected.join("bin/rustc"), &selected);
    fixture.assert_artifact("first-rustc", 1);

    // Equal -vV text does not prove two installations have the same standard library.
    fixture.wipe_outputs();
    fixture.compile(&second, second_sysroot);
    fixture.assert_artifact("second-rustc", 2);
    fixture.wipe_outputs();
    fixture.compile(&second, second_sysroot);
    fixture.assert_artifact("second-rustc", 2);

    // Explicit overrides are not aliases for the selected compiler's default root.
    for (index, name) in ["custom-a", "custom-b", "custom-a"].into_iter().enumerate() {
        let custom = fixture.root.join(name);
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("marker"), name).unwrap();
        fixture.wipe_outputs();
        fixture.compile(&first, &custom);
        fixture.assert_artifact(name, 3 + index);
    }
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
