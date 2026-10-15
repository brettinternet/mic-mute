#!/usr/bin/env bash
# shellcheck source-path=SCRIPTDIR
#
# Stops Mic Mute (SIGTERM restores the microphones), then removes the app and
# its Launch at Login agent. Settings stay unless --purge is given.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/uninstall.sh [options]    (or: task uninstall -- [options])

Stops Mic Mute, then removes "Mic Mute.app" from /Applications and
~/Applications and the LaunchAgent
~/Library/LaunchAgents/com.brettinternet.mic-mute.plist.

Options:
  --purge    also delete the settings in ~/Library/Application Support/mic-mute
  --dry-run  run only the read-only checks; print every other command
  -h, --help show this help

Testing only: MIC_MUTE_INSTALL_DIR (absolute) replaces both app directories,
MIC_MUTE_LAUNCH_AGENT the LaunchAgent path, and MIC_MUTE_SETTINGS_DIR the
settings directory.
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/common.sh"

readonly SETTINGS_DIR="${MIC_MUTE_SETTINGS_DIR:-$HOME/Library/Application Support/mic-mute}"

PURGE=0

parse_args() {
  while (($# > 0)); do
    case "$1" in
      --purge) PURGE=1 ;;
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

# Deletes the bundle at $1 if it is Mic Mute; leaves any other app alone.
remove_bundle() {
  local bundle="$1" id
  if [[ ! -e "$bundle" ]]; then
    info "Not installed: $bundle"
    return 0
  fi
  id="$(bundle_identifier "$bundle")"
  if [[ "$id" != "$BUNDLE_ID" ]]; then
    warn "skipping $bundle: it is a different app (bundle id: ${id:-none})"
    return 0
  fi
  run rm -rf "$bundle" || die "couldn't delete $bundle"
}

remove_apps() {
  step "Removing the app"
  if [[ -n "${MIC_MUTE_INSTALL_DIR:-}" ]]; then
    [[ "$MIC_MUTE_INSTALL_DIR" == /* ]] || die "MIC_MUTE_INSTALL_DIR must be an absolute path"
    remove_bundle "${MIC_MUTE_INSTALL_DIR%/}/$BUNDLE_NAME"
  else
    remove_bundle "/Applications/$BUNDLE_NAME"
    remove_bundle "$HOME/Applications/$BUNDLE_NAME"
  fi
}

remove_launch_agent() {
  step "Removing Launch at Login"
  if [[ -e "$LAUNCH_AGENT" ]]; then
    run rm -f "$LAUNCH_AGENT"
  else
    info "Off: $LAUNCH_AGENT doesn't exist"
  fi
}

remove_settings() {
  step "Settings"
  if [[ ! -e "$SETTINGS_DIR" ]]; then
    info "None: $SETTINGS_DIR doesn't exist"
  elif ((PURGE)); then
    run rm -rf "$SETTINGS_DIR"
  else
    info "Kept $SETTINGS_DIR (delete it with --purge)"
  fi
}

main() {
  parse_args "$@"
  require_macos_user
  if ((DRY_RUN)); then
    info "Dry run: only read-only checks run; every other command is printed"
  fi

  step "Stopping the running app"
  stop_app
  remove_apps
  remove_launch_agent
  remove_settings

  step "Done"
  if ((DRY_RUN)); then
    info "Dry run finished; nothing was changed"
  fi
}

# Run only when executed, so tests can source the functions.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
