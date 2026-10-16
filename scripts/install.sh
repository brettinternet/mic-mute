#!/usr/bin/env bash
# shellcheck source-path=SCRIPTDIR
#
# Builds, signs, and installs Mic Mute.app, then launches it.
# Run it again to update: it stops the running app with SIGTERM (which restores
# the microphones), replaces the bundle atomically, and repoints the Launch at
# Login agent at the installed binary.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [options]      (or: task install -- [options])

Checks, builds, ad-hoc signs, and installs "Mic Mute.app" to /Applications,
then launches it.

Options:
  --user         install to ~/Applications instead of /Applications
  --universal    build one app for Apple silicon and Intel Macs (two builds,
                 combined with lipo); the default builds for this Mac only
  --skip-checks  skip the cargo fmt, clippy, and test gates
  --no-launch    don't start the app after installing
  --bundle-only  stop after building, signing, and verifying the bundle
  --dry-run      run only the read-only checks; print every other command
  -h, --help     show this help

Testing only: MIC_MUTE_INSTALL_DIR (absolute) replaces the destination
directory and MIC_MUTE_LAUNCH_AGENT the LaunchAgent plist path.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/common.sh"
source "$SCRIPT_DIR/cargo-tools.env"

readonly ARM64_TARGET="aarch64-apple-darwin"
readonly X86_64_TARGET="x86_64-apple-darwin"
# cargo bundle names the output directory of a bundle built for several targets.
readonly UNIVERSAL_DIR="universal"
readonly MIN_MACOS_MAJOR=11
readonly LAUNCH_TIMEOUT_SECONDS=15
# Name of the replaced bundle inside the staging directory, until it's deleted.
readonly PREVIOUS_NAME="previous.app"

USER_SCOPE=0
UNIVERSAL=0
# The Rust targets to build: this Mac's, or both with --universal.
TARGETS=()
SKIP_CHECKS=0
NO_LAUNCH=0
BUNDLE_ONLY=0
BUNDLE=""
DEST_DIR=""
DEST=""
STAGING_DIR=""

parse_args() {
  while (($# > 0)); do
    case "$1" in
      --user) USER_SCOPE=1 ;;
      --universal) UNIVERSAL=1 ;;
      --skip-checks) SKIP_CHECKS=1 ;;
      --no-launch) NO_LAUNCH=1 ;;
      --bundle-only) BUNDLE_ONLY=1 ;;
      --dry-run) DRY_RUN=1 ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage >&2
        die "unknown option: $1"
        ;;
    esac
    shift
  done
}

# The Rust target of this Mac's CPU. sysctl reports Apple silicon also to a shell
# that runs under Rosetta, so the app isn't built for translation by mistake.
native_target() {
  if [[ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" == 1 ]]; then
    printf '%s\n' "$ARM64_TARGET"
  else
    printf '%s\n' "$X86_64_TARGET"
  fi
}

# The architecture name lipo reports for a Rust target.
lipo_arch() {
  case "$1" in
    "$ARM64_TARGET") printf 'arm64\n' ;;
    "$X86_64_TARGET") printf 'x86_64\n' ;;
    *) die "no Mach-O architecture is known for $1" ;;
  esac
}

check_platform() {
  local version native
  require_macos_user
  version="$(sw_vers -productVersion)"
  ((${version%%.*} >= MIN_MACOS_MAJOR)) ||
    die "macOS $MIN_MACOS_MAJOR or later is required (found $version)"
  native="$(native_target)"
  if [[ "$native" == "$ARM64_TARGET" ]]; then
    info "macOS $version on Apple silicon"
  else
    info "macOS $version on an Intel Mac"
  fi
  if ((UNIVERSAL)); then
    TARGETS=("$ARM64_TARGET" "$X86_64_TARGET")
    info "Building a universal app for Apple silicon and Intel Macs"
  else
    TARGETS=("$native")
    info "Building for this Mac ($native); --universal adds the other architecture"
  fi
}

check_developer_tools() {
  local developer_dir tool
  developer_dir="$(xcode-select -p 2>/dev/null || true)"
  [[ -n "$developer_dir" && -d "$developer_dir" ]] ||
    die "the Xcode Command Line Tools are missing; install them with: xcode-select --install"
  info "Developer tools: $developer_dir"
  for tool in codesign ditto lipo plutil pgrep pkill open; do
    command -v "$tool" >/dev/null || die "$tool not found"
  done
  [[ -x "$PLIST_BUDDY" ]] || die "$PLIST_BUDDY not found"
}

