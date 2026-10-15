<p align="center">
  <img width="128" src="./assets/icons/128x128@2x.png" style="padding:0.5rem;">
</p>

<h1 align="center">Mic Mute for macOS</h1>

A system-wide mute for macOS microphones with a global shortcut and visual confirmation of mute status. Inspired by [VCM](https://learn.microsoft.com/en-us/windows/powertoys/video-conference-mute) for Windows.

![popup window screenshot indicating the microphone is off](./screenshot.png)

Mute with <kbd>Cmd</kbd> <kbd>Shift</kbd> <kbd>M</kbd> or from the system tray dropdown, or switch the shortcut to [push-to-talk](./docs/settings.md#mode): hold it to talk. See the [settings documentation](./docs/settings.md) for the settings location, defaults, and available options.

## Features

- CoreAudio API mute input devices
  - [x] Mute input devices
    - Note: If native CoreAudio mute is unavailable or ignores changes, Mic Mute falls back to input volume controls, including virtual main volume. Devices exposing neither are skipped.
  - [x] Provide global hotkey muting
  - [x] Push-to-talk: hold the shortcut to unmute, release it to mute again
  - [x] Optionally mute at launch, when the screen locks, or when the Mac or its displays sleep; nothing unmutes automatically
  - [x] Mute a newly connected microphone, and undo another app's unmute, as soon as CoreAudio reports the change, while microphones should be off; a regular poll catches what CoreAudio doesn't report
- Visual confirmation of mute status
  - [x] Show microphone mute status in system tray: a monochrome icon that follows light and dark menu bars, red while muted
  - [x] Show microphone mute status in small popup window
  - [x] Popup window shouldn't appear in screenshots or recordings and ignores mouse events
  - [x] Popup follows screens and monitors with cursor
  - [x] List each microphone and its state in the tray's Microphones menu; uncheck one there to [exclude it](./docs/settings.md#excluded_devices)
- [x] Report whether a camera is in use: a dot on the tray icon (its tooltip says "Camera in use") and a label in the popup (disabling cameras appears to be impossible or too difficult for the scope of this project)
- [x] Add configurable settings (hotkey, startup, popup visibility)
- [x] Open app at login: a login item on macOS 13 and later, a LaunchAgent on macOS 11 and 12 (see [Launch at Login](#launch-at-login))
- [x] Control from other apps and scripts through `com.brettinternet.mic-mute://` URLs

## Control from other apps

Mic Mute handles `com.brettinternet.mic-mute://` URLs, so other apps and scripts can control it:

| URL | Effect |
| --- | --- |
| `com.brettinternet.mic-mute://mute` | Mute every microphone |
| `com.brettinternet.mic-mute://status` | Show the popup with the current state for two seconds, also when `show_popup` is off |
| `com.brettinternet.mic-mute://unmute` | Unmute every microphone; needs `allow_url_unmute` |
| `com.brettinternet.mic-mute://toggle` | Unmute when muted, mute otherwise; needs `allow_url_unmute` |

Any app, and any web page through the browser, can open these URLs. So `unmute` and `toggle` run only after you set `"allow_url_unmute": true` in the [settings](./docs/settings.md#allow_url_unmute); until then Mic Mute refuses them and logs a warning. Action names are case-insensitive, and a trailing `/`, a query (`?…`), or a fragment (`#…`) is ignored. A URL with an unknown action does nothing but log a warning.

- **Scripts and the terminal:** `open "com.brettinternet.mic-mute://mute"`
- **Shortcuts.app:** add an **Open URLs** action with the URL, such as `com.brettinternet.mic-mute://mute`, and run the shortcut from the menu bar, a keyboard shortcut, or Siri.
- **Raycast:** create a Quicklink whose link is the URL.

macOS hands these URLs only to an installed app bundle (`task install` or a release). If Mic Mute isn't running, opening a URL starts it, and the action runs once it has started.

## Limitations

Mic Mute is best-effort, **not** a hardware privacy switch.

- Mutes CoreAudio-controllable devices only.
- Skips devices without mute/volume controls, such as iPhone Continuity Microphone.
- Skips devices that accept mute and volume changes but ignore them, such as Microsoft Teams Audio, with a warning in the log. Mic Mute tries such a device again every minute and when it reconnects; until it responds, the tray and popup show the state of the other microphones.
- Never mutes the devices unchecked in the tray's Microphones menu (the [`excluded_devices`](./docs/settings.md#excluded_devices) setting), and leaves them out of the mute state: an excluded microphone may be live while the tray and popup show muted.
- A change CoreAudio doesn't report, such as a microphone that gains its input channels only after it appears, waits for the next poll, which can leave a mute gap of up to a second (200 ms while a CoreAudio listener can't be registered).
- Drivers can lie; use hardware mute, unplug, or macOS permissions for high assurance.
- Quitting restores the microphones Mic Mute muted: tray → Exit, Dock → Quit, <kbd>Cmd</kbd> <kbd>Q</kbd>, logging out, `osascript -e 'quit app "Mic Mute"'`, `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGQUIT`, and internal errors (panics). After `kill -9`, a crash, or power loss, the next launch restores them instead; a microphone unplugged by then is restored when it reconnects.
- Microphones without native mute are muted by setting their input volume to 0. Unmuting restores the level they had; a microphone Mic Mute has never seen audible comes back at 50%.

## Launch at Login

Turn it on from the tray menu, or with the [`launch_at_login`](./docs/settings.md#launch_at_login) setting.

- **macOS 13 and later:** Mic Mute registers itself as a login item with `SMAppService`. It's listed in System Settings → General → Login Items under Open at Login, where you can also remove it, and macOS finds the app wherever you move it. If the login item can't be registered, Mic Mute logs a warning and uses a LaunchAgent instead.
- **macOS 11 and 12:** Mic Mute writes the LaunchAgent `~/Library/LaunchAgents/com.brettinternet.mic-mute.plist` and points it at the app's current location at each launch.
- **Upgrading** on macOS 13 and later: the first launch replaces the LaunchAgent an earlier version wrote with a login item, then deletes the plist. While the login item can't be registered or waits for approval, the LaunchAgent stays.

macOS (Background Task Management) records a login item against the app's code signature, and these builds are signed ad hoc, not with a Developer ID:

- macOS may notify you that a login item was added, and Login Items may list entries for earlier builds until macOS cleans up its records.
- Every `task install` signs a new build, and macOS may stop opening a re-signed build at login while Login Items still shows it. If Mic Mute doesn't open at your next login after a reinstall, turn Launch at Login off and on in the tray: that registers the new build.
- If macOS waits for your approval, the tray item reads "Launch at Login (Approve in System Settings)": allow Mic Mute in System Settings → General → Login Items.
- Only an app bundle in a stable place, such as `/Applications`, changes the login item. A copy opened from a download without moving it (App Translocation) and a development build from `cargo run` leave it alone.
- Deleting the app doesn't remove its login item: turn Launch at Login off first, or remove Mic Mute in Login Items afterwards.
- `sudo sfltool dumpbtm` prints the records macOS keeps (search for `com.brettinternet.mic-mute`); without `sudo` it waits for an administrator to authorize it.

## Releases

I have not elected to sign the app by joining the Apple Developer Program. The releases have been self-signed by me and can be installed by bypassing the typical app security on macOS. You're also welcome to build and bundle the app yourself with the simple instructions described below.

[View releases](https://github.com/brettinternet/mic-mute/releases)

Pushing a version tag that matches `Cargo.toml` (for example `0.6.0`) runs the [Release workflow](./.github/workflows/release.yaml) on a macOS runner: the `task install` checks, an ad-hoc signed build, and a GitHub release with a DMG, `SHA256SUMS`, and a build provenance attestation. Running the workflow by hand with an existing tag republishes that release; without a tag, it only uploads the files as a workflow artifact. These builds aren't notarized, so macOS blocks the first launch until you choose **Open Anyway** in System Settings → Privacy & Security. To verify a download:

```sh
shasum -a 256 -c SHA256SUMS
gh attestation verify mic-mute-<version>-aarch64-apple-darwin.dmg --repo <owner>/mic-mute
```

## Build

Install dependencies with `mise`. Or you may [install Rust](https://www.rust-lang.org/tools/install). Then, [install go-task](https://taskfile.dev/installation/) or run the commands from the Taskfile separately yourself.

### Install

Build the app, install it to `/Applications`, and launch it:

```sh
task install
```

[`scripts/install.sh`](./scripts/install.sh) checks for macOS 11 or later on Apple silicon, the Xcode Command Line Tools, and the `aarch64-apple-darwin` Rust target, and installs the pinned cargo-bundle. It runs `cargo fmt --check`, clippy, and the tests, bundles and ad-hoc signs `Mic Mute.app`, and stops a running Mic Mute with SIGTERM, which restores the microphones. Then it replaces the installed copy atomically, points an existing Launch at Login LaunchAgent at it (on macOS 13 and later, the app replaces that with a login item when it starts), and launches it. Run it again to update. A locally built app isn't quarantined, so Gatekeeper doesn't block it.

Pass flags after `--`, for example `task install -- --user`:

| Flag | Effect |
| --- | --- |
| `--user` | Install to `~/Applications` instead of `/Applications` |
| `--skip-checks` | Skip the fmt, clippy, and test gates |
| `--no-launch` | Don't start the app after installing |
| `--bundle-only` | Stop after building, signing, and verifying the bundle |
| `--dry-run` | Run only the read-only checks; print every other command |

### Uninstall

```sh
task uninstall              # stop the app; remove it and its LaunchAgent
task uninstall -- --purge   # also delete the settings
```

[`scripts/uninstall.sh`](./scripts/uninstall.sh) stops Mic Mute with SIGTERM, then deletes `Mic Mute.app` from `/Applications` and `~/Applications` (only copies whose bundle identifier is `com.brettinternet.mic-mute`) and `~/Library/LaunchAgents/com.brettinternet.mic-mute.plist`. It keeps `~/Library/Application Support/mic-mute` unless you pass `--purge`; `--dry-run` prints the commands instead of running them. A script can't remove a macOS 13+ login item, so turn Launch at Login off in the tray before uninstalling, or remove Mic Mute in System Settings → General → Login Items afterwards; the script reminds you.

### Bundle only

Build and bundle the app without installing it:

```sh
task build
```

Once the build is complete, a Finder window opens to the bundle in `./target/aarch64-apple-darwin/release/bundle/osx` (`NO_OPEN=1 task build` skips it).

## Develop

[![CI](https://github.com/brettinternet/mic-mute/actions/workflows/ci.yaml/badge.svg)](https://github.com/brettinternet/mic-mute/actions/workflows/ci.yaml)
[![Audit](https://github.com/brettinternet/mic-mute/actions/workflows/audit.yaml/badge.svg)](https://github.com/brettinternet/mic-mute/actions/workflows/audit.yaml)

### Setup

[Install Rust](https://www.rust-lang.org/tools/install). Then, [install go-task](https://taskfile.dev/installation/) or run the commands from the Taskfile separately yourself.

Install development dependencies.

```sh
task init
```

### Run

Run and watch for changes.

```sh
task start
```

### Build

```sh
task build
```

<details>
<summary>Release</summary>
Create a certificate to self-sign.

```sh
openssl req -x509 -newkey rsa:2048 -keyout sign.key -out sign.crt \
    -days 3650 -nodes -subj "/CN=mic-mute"
cat sign.key >> sign.crt
rm sign.key
```

Build a release.

```sh
task release
```

</details>
