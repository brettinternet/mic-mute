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
    "key": "M"
  },
  "show_in_dock": false,
  "launch_at_login": false,
  "show_popup": true
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

### `show_popup`

Controls the small on-screen mute-status popup.

- `true` (default): show the popup when the microphone is muted.
- `false`: keep the popup hidden while retaining the tray indicator.

The tray menu can also toggle this setting.

## About window

The tray menu's About item shows the mute shortcut in use, the settings file's location, and the version.

- **Open Settings** opens `settings.json` in the default text editor. If the file doesn't exist yet, as on a fresh install, Mic Mute first writes it with the settings in use, which are then the defaults. An existing file opens as it is, even an invalid one.

## Editing settings

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu writes its setting changes to this file.

Fields omitted from the settings file use the documented defaults, including `show_popup: true`. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change. Normal saves serialize the documented settings fields.