check_rust() {
  local installed host target
  command -v cargo >/dev/null ||
    die "cargo not found; install Rust with rustup (https://rustup.rs) or run: mise install"
  info "$(cargo --version)"
  if command -v rustup >/dev/null; then
    installed="$(rustup target list --installed)"
    for target in "${TARGETS[@]}"; do
      if grep -qxF "$target" <<<"$installed"; then
        info "Rust target $target is installed"
      else
        run rustup target add "$target"
      fi
    done
  else
    host="$(rustc -vV | sed -n 's/^host: //p')"
    for target in "${TARGETS[@]}"; do
      [[ "$host" == "$target" ]] ||
        die "rustc builds for ${host:-an unknown host}, and rustup isn't available to add $target"
    done
    info "Rust host target is $host"
  fi
  if ((!SKIP_CHECKS)); then
    cargo fmt --version >/dev/null 2>&1 ||
      die "rustfmt is missing; run: rustup component add rustfmt"
    cargo clippy --version >/dev/null 2>&1 ||
      die "clippy is missing; run: rustup component add clippy"
  fi
}

ensure_cargo_bundle() {
  if "$SCRIPT_DIR/check-tool-version.sh" cargo-bundle "$CARGO_BUNDLE_VERSION"; then
    info "cargo-bundle $CARGO_BUNDLE_VERSION is installed"
  else
    info "Installing cargo-bundle $CARGO_BUNDLE_VERSION; the first build takes several minutes"
    run cargo install --locked "cargo-bundle@$CARGO_BUNDLE_VERSION"
  fi
}

