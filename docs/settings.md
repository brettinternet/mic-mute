# Settings

Mic Mute stores its settings at:

```text
~/Library/Application Support/mic-mute/settings.json
```

The settings file shares its folder with three files Mic Mute manages itself, none of them a setting: `restore.json`, which exists only while microphones it muted still need restoring (after a crash, the next launch restores them, or keeps them muted when it mutes at launch); `input-levels.json`, which remembers the last audible input level of each microphone it mutes by volume; and `mute-state.json`, which remembers whether you last muted or unmuted, for [`launch_state`](#launch_state).

The tray menu's **Settings…** item opens a [Settings window](#settings-window) that changes every option below except `excluded_devices`, which the tray's Microphones menu edits. The file is JSON. The complete default configuration is:

```json
{
  "mic_shortcut": {
    "modifiers": ["shift", "meta"],
    "key": "M"
  },
  "show_in_dock": false,
  "launch_at_login": false,
  "show_popup": true,
  "excluded_devices": [],
  "mode": "toggle",
  "allow_url_unmute": false,
  "launch_state": "keep",
  "mute_on_lock": false,
  "mute_on_sleep": false,
  "mute_on_display_sleep": false,
  "play_sound": false,
  "show_notification": false,
  "popup_position": "bottom_center",
  "popup_size": "medium",
  "popup_mic_on_ms": 1000,
  "popup_mic_off_ms": null
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

The tray menu, the About window, and the Settings window show the shortcut in use, and both windows explain why the configured one isn't in use. To change the shortcut without editing the file, [record it](#recording-a-shortcut) in the Settings window.

### `show_in_dock`

Controls whether Mic Mute appears in the macOS Dock.

- `false` (default): run as a menu bar accessory.
- `true`: show an application icon in the Dock.

The tray menu can also toggle this setting.

### `launch_at_login`

Controls whether Mic Mute opens when the user logs in.

- `false` (default)
- `true`

The tray menu can also toggle this setting. macOS keeps the login item itself, so each launch sets `launch_at_login` to match it: removing Mic Mute from System Settings → General → Login Items turns the setting off.

Mic Mute changes the login item only when `launch_at_login` changes, from the tray or in this file, so saving another setting never adds back a login item you removed. While Mic Mute runs, it doesn't notice a removal in System Settings: the tray item stays checked until the next launch, and turning it off and on again from the tray registers the login item again. If changing the login item fails, the log says so, and the next settings change tries again.

- **macOS 13 and later:** Mic Mute registers itself as a login item (`SMAppService`), listed in System Settings → General → Login Items under Open at Login. macOS finds the app wherever it's moved. If macOS waits for your approval, the setting stays on, the log warns, and the tray item reads "Launch at Login (Approve in System Settings)"; turning the setting on from the tray then opens Login Items. If the login item can't be registered, Mic Mute logs a warning and uses the LaunchAgent below instead.
- **macOS 11 and 12:** Mic Mute writes the LaunchAgent `~/Library/LaunchAgents/com.brettinternet.mic-mute.plist`. Each launch of the app bundle points it at the bundle's current location, so moving the app doesn't break it.
- **After upgrading** on macOS 13 and later, the first launch replaces a LaunchAgent from an earlier version with a login item, then deletes the plist so Mic Mute doesn't open twice. While the login item can't be registered or waits for your approval, the LaunchAgent stays and keeps opening Mic Mute.

A copy that isn't inside an app bundle, such as a development build started with `cargo run`, never registers or removes a login item and never writes or deletes the LaunchAgent; it logs instead. On macOS 13 and later it can't see the installed app's login item, so it leaves the setting as it is.

See the README's [Launch at Login](../README.md#launch-at-login) section for how macOS treats the login item of a locally built app.

### `show_popup`

Controls the small on-screen mute-status popup.

- `true` (default): show the popup while the microphones are muted, or only briefly after each mute with [`popup_mic_off_ms`](#popup_mic_off_ms).
- `false`: keep the popup hidden while retaining the tray indicator.

The tray menu can also toggle this setting. A `status` URL shows the popup for two seconds either way.

### `popup_position`

Controls where the popup appears on the monitor with the mouse pointer.

- `"bottom_center"` (default): centered near the bottom edge, where the popup has always been.
- `"top_center"`, `"top_left"`, `"top_right"`, `"bottom_left"`, `"bottom_right"`.

The popup keeps 40 points from the monitor's edges and, at the top, 40 points below the menu bar. It can cover part of the Dock, but it never takes a click: clicks go through it.

### `popup_size`

Controls how large the popup is. Its text and icons scale with it.

- `"medium"` (default): 250 × 40 points, the size the popup has always had.
- `"small"`: three quarters of medium.
- `"large"`: one and a half times medium.

### `popup_mic_on_ms`

Controls how long the popup stays up showing "Mic on" after you unmute, in milliseconds.

- `1000` (default): one second.
- `0`: hide the popup as soon as you unmute, without showing "Mic on".
- At most `10000`, ten seconds; a larger value counts as `10000`.

Muting again while "Mic on" shows switches the popup to "Mic off": it then stays up while muted or, with [`popup_mic_off_ms`](#popup_mic_off_ms) set, hides that long after the mute. With `popup_mic_off_ms` set, an unmute shows the popup again even after "Mic off" has hidden.

### `popup_mic_off_ms`

Controls how long the popup shows "Mic off" after the microphones mute, in milliseconds, so that it doesn't stay on screen.

- `null` (default): keep the popup up for as long as the microphones stay muted, as it always has.
- A whole number, such as `1500`: hide the popup that long after each mute. `0` shows nothing on a mute: the popup then appears only to show "Mic on" after an unmute, and for `status` URLs.
- At most `10000`, ten seconds; a larger value counts as `10000`.

Every mute shows "Mic off" and starts the time over, also while the popup is already up: from the shortcut (in push-to-talk mode, each release), the tray's Mute item, a URL action, or an automatic mute (`launch_state`, `mute_on_lock`, `mute_on_sleep`, `mute_on_display_sleep`, and the mutes of [push-to-talk mode](#mode)). The re-mutes Mic Mute makes when another app unmutes a microphone or one is plugged in, and a camera turning on or off, don't show the popup or bring it to the front, and they leave the time as it is. Starting Mic Mute while every microphone is already muted, without a mute at launch, shows nothing.

After "Mic off" hides, an unmute shows the popup with "Mic on" for [`popup_mic_on_ms`](#popup_mic_on_ms), and a `status` URL shows it for two seconds. With `show_popup` off, only `status` URLs show it. Setting a number while the popup is up because the microphones are muted hides it at once, or when a `status` URL's two seconds end; setting `null` again while muted shows it.

### Popup values Mic Mute doesn't accept

A `popup_position`, `popup_size`, `popup_mic_on_ms`, or `popup_mic_off_ms` value Mic Mute doesn't accept, such as `"top-right"`, `"huge"`, `-1`, or `1.5`, doesn't make the file invalid: Mic Mute logs a warning naming the value and uses the default instead (`10000` for a `popup_mic_on_ms` or `popup_mic_off_ms` above it; for `popup_mic_off_ms` the default, `null`, keeps the popup up while muted), while the file's other settings apply. Names are read in any letter case. Saves keep such a value as written, so it stays in the file for you to fix.

### `play_sound`

Controls whether Mic Mute plays a sound when the microphones mute or unmute.

- `false` (default)
- `true`: the system sound Bottle when they mute, Tink when they unmute.

The sound plays through the current output device, and only when a request changes the mute state: the shortcut (in push-to-talk mode, each press and release), the tray's Mute item, a URL action, or an automatic mute such as `mute_on_lock`. A request that changes nothing, such as a mute while muted or a mute that fails, plays nothing, so a sound always means the state changed. The re-mutes Mic Mute makes when another app unmutes a microphone, or one is plugged in, play nothing either.

### `show_notification`

Controls whether Mic Mute posts a macOS notification, "Mic off" or "Mic on", when the microphones mute or unmute, for the same changes as `play_sound`. Each notification replaces the previous one in Notification Center, and none has a sound of its own.

- `false` (default)
- `true`

macOS limits these notifications:

- Only the installed app (`task install`, or a release) can post them: macOS attributes a notification to an app bundle. A copy that doesn't run from an app bundle, such as `cargo run`, posts none and logs a warning.
- The first time the setting is on, macOS asks whether Mic Mute may send notifications. Until you allow it, none appear. If you decline, Mic Mute logs a warning; allow them later in System Settings → Notifications → Mic Mute.
- Focus modes hide them, and so does sharing or mirroring the screen unless System Settings → Notifications allows notifications then. During a screen-shared call, rely on the popup or the sound.
- macOS shows no banner while Mic Mute is the active app, for example while its About window has focus.

### `mode`

Controls what the mute shortcut does.

- `"toggle"` (default): each press mutes or unmutes, following what the tray shows. While a microphone that can't be muted keeps the tray at unmuted, a press mutes again and retries it.
- `"push_to_talk"`: holding the shortcut unmutes the microphones, and releasing it mutes them again.

In push-to-talk mode the microphones rest muted:

- Launching Mic Mute in this mode mutes them, and so does switching to it.
- A release can get lost, for example when the screen locks while you hold the shortcut. So Mic Mute also mutes when the screen locks, the screen saver starts, the Mac or its displays go to sleep, or another user's session becomes active; when the shortcut changes while held; and when the mode changes back to `"toggle"` while held. To talk again after one of these, press the shortcut again.
- The tray menu's Mute item and the URL actions work as in toggle mode. An unmute from either lasts until the next release of the shortcut, or one of the events above.

Any shortcut works; one you can hold with one hand, such as `F13` without modifiers, is easiest.

### `allow_url_unmute`

Controls which [URL actions](../README.md#control-from-other-apps) may run.

- `false` (default): only `mute` and `status` run. Mic Mute refuses `unmute` and `toggle` and logs a warning.
- `true`: every action runs, `unmute` and `toggle` included.

Any app, and any web page through the browser, can open a `com.brettinternet.mic-mute://` URL. By default a URL can therefore mute the microphones and show their state, but never unmute them. With this setting on, any app, or a web page you let open Mic Mute, can unmute them.

### `launch_state`

Controls whether Mic Mute mutes the microphones when it starts.

- `"keep"` (default): leave them as they are. Mic Mute starts muted when every microphone is muted, and unmuted otherwise.
- `"muted"`: mute every microphone.
- `"last"`: mute them if you last chose mute before Mic Mute quit; otherwise leave them as they are.

Mic Mute never unmutes at launch, and in push-to-talk mode it always mutes. Quitting restores the microphones Mic Mute muted, so with `"last"`, quitting while muted and starting again leaves them muted. After a crash or `kill -9`, a launch that mutes keeps the microphones Mic Mute had muted as they are instead of restoring them first, so they're never live in between. Mic Mute remembers your last choice in `mute-state.json`, not in `settings.json`.

### `mute_on_lock`, `mute_on_sleep`, `mute_on_display_sleep`

Mute automatically when you step away. Each is `false` by default.

- `mute_on_lock`: mute when the screen locks, or when fast user switching makes another user's session active.
- `mute_on_sleep`: mute when the Mac goes to sleep.
- `mute_on_display_sleep`: mute when the displays go to sleep.

Nothing unmutes automatically: unlocking or waking the Mac leaves the microphones muted until you unmute them. In push-to-talk mode these events, and the screen saver starting, mute whatever these settings say.

macOS reports no lid-close event. Closing the lid of a Mac without an external display puts it and its display to sleep, so `mute_on_sleep` or `mute_on_display_sleep` covers it, as does `mute_on_lock` when the Mac locks on sleep. In clamshell mode, with an external display keeping the Mac awake, closing the lid only turns the built-in display off: nothing reports it, and nothing mutes.

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

## Settings window

The tray menu's **Settings…** item (<kbd>Cmd</kbd> <kbd>,</kbd> while the menu is open) opens a window with a control for every option above except `excluded_devices`, grouped as General (`launch_at_login`, `show_in_dock`), Shortcut (`mic_shortcut`, which you [record](#recording-a-shortcut), and `mode`), Popup (`show_popup`, `popup_position`, `popup_size`, `popup_mic_on_ms`, `popup_mic_off_ms`), Feedback (`play_sound`, `show_notification`), Automatic mute (`launch_state`, `mute_on_lock`, `mute_on_sleep`, `mute_on_display_sleep`), and External control (`allow_url_unmute`, with a warning). Choosing Settings… again brings the open window to the front.

- Each change applies and is saved at once, like a tray toggle. The "Mic on" and "Mic off" fields take whole milliseconds when you press Return or Tab or click elsewhere; anything else puts the setting's value back, and each stepper moves its field by 100 ms.
- Checking **Hide “Mic off” after** sets `popup_mic_off_ms` to `1000`, which its field and stepper then change; unchecking it sets `null`, so the popup stays up while muted again. The field and stepper work only while it's checked.
- The window shows changes made elsewhere as they apply: tray toggles, About → Reset Settings, and edits to `settings.json`, which Mic Mute reloads within two seconds.
- Under the shortcut, the window explains why the configured shortcut isn't the one in use, as the About window does, and under the popup settings it lists the [popup values Mic Mute doesn't accept](#popup-values-mic-mute-doesnt-accept) that the file sets. Choosing a value in the window replaces such a value.
- The window doesn't pause Mic Mute: muting, the shortcut, and settings reloads keep working while it's open. Only a recording pauses the shortcut.
- <kbd>Cmd</kbd> <kbd>W</kbd> closes the window, and <kbd>Cmd</kbd> with <kbd>X</kbd>, <kbd>C</kbd>, <kbd>V</kbd>, or <kbd>A</kbd> cuts, copies, pastes, or selects text in it.
- While `settings.json` is invalid, the top of the window shows the error, with the file's path and the line and column, and an **Open settings.json** button. The controls stay usable: a change applies at once but isn't saved, as with the tray menu (see [Invalid settings](#invalid-settings)).

### Recording a shortcut

Click **Change…** next to the shortcut, then press the new one: hold its modifiers and press its key. The first key that makes a usable shortcut becomes `mic_shortcut`, which is saved and takes effect at once.

- While the window records, the shortcut in use is paused, so pressing it reaches the recorder instead of muting. In `push_to_talk` mode, a hold in progress ends with a mute. The shortcut works again when the recording ends.
- The recorder takes what `mic_shortcut` takes: a key without modifiers only if it's `F13` or higher. It also refuses `Shift` or `Option`, alone or together, on a key that types or edits text (a letter, a digit, punctuation, the space bar, the keypad, Tab, Return, Delete, and the arrow, Home, End, and page keys): as a shortcut, that combination would stop working in every app's text, so `Shift` + `M` would no longer type a capital M. Add `Ctrl` or `Cmd`. Caps Lock, Fn, and keys macOS hotkeys can't use, such as the JIS Eisu and Kana keys, can't be part of a shortcut.
- A key it refuses shows why under the shortcut, and the recorder keeps waiting.
- <kbd>Esc</kbd> without modifiers, **Cancel**, another control, closing the window, switching to another app, and 30 seconds without a key press end the recording without a change. <kbd>Cmd</kbd> <kbd>W</kbd> closes the window even while it records.
- If macOS can't register the recorded shortcut, for example because another app has it, the shortcut in use stays, and the window says why. `settings.json` keeps the recorded one, as if you had typed it into the file.

## About window

The tray menu's About item shows the mute shortcut in use, the settings file's location, and the version.

The About window doesn't pause Mic Mute: muting, the shortcut, and settings reloads keep working while it's open. Choosing About again brings the open window to the front, updated if the shortcut in use has changed. Each button closes the window.

- **Open settings.json** opens `settings.json` in the default text editor, the app macOS opens plain-text files with: TextEdit, unless you've picked another for `.txt` files (Get Info → Open with → Change All). If the file doesn't exist yet, as on a fresh install, Mic Mute first writes it with the settings in use, which are then the defaults. An existing file opens as it is, even an invalid one.
- **Reset Settings** restores every setting to its default, except `launch_at_login`, which mirrors the macOS login item and keeps its value. It also empties `excluded_devices`, so Mic Mute mutes every microphone again, and turns `play_sound` and `show_notification` off; macOS keeps its own answer about Mic Mute's notifications. The defaults apply at once and are saved like any other settings change.

## Editing settings

Mic Mute checks the file every two seconds and reloads it when its modification time changes. Valid changes apply without a restart. The tray menu and the Settings window write their setting changes to this file; Mic Mute's own saves don't trigger a reload.

A change from the tray menu, the Settings window, or About → Reset Settings never saves over an edit that check hasn't picked up yet. When the file's modification time or contents differ from what Mic Mute last read or wrote, it first reloads the file, applies all of it, and then saves the change into it, so both your edit and the change are kept. A tray item sets the value opposite to the one the menu showed: if your edit already set that value, the click keeps it.

Fields omitted from the settings file use the documented defaults, including `show_popup: true` and `excluded_devices: []`, and a missing file means every default. Loading settings does not rewrite the file. The file is created or updated when settings are explicitly saved, such as through a tray-menu setting change.

Saves write every documented field and keep fields Mic Mute doesn't recognize, such as settings from a newer version. Each save is atomic: Mic Mute writes `settings.json.tmp` in the same folder, then renames it over `settings.json`, so a crash never leaves a half-written file. If `settings.json` is a symlink, the file it points to is replaced and the link stays.

### Invalid settings

If `settings.json` can't be read or isn't valid settings JSON (a syntax error, a value of the wrong type, or an empty file), Mic Mute doesn't reset anything:

- It keeps running on the settings it already has: the defaults if the file was invalid at startup, otherwise the last valid settings. Nothing from the broken file is applied.
- It logs a warning with the file's path and the line and column of the error, once per change to the file.
- The first tray menu item and the tray tooltip say what's wrong, for example `settings.json is invalid (line 3, column 5) — using defaults` or `settings.json is invalid (line 3, column 5) — keeping previous settings`. The Settings window shows the full error, with the file's path, at its top.
- It never writes to the broken file. Changes from the tray menu, the Settings window, or About → Reset Settings apply right away but aren't saved, and the log notes each one.

Fix the file, or delete it to reset every setting to its default. The next check, within two seconds, applies the file and removes the notice, replacing any unsaved changes made in the meantime. A change made after the fix but before that check goes into the fixed file rather than replacing it.

A popup value Mic Mute doesn't accept leaves the file valid: only that setting falls back (see [Popup values Mic Mute doesn't accept](#popup-values-mic-mute-doesnt-accept)).
