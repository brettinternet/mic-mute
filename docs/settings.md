# Settings

Mic Mute stores its settings at:

```text
~/Library/Application Support/mic-mute/settings.json
```

The file is JSON. The complete default configuration is:

```json
{
  "mic_shortcut": {
    "modifiers": ["shift", "meta"],
    "key": "A"
  },
  "show_in_dock": false,
  "launch_at_login": false,
  "show_popup": true
}
```

## Options

### `mic_shortcut`

Controls the global microphone mute shortcut.

- `modifiers`: zero or more of `shift`, `meta`/`cmd`/`command`, `ctrl`/`control`, and `alt`/`option`.
- `key`: a key such as `A`, `M`, or `F13`.

The default is `Cmd` + `Shift` + `A`.

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

### `show_popup`

Controls the small on-screen mute-status popup.

- `true` (default): show the popup when the microphone is muted.
- `false`: keep the popup hidden while retaining the tray indicator.

The tray menu can also toggle this setting.

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