# Picks the destination and refuses to replace an app that isn't Mic Mute.
check_destination() {
  local id
  if [[ -n "${MIC_MUTE_INSTALL_DIR:-}" ]]; then
    [[ "$MIC_MUTE_INSTALL_DIR" == /* ]] || die "MIC_MUTE_INSTALL_DIR must be an absolute path"
    DEST_DIR="${MIC_MUTE_INSTALL_DIR%/}"
  elif ((USER_SCOPE)); then
    DEST_DIR="$HOME/Applications"
  else
    DEST_DIR="/Applications"
  fi
  DEST="$DEST_DIR/$BUNDLE_NAME"
  if [[ -d "$DEST_DIR" && ! -w "$DEST_DIR" ]]; then
    die "$DEST_DIR is not writable by $(id -un); install to ~/Applications with --user"
  fi
  if [[ -e "$DEST" ]]; then
    id="$(bundle_identifier "$DEST")"
    [[ "$id" == "$BUNDLE_ID" ]] ||
      die "$DEST is a different app (bundle id: ${id:-none}); move it away first"
    info "Will replace $DEST"
  else
    info "Will install to $DEST"
  fi
}

preflight() {
  step "Preflight"
  check_platform
  check_developer_tools
  check_rust
  ensure_cargo_bundle
  if ((!BUNDLE_ONLY)); then
    check_destination
  fi
}

run_checks() {
  if ((SKIP_CHECKS)); then
    step "Skipping the checks (--skip-checks)"
    return 0
  fi
  step "Checks"
  run cargo fmt -- --check
  run cargo clippy --locked --release -- -D warnings
  run cargo test --locked
}

# Respects CARGO_TARGET_DIR and build.target-dir, like cargo bundle itself.
cargo_target_dir() {
  local metadata dir
  metadata="$(cargo metadata --no-deps --format-version 1)" || die "cargo metadata failed"
  dir="$(sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' <<<"$metadata")"
  [[ "$dir" == /* ]] || die "couldn't read cargo's target directory from cargo metadata"
  printf '%s\n' "$dir"
}

verify_info_plist() {
  local plist="$BUNDLE/Contents/Info.plist" id
  plutil -lint -s "$plist" || die "$plist is not a valid property list"
  id="$(plist_value "$plist" CFBundleIdentifier)"
  [[ "$id" == "$BUNDLE_ID" ]] || die "$plist: CFBundleIdentifier is '${id}', expected $BUNDLE_ID"
  [[ -n "$(plist_value "$plist" NSCameraUsageDescription)" ]] ||
    die "$plist lacks NSCameraUsageDescription; check info_plist_exts in Cargo.toml"
  info "Info.plist: $id $(plist_value "$plist" CFBundleShortVersionString)," \
    "macOS $(plist_value "$plist" LSMinimumSystemVersion) or later"
}

# Checks that the bundled executable holds exactly one slice per built target.
verify_architectures() {
  local executable="$BUNDLE/Contents/MacOS/$EXECUTABLE" archs target
  local -a slices
  archs="$(lipo -archs "$executable")" || die "lipo couldn't read $executable"
  read -r -a slices <<<"$archs"
  ((${#slices[@]} == ${#TARGETS[@]})) ||
    die "$executable has the architectures '$archs', expected one per target: ${TARGETS[*]}"
  for target in "${TARGETS[@]}"; do
    [[ " $archs " == *" $(lipo_arch "$target") "* ]] ||
      die "$executable lacks $(lipo_arch "$target") (lipo -archs: $archs)"
  done
  info "Architectures: $archs"
}

build_bundle() {
  local target_dir target out_dir binary
  local -a slices=() bundle_targets=()
  target_dir="$(cargo_target_dir)"
  step "Building $BUNDLE_NAME"
  for target in "${TARGETS[@]}"; do
    run cargo build --locked --release --target "$target"
    slices+=("$target_dir/$target/release/$EXECUTABLE")
    bundle_targets+=(--target "$target")
  done
  if ((UNIVERSAL)); then
    # cargo bundle writes a bundle for several targets under target/universal and can lipo
    # by itself, but only after building without --locked; combine the builds above instead.
    out_dir="$target_dir/$UNIVERSAL_DIR/release"
    binary="$out_dir/$EXECUTABLE"
    if [[ ! -d "$out_dir" ]]; then
      run mkdir -p "$out_dir"
    fi
    run lipo -create -output "$binary" "${slices[@]}"
  else
    out_dir="$target_dir/${TARGETS[0]}/release"
    binary="${slices[0]}"
  fi
  BUNDLE="$out_dir/bundle/osx/$BUNDLE_NAME"
  # --binary-path bundles the --locked build above instead of rebuilding without --locked.
  run cargo bundle --release "${bundle_targets[@]}" --format osx --binary-path "$binary"
  if ((!DRY_RUN)); then
    verify_info_plist
    verify_architectures
  fi

  step "Signing (ad hoc)"
  run codesign --force --deep --sign - "$BUNDLE"
  run codesign --verify --deep --strict --verbose=2 "$BUNDLE"
}

stop_running_app() {
  step "Stopping the running app"
  stop_app
}

# Moves a failed install's previous bundle back, then deletes the staging directory.
cleanup() {
  local previous
  [[ -n "$STAGING_DIR" && -d "$STAGING_DIR" ]] || return 0
  previous="$STAGING_DIR/$PREVIOUS_NAME"
  if [[ -e "$previous" && ! -e "$DEST" ]]; then
    if mv "$previous" "$DEST"; then
      warn "the install failed; restored the previous $DEST"
    else
      warn "the install failed; the previous app is at $previous"
      return 0
    fi
  fi
  rm -rf "$STAGING_DIR"
}

# Copies the bundle next to the destination, then swaps it in with renames on
# the same volume, so the destination is never a partial copy.
install_bundle() {
  local template
  step "Installing $DEST"
  if [[ ! -d "$DEST_DIR" ]]; then
    run mkdir -p "$DEST_DIR"
  fi
  template="$DEST_DIR/.mic-mute-install.XXXXXX"
  print_cmd mktemp -d "$template"
  if ((DRY_RUN)); then
    STAGING_DIR="$template"
  else
    STAGING_DIR="$(mktemp -d "$template")"
  fi
  run ditto "$BUNDLE" "$STAGING_DIR/$BUNDLE_NAME"
  run codesign --verify --deep --strict "$STAGING_DIR/$BUNDLE_NAME"
  if [[ -e "$DEST" ]]; then
    run mv "$DEST" "$STAGING_DIR/$PREVIOUS_NAME"
  fi
  if ((!DRY_RUN)) && [[ -e "$DEST" ]]; then
    die "$DEST reappeared during the install"
  fi
  run mv "$STAGING_DIR/$BUNDLE_NAME" "$DEST"
  run rm -rf "$STAGING_DIR"
  STAGING_DIR=""
}

# Quotes a PlistBuddy command argument; PlistBuddy parses quotes and backslashes
# itself. sed, because bash 3.2 and 5 differ on quotes in ${var//…/…}.
plist_buddy_quote() {
  printf '"%s"' "$(printf '%s' "$1" | sed "s/[\\\\\"']/\\\\&/g")"
}

# The app writes the LaunchAgent only when Launch at Login is toggled, so point
# an existing one at the installed binary. On macOS 13 and later the app keeps
# a login item instead, which follows the app by itself, and replaces this
# LaunchAgent with it when it starts.
update_launch_agent() {
  local exe="$DEST/Contents/MacOS/$EXECUTABLE" current
  step "Launch at Login"
  if [[ ! -f "$LAUNCH_AGENT" ]]; then
    if uses_login_item; then
      info "No LaunchAgent to update; on macOS 13 and later, Mic Mute keeps its own login item"
    else
      info "Off: $LAUNCH_AGENT doesn't exist"
    fi
    return 0
  fi
  if uses_login_item; then
    info "Mic Mute replaces this LaunchAgent with a login item when it starts"
  fi
  current="$(plist_value "$LAUNCH_AGENT" ProgramArguments:0)"
  if [[ "$current" == "$exe" ]]; then
    info "The LaunchAgent already starts $exe"
    return 0
  fi
  info "The LaunchAgent starts ${current:-nothing}; repointing it"
  run "$PLIST_BUDDY" -c "Set :ProgramArguments:0 $(plist_buddy_quote "$exe")" "$LAUNCH_AGENT" ||
    die "couldn't update $LAUNCH_AGENT"
  run plutil -lint "$LAUNCH_AGENT"
  if ((DRY_RUN)); then
    return 0
  fi
  current="$(plist_value "$LAUNCH_AGENT" ProgramArguments:0)"
  [[ "$current" == "$exe" ]] ||
    die "$LAUNCH_AGENT starts '$current' after the update, expected $exe"
}

# Opens the installed bundle by path (a name could pick another copy) and waits
# until a process runs its executable.
launch_app() {
  local exe="$DEST/Contents/MacOS/$EXECUTABLE" physical_exe path pid deadline
  step "Launching Mic Mute"
  run open -a "$DEST"
  if ((DRY_RUN)); then
    return 0
  fi
  # ps shows the path the process was started with, symlinks resolved or not.
  physical_exe="$(cd "$DEST/Contents/MacOS" && pwd -P)/$EXECUTABLE"
  deadline=$((SECONDS + LAUNCH_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    for pid in $(running_pids); do
      path="$(ps -o comm= -p "$pid" 2>/dev/null || true)"
      if [[ "$path" == "$exe" || "$path" == "$physical_exe" ]]; then
        info "Mic Mute is running (pid $pid)"
        return 0
      fi
    done
    sleep 0.2
  done
  die "Mic Mute didn't start within ${LAUNCH_TIMEOUT_SECONDS}s;" \
    "check Console.app for a crash report"
}

warn_other_copy() {
  local other="$HOME/Applications/$BUNDLE_NAME"
  [[ -z "${MIC_MUTE_INSTALL_DIR:-}" ]] || return 0
  if ((USER_SCOPE)); then
    other="/Applications/$BUNDLE_NAME"
  fi
  if [[ -e "$other" && "$(bundle_identifier "$other")" == "$BUNDLE_ID" ]]; then
    warn "another copy is installed at $other; delete it so it isn't opened by mistake"
  fi
}

main() {
  parse_args "$@"
  cd "$REPO_ROOT"
  if ((DRY_RUN)); then
    info "Dry run: only read-only checks run; every other command is printed"
  fi
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  preflight
  run_checks
  build_bundle
  if ((BUNDLE_ONLY)); then
    step "Done"
    info "Signed bundle: $BUNDLE"
    return 0
  fi

  stop_running_app
  install_bundle
  update_launch_agent
  if ((NO_LAUNCH)); then
    step "Not launching (--no-launch)"
    if ((APP_WAS_RUNNING)); then
      info "Mic Mute was stopped; start it with: open -a $(quote_word "$DEST")"
    fi
  else
    launch_app
  fi
  warn_other_copy

  step "Done"
  if ((DRY_RUN)); then
    info "Dry run finished; nothing was changed"
  else
    info "Installed $DEST ($(plist_value "$DEST/Contents/Info.plist" CFBundleShortVersionString))"
  fi
}

# Run only when executed, so tests can source the functions.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
