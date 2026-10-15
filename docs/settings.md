# Settings

Mic Mute stores its settings at:

```text
~/Library/Application Support/mic-mute/settings.json
```

The settings file shares its folder with two files Mic Mute manages itself, neither of them a setting: `restore.json`, which exists only while microphones it muted still need restoring (the next launch restores them after a crash), and `input-levels.json`, which remembers the last audible input level of each microphone it mutes by volume.

The file is JSON. The complete default configuration is:

```json
{
  "mic_shortcut": {
    "modifiers": ["shift", "meta"],
    "key": "M"
  },
  "show_in_dock": false,
  "launch_at_login": false,
  "show_popup": true,
  "allow_url_unmute": false
}
```

## Options

### `mic_shortcut`

Controls the global microphone mute shortcut.

- `modifiers`: zero or more of `shift`, `meta`/`cmd`/`command`, `ctrl`/`control`, and `alt`/`option`, in any letter case. Any other name makes the shortcut invalid.
- `key`: one key, such as `M`, `F13`, or `Space`, in any letter case. A modifier (`"shift"`) or a combination (`"cmd+M"`) is invalid here.
- Without modifiers, only `F13`–`F24` are allowed, so a plain key is never taken from every app. macOS has no key codes for `F21`–`F24`, so those can't be registered.

The default is `Cmd` + `Shift` + `M`. A settings file that already sets `mic_shortcut` keeps its shortcut.

While Mic Mute runs, its shortcut works in every app, and other apps no longer receive it: `Cmd` + `Shift` + `M`, for example, no longer opens VS Code's Problems panel.

If the shortcut is invalid or macOS can't register it, Mic Mute logs an error and keeps a working shortcut where it can:

- At startup, it uses the default, `Cmd` + `Shift` + `M`. If that can't be registered either, Mic Mute runs without a shortcut; mute from the tray menu.
- When the settings file is reloaded, the shortcut already in use stays active.

The tray menu and the About window show the shortcut in use, and the About window explains why the configured one isn't in use.

### `show_in_dock`

Controls whether Mic Mute appears in the macOS Dock.

- `false` (default): run as a menu bar accessory.
- `true`: show an application icon in the Dock.

The tray menu can also toggle this setting.

### `launch_at_login`

Controls whether Mic Mute opens when the user logs in.

- `false` (default)
- `true`

The tray menu can also toggle this setting.

Mic Mute implements this with the LaunchAgent `~/Library/LaunchAgents/com.brettinternet.mic-mute.plist`. When the setting is on, each launch of the app bundle points that file at the bundle's current location, so moving the app doesn't break the login item. A copy that isn't inside an app bundle, such as a development build started with `cargo run`, never repoints a login item that starts an app bundle.

### `show_popup`

Controls the small on-screen mute-status popup.

- `true` (default): show the popup when the microphone is muted.
- `false`: keep the popup hidden while retaining the tray indicator.

The tray menu can also toggle this setting. A `status` URL shows the popup for two seconds either way.

### `allow_url_unmute`

Controls which [URL actions](../README.md#control-from-other-apps) may run.

- `false` (default): only `mute` and `status` run. Mic Mute refuses `unmute` and `toggle` and logs a warning.
- `true`: every action runs, `unmute` and `toggle` included.

Any app, and any web page through the browser, can open a `com.brettinternet.mic-mute://` URL. By default a URL can therefore mute the microphones and show their state, but never unmute them. With this setting on, any app, or a web page you let open Mic Mute, can unmute them.

## About window

The tray menu's About item shows the mute shortcut in use, the settings file's location, and the version.

The About window doesn't pause Mic Mute: muting, the shortcut, and settings reloads keep working while it's open. Choosing About again brings the open window to the front, updated if the shortcut in use has changed. Each button closes the window.

- **Open Settings** opens `settings.json` in the default text editor. If the file doesn't exist yet, as on a fresh install, Mic Mute first writes it with the settings in use, which are then the defaults. An existing file opens as it is, even an invalid one.
- **Reset Settings** restores every setting to its default, except `launch_at_login`, which mirrors the macOS login item and keeps its value. The defaults apply at once and are saved like any other settings change.

## Editing settings

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu writes its setting changes to this file; Mic Mute's own saves don't trigger a reload.

Fields omitted from the settings file use the documented defaults, including `show_popup: true`, and a missing file means every default. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change.

Saves write every documented field and keep fields Mic Mute doesn't recognize, such as settings from a newer version. Each save is atomic: Mic Mute writes `settings.json.tmp` in the same folder, then renames it over `settings.json`, so a crash never leaves a half-written file. If `settings.json` is a symlink, the file it points to is replaced and the link stays.

### Invalid settings

If `settings.json` can't be read or isn't valid settings JSON (a syntax error, a value of the wrong type, or an empty file), Mic Mute doesn't reset anything:

- It keeps running on the settings it already has: the defaults if the file was invalid at startup, otherwise the last valid settings. Nothing from the broken file is applied.
- It logs a warning with the file's path and the line and column of the error, once per change to the file.
- The first tray menu item and the tray tooltip say what's wrong, for example `settings.json is invalid (line 3, column 5) — using defaults` or `settings.json is invalid (line 3, column 5) — keeping previous settings`.
- It never writes to the broken file. Changes from the tray menu or from About → Reset Settings apply right away but aren't saved, and the log notes each one.

Fix the file, or delete it to reset every setting to its default. The next check, within two seconds, applies the file and removes the notice, replacing any unsaved changes made in the meantime.
