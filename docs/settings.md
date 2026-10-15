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

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu writes its setting changes to this file.

Fields omitted from the settings file use the documented defaults, including `show_popup: true`. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change. Normal saves serialize the documented settings fields.
