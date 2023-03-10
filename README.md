# Mic Mute for macOS

A system-wide mute for macOS microphones with a global shortcut and visual confirmation of mute status. Inspired by [VCM](https://github.com/microsoft/PowerToys/issues/21473) for Windows.

Mute with <kbd>Cmd</kbd> <kbd>Shift</kbd> <kbd>A</kbd> or from the system tray dropdown.

## Features

- [x] Mute all input devices via MacOS's CoreAudio API
  - Some virtual devices are unable to mute for now
- [ ] Mute newly connected input devices
- [ ] While active, keep input devices muted even if toggled by other methods
- [x] Show microphone mute status in tray
- [x] Show microphone mute status in small popup window
- [x] Follow screen with cursor
- [x] Provide global hotkey muting
- [ ] Support camera toggle
- [ ] Add configurable settings (hotkey, window position)
