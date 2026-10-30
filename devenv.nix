# mr-boxington's development shell: the Rust toolchain and the tools the checks
# in AGENTS.md ("Before Pushing") run. Cargo is reached through the host's mbx
# route, which cowshed puts ahead of this shell on PATH (smoothbricks
# specs/cowshed/11_shell.md, "Canonical mbx environment"), so the shell pins no
# mbx and no Cargo shim of its own.
{
  inputs,
  lib,
  pkgs,
  options,
  ...
}: {
  # On Darwin the shell carries no C compiler or Apple SDK of its own:
  # /usr/bin/cc, Xcode's SDK and /usr/bin/xcrun answer, as in the other
  # repositories' shells, and no Nix cc-wrapper flags, store SDKROOT,
  # DEVELOPER_DIR or xcbuild xcrun reach a build script or test. Three settings
  # hold it: the shell's stdenv has no compiler, devenv adds no Apple SDK, and
  # the Rust toolchain stops propagating stdenv's clang-wrapper (it would put
  # it first on PATH and export CC and NIX_LDFLAGS). The toolchain is changed
  # after devenv assembles its pinned components, so no second one is built.
  # Linux keeps stdenv's compiler, which links what rustc builds there.
  imports = [
    {
      options.languages.rust.toolchainPackage = lib.mkOption {
        apply = toolchain:
          if pkgs.stdenv.isDarwin
          then
            toolchain.overrideAttrs (previous: {
              propagatedBuildInputs = lib.remove pkgs.stdenv.cc (previous.propagatedBuildInputs or []);
              depsHostHostPropagated = lib.remove pkgs.stdenv.cc (previous.depsHostHostPropagated or []);
            })
          else toolchain;
      };
    }
  ];
  stdenv =
    if pkgs.stdenv.isDarwin
    then pkgs.stdenvNoCC
    else pkgs.stdenv;
  apple.sdk = null;

  overlays = [inputs.rust-overlay.overlays.default];

  # One stable release, so a toolchain change is a deliberate commit: the
  # release the mbx units in review were verified with. CI keeps floating to
  # its runner's newest stable (.github/workflows/ci.yml).
  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.95.0";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-src"
    ];
    # The wasm end-to-end test builds for it (AGENTS.md, "Before Pushing").
    targets = ["wasm32-unknown-unknown"];
  };

  packages = [
    # crates/mbx/tests/nextest_scheduling.rs drives cargo-nextest's runner.
    pkgs.cargo-nextest
    # aws-lc-sys falls back to CMake where its cc build does not apply.
    pkgs.cmake
    # test/setup.bats drives mise's postinstall hook and command wrappers, as
    # CI runs the suite under jdx/mise-action.
    pkgs.mise
  ];

  # Builder-only variables the shell derivation would export: they change what
  # a compiler run from the shell produces. The shell runs tools; it is not a
  # Nix builder. devenv's own list is named explicitly: languages.rust defines
  # unsetEnvVars at normal priority, which would drop every default beside it,
  # this list's included, and leave NIX_CFLAGS_COMPILE and
  # MACOSX_DEPLOYMENT_TARGET in every build.
  unsetEnvVars =
    options.unsetEnvVars.default
    ++ [
      "CONFIG_SHELL"
      "DETERMINISTIC_BUILD"
      "IN_NIX_SHELL"
      "MACOSX_DEPLOYMENT_TARGET"
      "NIX_CFLAGS_COMPILE"
      "NIX_DONT_SET_RPATH"
      "NIX_DONT_SET_RPATH_FOR_BUILD"
      "NIX_ENFORCE_NO_NATIVE"
      "NIX_IGNORE_LD_THROUGH_GCC"
      "NIX_NO_SELF_RPATH"
      "NIX_STORE"
      "SOURCE_DATE_EPOCH"
      "__darwinAllowLocalNetworking"
      "__impureHostDeps"
      "__propagatedImpureHostDeps"
      "__propagatedSandboxProfile"
      "__sandboxProfile"
      "cmakeFlags"
      "configureFlags"
      "mesonFlags"
      "name"
      "system"
    ];
}
