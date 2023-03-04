# Mic Mute for macOS

Mute your mic with a global shortcut and visual confirmation of mute status.

## Features

- [x] Mute all input devices via MacOS's CoreAudio API
  - Some virtual devices are unable to mute for now
- [ ] Mute newly connected input devices
- [ ] While active, keep input devices muted
- [x] Show mute status in tray
- [x] Show mute status in small popup window
- [x] Follow screen with cursor
- [x] Provide global hotkey muting
- [ ] Add configurable settings (hotkey, window position)

## Background

[VCM](https://github.com/microsoft/PowerToys/issues/21473) for Windows is an excellent utility. However, something similar was missing for MacOS.
