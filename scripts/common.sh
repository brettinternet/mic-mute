# shellcheck shell=bash
#
# Helpers shared by scripts/install.sh and scripts/uninstall.sh. Source it; don't run it.
#
# Testing-only overrides. They keep the real app, login item, and settings untouched:
#   MIC_MUTE_INSTALL_DIR   absolute directory that holds "Mic Mute.app", replacing
#                          /Applications and ~/Applications
#   MIC_MUTE_LAUNCH_AGENT  LaunchAgent plist path
#   MIC_MUTE_SETTINGS_DIR  settings directory that `uninstall.sh --purge` deletes

# shellcheck disable=SC2034 # used by the scripts that source this file
readonly BUNDLE_NAME="Mic Mute.app"
readonly BUNDLE_ID="com.brettinternet.mic-mute"
readonly EXECUTABLE="mic-mute"
readonly PLIST_BUDDY="/usr/libexec/PlistBuddy"
# The app restores the microphones within ~100 ms of SIGTERM.
readonly STOP_TIMEOUT_SECONDS=10
# shellcheck disable=SC2034 # used by the scripts that source this file
readonly LAUNCH_AGENT="${MIC_MUTE_LAUNCH_AGENT:-$HOME/Library/LaunchAgents/$BUNDLE_ID.plist}"

DRY_RUN=0
# shellcheck disable=SC2034 # set by stop_app, read by install.sh
APP_WAS_RUNNING=0

step() { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

# Quotes one word for display, so a printed command can be pasted into a shell.
quote_word() {
  case "$1" in
    "" | *[!A-Za-z0-9_./:=@%+,-]*)
      printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
      ;;
    *) printf '%s' "$1" ;;
  esac
}

print_cmd() {
  local line="+" word
  for word in "$@"; do
    line="$line $(quote_word "$word")"
  done
  info "$line"
}

# Prints a command, then runs it unless --dry-run is set.
run() {
  print_cmd "$@"
  if ((DRY_RUN)); then
    return 0
  fi
  "$@"
}

require_macos_user() {
  [[ "$(uname -s)" == Darwin ]] || die "Mic Mute runs on macOS only"
  [[ "$EUID" -ne 0 ]] || die "run this as your own user, not root"
}

# Whether Mic Mute keeps Launch at Login as an SMAppService login item (macOS 13
# and later, listed in System Settings > General > Login Items). macOS 11 and 12
# use the LaunchAgent, which is also the 13+ fallback when registering fails.
uses_login_item() {
  local version
  version="$(sw_vers -productVersion)"
  [[ "${version%%.*}" =~ ^[0-9]+$ ]] && ((${version%%.*} >= 13))
}

# Prints the value at plist key path $2 (e.g. "ProgramArguments:0") of file $1, or nothing.
plist_value() {
  "$PLIST_BUDDY" -c "Print :$2" "$1" 2>/dev/null || true
}

bundle_identifier() {
  plist_value "$1/Contents/Info.plist" CFBundleIdentifier
}

running_pids() {
  pgrep -x -u "$(id -u)" "$EXECUTABLE" | tr '\n' ' ' || true
}

# Stops this user's running Mic Mute with SIGTERM and waits for it to exit. Its
# SIGTERM handler restores the microphones; SIGKILL would leave them muted, so
# a hung app is reported instead of killed.
stop_app() {
  local pids status=0 deadline
  pids="$(running_pids)"
  if [[ -z "$pids" ]]; then
    info "Mic Mute is not running"
    return 0
  fi
  # shellcheck disable=SC2034 # read by install.sh
  APP_WAS_RUNNING=1
  info "Mic Mute is running (pid ${pids% }); SIGTERM makes it restore the microphones and exit"
  # pkill exits 1 when nothing matched: the app quit in the meantime.
  run pkill -TERM -x -u "$(id -u)" "$EXECUTABLE" || status=$?
  if ((status > 1)); then
    die "pkill failed with exit status $status"
  fi
  if ((DRY_RUN)); then
    return 0
  fi
  deadline=$((SECONDS + STOP_TIMEOUT_SECONDS))
  while [[ -n "$(running_pids)" ]]; do
    if ((SECONDS >= deadline)); then
      die "Mic Mute is still running ${STOP_TIMEOUT_SECONDS}s after SIGTERM;" \
        "quit it from the menu bar (Exit) and run this again"
    fi
    sleep 0.2
  done
  info "Mic Mute exited"
}
