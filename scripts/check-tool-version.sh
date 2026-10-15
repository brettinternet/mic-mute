#!/usr/bin/env bash
#
# Usage: scripts/check-tool-version.sh BINARY VERSION
#
# Exits 0 when BINARY in cargo's install directory reports VERSION in its
# `--version` output ("cargo-bundle v0.12.0" and "watchexec 2.8.0" both match).
# The Taskfile `status:` checks and scripts/install.sh use it to decide whether
# a pinned `cargo install` must run.

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 BINARY VERSION" >&2
  exit 2
fi

binary="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/$1"
expected="$2"

[[ -x "$binary" ]] || exit 1
output="$("$binary" --version 2>/dev/null)" || exit 1

read -r -a words <<<"$output"
for word in ${words[@]+"${words[@]}"}; do
  if [[ "${word#v}" == "$expected" ]]; then
    exit 0
  fi
done
exit 1
