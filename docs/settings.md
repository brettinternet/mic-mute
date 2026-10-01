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
  "show_popup": true,
  "excluded_devices": [],
  "skip_unresponsive_devices": true
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

### `excluded_devices`

Lists input device names that Mic Mute never mutes and leaves out of the mute status shown in the tray and popup. Names match case-insensitively and must match the whole device name, as shown in Audio MIDI Setup.

- `[]` (default): every controllable input device is muted.

Use it for virtual inputs that aren't microphones, such as `"Microsoft Teams Audio"`:

```json
{
  "excluded_devices": ["Microsoft Teams Audio"]
}
```

The tray menu's **Skip Mics** submenu lists connected input devices, plus any excluded device that isn't connected. Ticking a device adds it to this list and unticking removes it. If Mic Mute muted a device, excluding it unmutes it.

### `skip_unresponsive_devices`

Controls how Mic Mute treats an input device that accepts a mute request but stays unmuted, such as some virtual devices.

- `true` (default): leave the device out of the mute status, so the tray and popup show muted when every other device muted. Mic Mute keeps retrying the device and logs each failure.
- `false`: report the microphone as unmuted while any such device stays live.

The tray menu can also toggle this setting with **Skip Unresponsive Mics**.

## Editing settings

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu writes its setting changes to this file.

Fields omitted from the settings file use the documented defaults, including `show_popup: true` and `skip_unresponsive_devices: true`. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change. Normal saves serialize the documented settings fields.
