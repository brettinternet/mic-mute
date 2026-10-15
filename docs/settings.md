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
  "excluded_devices": []
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

The tray menu can also toggle this setting.

### `excluded_devices`

Lists the input devices Mic Mute leaves alone. It never mutes, unmutes, or keeps muted an excluded device, and the mute state in the tray and popup covers only the other microphones.

- `[]` (default): Mic Mute mutes every input device it can control.
- Each entry names a device by its CoreAudio device UID, which stays the same when the device reconnects, the Mac restarts, or the device is renamed, so two devices with the same name stay separate. UIDs are case-sensitive and must match exactly. `name` is only shown while the device isn't connected.

```json
{
  "excluded_devices": [
    { "uid": "MSLoopbackDriverDevice_UID", "name": "Microsoft Teams Audio" }
  ]
}
```

An entry can also be the bare UID, as in `"excluded_devices": ["MSLoopbackDriverDevice_UID"]`; Mic Mute saves such an entry back unchanged. A device whose UID can't be read is never excluded.

The tray menu's **Microphones** submenu edits this list. It shows every connected input device, checked while Mic Mute controls it: unchecking a device adds it to the list with its name, and checking it removes it. Excluded devices that aren't connected are listed as `Excluded, not connected`, so they can be checked again; a device whose UID can't be read can't be unchecked. Each row also shows the device's state: `Muted`, `Muted by input volume`, `Unmuted`, `Skipped: ignores mute` (it accepts mute and volume changes but ignores them), `Skipped: no mute control`, `Failed` (its last mute or unmute failed, or its state can't be read), or `Excluded`. **Open Sound Settings…** opens System Settings at Sound → Input.

**An excluded microphone may be live while the tray and popup show muted.** Excluding a device that Mic Mute muted undoes what Mic Mute changed, as quitting would: it turns native mute off or restores the input volume. A device that was already muted when Mic Mute muted the others stays muted. If that restore fails, Mic Mute tries again on the next unmute, when the device reconnects, and on quit. Removing a device from the list while mute is on mutes it as soon as Mic Mute applies the change. If every input device is excluded or can't be controlled, Mic Mute doesn't show muted.

About → Reset Settings empties the list.

## About window

The tray menu's About item shows the mute shortcut in use, the settings file's location, and the version.

The About window doesn't pause Mic Mute: muting, the shortcut, and settings reloads keep working while it's open. Choosing About again brings the open window to the front, updated if the shortcut in use has changed. Each button closes the window.

- **Open Settings** opens `settings.json` in the default text editor. If the file doesn't exist yet, as on a fresh install, Mic Mute first writes it with the settings in use, which are then the defaults. An existing file opens as it is, even an invalid one.
- **Reset Settings** restores every setting to its default, except `launch_at_login`, which mirrors the macOS login item and keeps its value. It also empties `excluded_devices`, so Mic Mute mutes every microphone again. The defaults apply at once and are saved like any other settings change.

## Editing settings

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu writes its setting changes to this file; Mic Mute's own saves don't trigger a reload.

Fields omitted from the settings file use the documented defaults, including `show_popup: true` and `excluded_devices: []`, and a missing file means every default. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change.

Saves write every documented field and keep fields Mic Mute doesn't recognize, such as settings from a newer version. Each save is atomic: Mic Mute writes `settings.json.tmp` in the same folder, then renames it over `settings.json`, so a crash never leaves a half-written file. If `settings.json` is a symlink, the file it points to is replaced and the link stays.

### Invalid settings

If `settings.json` can't be read or isn't valid settings JSON (a syntax error, a value of the wrong type, or an empty file), Mic Mute doesn't reset anything:

- It keeps running on the settings it already has: the defaults if the file was invalid at startup, otherwise the last valid settings. Nothing from the broken file is applied.
- It logs a warning with the file's path and the line and column of the error, once per change to the file.
- The first tray menu item and the tray tooltip say what's wrong, for example `settings.json is invalid (line 3, column 5) — using defaults` or `settings.json is invalid (line 3, column 5) — keeping previous settings`.
- It never writes to the broken file. Changes from the tray menu or from About → Reset Settings apply right away but aren't saved, and the log notes each one.

Fix the file, or delete it to reset every setting to its default. The next check, within two seconds, applies the file and removes the notice, replacing any unsaved changes made in the meantime.
