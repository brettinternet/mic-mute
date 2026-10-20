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
  "excluded_devices": []
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

Lists exact CoreAudio device UIDs that Mic Mute excludes from mute control and from the status shown in the tray and popup.

- `[]` (default): all controllable input devices are managed.
- UIDs are case-sensitive and are not device names or numeric runtime device IDs. A selection survives device renaming and reconnecting without excluding other devices with the same name.

Use the tray menu's **Excluded Mics** submenu to select a virtual input such as Microsoft Teams Audio. The menu saves its UID automatically; you do not need to look it up. Connected devices are displayed by name, with UIDs added when names collide. Disconnected exclusions are displayed by UID so they can still be removed.

**Excluding a device may make it live.** The tray asks for confirmation before adding an exclusion, with Cancel as the default. If Mic Mute muted the device, it attempts to unmute it or restore its saved input volume. Devices already muted before Mic Mute took control are left unchanged. Excluded inputs may record even while the tray and popup show “Mic off”; that status only covers the remaining managed inputs. If none remain controllable, the app does not report muted.

For manual editing, the format is:

```json
{
  "excluded_devices": ["<exact CoreAudio device UID>"]
}
```

Editing the file applies exclusions without a confirmation dialog, including the same unmute behavior. To include an input again, untick it in the submenu or remove its UID. While mute is requested, it will be muted on the next enforcement poll.

Devices that ignore mute requests are never automatically excluded. Unless explicitly excluded, a device that remains live keeps the reported status unmuted and Mic Mute continues retrying.

## Editing settings

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu writes its setting changes to this file.

Fields omitted from the settings file use the documented defaults, including `show_popup: true` and `excluded_devices: []`. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change. Normal saves serialize the documented settings fields.
