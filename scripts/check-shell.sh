#!/usr/bin/env bash
# The repository shell's Darwin invariant (devenv.nix): /usr/bin/cc, Xcode's SDK
# and /usr/bin/xcrun answer, and no Nix cc-wrapper, its flags, a store SDKROOT or
# DEVELOPER_DIR, or xcbuild's xcrun reach a build script or test. A link driven
# by a Nix cc-wrapper is one mbx refuses to cache across checkouts, so a leak
# turns every cross-checkout link in the suite into a bypass. Linux keeps
# stdenv's compiler, and the invariant names Darwin only. The environment a build
# gets is the shell's with the caller's laid over it (smoothbricks
# specs/cowshed/11_shell.md, "Exec hosts"), so a leak names either source.
set -euo pipefail
if [ "$(uname -s)" != Darwin ]; then
  echo "check-shell: the shell's compiler invariant is Darwin's; $(uname -s) keeps stdenv's compiler"
  exit 0
fi
leaks=()
for tool in cc c++ clang clang++ ld xcrun; do
  path=$(command -v "$tool" || true)
  case "$path" in
    /nix/store/*) leaks+=("$tool resolves to $path, ahead of /usr/bin") ;;
  esac
done
for name in NIX_LDFLAGS NIX_CFLAGS_COMPILE NIX_CC NIX_BINTOOLS CC CXX; do
  if [ -n "${!name-}" ]; then
    leaks+=("$name is set to ${!name}")
  fi
done
for name in SDKROOT DEVELOPER_DIR; do
  case "${!name-}" in
    /nix/store/*) leaks+=("$name is the store path ${!name}") ;;
  esac
done
if [ "${#leaks[@]}" -ne 0 ]; then
  echo "check-shell: Nix compiler tooling reaches builds on Darwin, from this shell (devenv.nix) or from the caller's environment laid over it:" >&2
  printf '  %s\n' "${leaks[@]}" >&2
  exit 1
fi
echo "check-shell: cc, clang, ld and xcrun answer from the host; no Nix compiler flags or store SDK"