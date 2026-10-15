//! What Mic Mute remembers about microphones between runs, next to `settings.json`:
//! the restore journal (`restore.json`) and the last audible input levels (`input-levels.json`).

use crate::store::StateStore;
use anyhow::{Context, Result};
use log::{error, info, trace, warn};
use objc2_core_audio::AudioDeviceID;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const JOURNAL_FILE_NAME: &str = "restore.json";
pub const LEVELS_FILE_NAME: &str = "input-levels.json";

/// Level changes smaller than this aren't worth a write.
const LEVEL_EPSILON: f32 = 0.001;
/// Users drag input sliders, so `input-levels.json` is written at most this often.
const LEVELS_SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// Identifies a device in the records.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeviceKey {
    /// The CoreAudio device UID, stable across reconnects and restarts.
    Uid(String),
    /// A device whose UID couldn't be read. Its records stay in memory only: after a restart
    /// the same `AudioDeviceID` may name another device.
    Session(AudioDeviceID),
}

impl DeviceKey {
    fn uid(&self) -> Option<&str> {
        match self {
            DeviceKey::Uid(uid) => Some(uid),
            DeviceKey::Session(_) => None,
        }
    }
}

fn is_valid_volume(volume: f32) -> bool {
    volume.is_finite() && (0.0..=1.0).contains(&volume)
}

/// What the app changed on one device and must undo.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceChanges {
    /// The device name when the change was recorded, for logs and readers of the file.
    #[serde(default)]
    pub name: String,
    /// The app turned native mute on; restoring turns it off.
    #[serde(default)]
    pub native_mute: bool,
    /// The input volume before the app muted the device by volume; restoring writes it back.
    #[serde(default)]
    pub saved_volume: Option<f32>,
}

impl DeviceChanges {
    fn is_empty(&self) -> bool {
        !self.native_mute && self.saved_volume.is_none()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JournalFile {
    #[serde(default)]
    devices: BTreeMap<String, DeviceChanges>,
}

/// What the journal file holds, as far as the app knows.
#[derive(Debug, PartialEq)]
enum OnDisk {
    Absent,
    Holds(BTreeMap<String, DeviceChanges>),
    /// The file couldn't be read or parsed; the next save replaces or deletes it.
    Unknown,
}

/// The restore journal: per device, what the app changed and hasn't undone yet.
///
/// Its file exists exactly while the app has unrestored changes on a device with a readable
/// UID, so a launch that finds it knows the previous run didn't exit cleanly.
pub struct RestoreJournal {
    store: Box<dyn StateStore>,
    devices: BTreeMap<DeviceKey, DeviceChanges>,
    on_disk: OnDisk,
    has_unsaved_changes: bool,
    save_failing: bool,
}

impl RestoreJournal {
    /// Reads the journal; an unreadable file is logged and treated as empty.
    pub fn load(store: Box<dyn StateStore>) -> Self {
        let (devices, on_disk) = match store.load() {
            Ok(None) => (BTreeMap::new(), OnDisk::Absent),
            Ok(Some(text)) => match serde_json::from_str::<JournalFile>(&text) {
                Ok(file) => (
                    Self::valid_devices(&file.devices),
                    OnDisk::Holds(file.devices),
                ),
                Err(err) => {
                    error!("Ignoring unreadable {}: {}", JOURNAL_FILE_NAME, err);
                    (BTreeMap::new(), OnDisk::Unknown)
                }
            },
            Err(err) => {
                error!("Failed to read {}: {:#}", JOURNAL_FILE_NAME, err);
                (BTreeMap::new(), OnDisk::Unknown)
            }
        };
        let mut journal = Self {
            store,
            devices,
            on_disk,
            has_unsaved_changes: false,
            save_failing: false,
        };
        // Rewrite a file whose invalid entries were dropped at the next save.
        journal.has_unsaved_changes =
            matches!(&journal.on_disk, OnDisk::Holds(saved) if *saved != journal.persisted());
        journal
    }

    fn valid_devices(
        devices: &BTreeMap<String, DeviceChanges>,
    ) -> BTreeMap<DeviceKey, DeviceChanges> {
        let mut valid = BTreeMap::new();
        for (uid, changes) in devices {
            let mut changes = changes.clone();
            if let Some(volume) = changes
                .saved_volume
                .filter(|volume| !is_valid_volume(*volume))
            {
                warn!(
                    "Ignoring invalid saved volume {} for {} in {}",
                    volume, uid, JOURNAL_FILE_NAME
                );
                changes.saved_volume = None;
            }
            if !uid.is_empty() && !changes.is_empty() {
                valid.insert(DeviceKey::Uid(uid.clone()), changes);
            }
        }
        valid
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn get(&self, key: &DeviceKey) -> Option<&DeviceChanges> {
        self.devices.get(key)
    }

    pub fn saved_volume(&self, key: &DeviceKey) -> Option<f32> {
        self.devices
            .get(key)
            .and_then(|changes| changes.saved_volume)
    }

    fn entry(&mut self, key: &DeviceKey, name: &str) -> &mut DeviceChanges {
        self.devices
            .entry(key.clone())
            .or_insert_with(|| DeviceChanges {
                name: name.to_string(),
                ..DeviceChanges::default()
            })
    }

    /// Records that the app is turning native mute on. Returns whether that's new.
    pub fn record_native_mute(&mut self, key: &DeviceKey, name: &str) -> bool {
        let changes = self.entry(key, name);
        let is_new = !changes.native_mute;
        changes.native_mute = true;
        self.has_unsaved_changes |= is_new;
        is_new
    }

    /// Records the volume a device had before the app mutes it by volume, unless one is
    /// saved already. Returns whether it was saved now.
    pub fn record_saved_volume(&mut self, key: &DeviceKey, name: &str, volume: f32) -> bool {
        if !is_valid_volume(volume) {
            return false;
        }
        let changes = self.entry(key, name);
        if changes.saved_volume.is_some() {
            return false;
        }
        changes.saved_volume = Some(volume);
        self.has_unsaved_changes = true;
        true
    }

    pub fn clear_native_mute(&mut self, key: &DeviceKey) {
        self.update(key, |changes| changes.native_mute = false);
    }

    pub fn clear_saved_volume(&mut self, key: &DeviceKey) {
        self.update(key, |changes| changes.saved_volume = None);
    }

    fn update(&mut self, key: &DeviceKey, change: impl FnOnce(&mut DeviceChanges)) {
        let Some(changes) = self.devices.get_mut(key) else {
            return;
        };
        let before = changes.clone();
        change(changes);
        if *changes != before {
            self.has_unsaved_changes = true;
        }
        if changes.is_empty() {
            self.devices.remove(key);
        }
    }

    /// The journal as saved: only devices with a UID.
    fn persisted(&self) -> BTreeMap<String, DeviceChanges> {
        self.devices
            .iter()
            .filter_map(|(key, changes)| Some((key.uid()?.to_string(), changes.clone())))
            .collect()
    }

    /// Mirrors the journal to its file when it changed: writes it atomically, or deletes the
    /// file once no device needs restoring.
    pub fn save(&mut self) -> Result<()> {
        if !self.has_unsaved_changes && self.on_disk != OnDisk::Unknown {
            return Ok(());
        }
        let devices = self.persisted();
        if devices.is_empty() {
            if self.on_disk != OnDisk::Absent {
                self.store.remove()?;
                self.on_disk = OnDisk::Absent;
            }
        } else if self.on_disk != OnDisk::Holds(devices.clone()) {
            let text = serde_json::to_string_pretty(&JournalFile {
                devices: devices.clone(),
            })
            .with_context(|| format!("Failed to serialize {}", JOURNAL_FILE_NAME))?;
            self.store.save(&text)?;
            self.on_disk = OnDisk::Holds(devices);
        }
        self.has_unsaved_changes = false;
        Ok(())
    }

    /// [`Self::save`], logging a failure once until a save succeeds again; the in-memory
    /// journal still drives restores in this run.
    pub fn save_logged(&mut self) {
        match self.save() {
            Ok(()) => {
                if self.save_failing {
                    info!("Saved {} again", JOURNAL_FILE_NAME);
                    self.save_failing = false;
                }
            }
            Err(err) => {
                if self.save_failing {
                    trace!("Still failing to save {}: {:#}", JOURNAL_FILE_NAME, err);
                } else {
                    error!("Failed to save {}: {:#}", JOURNAL_FILE_NAME, err);
                    self.save_failing = true;
                }
            }
        }
    }
}

/// The last audible input level of one device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputLevel {
    /// The device name when the level was recorded, for readers of the file.
    #[serde(default)]
    pub name: String,
    pub volume: f32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LevelsFile {
    #[serde(default)]
    devices: BTreeMap<String, InputLevel>,
}

/// The last audible input level of each volume-controlled device. Unlike the journal it
/// survives clean exits: it's the fallback when a device is unmuted with no saved level.
pub struct InputLevels {
    store: Box<dyn StateStore>,
    levels: BTreeMap<DeviceKey, InputLevel>,
    has_unsaved_changes: bool,
    last_save: Option<Instant>,
    save_failing: bool,
}

impl InputLevels {
    /// Reads the levels; an unreadable file is logged and treated as empty.
    pub fn load(store: Box<dyn StateStore>) -> Self {
        let levels = match store.load() {
            Ok(None) => BTreeMap::new(),
            Ok(Some(text)) => match serde_json::from_str::<LevelsFile>(&text) {
                Ok(file) => Self::valid_levels(file.devices),
                Err(err) => {
                    error!("Ignoring unreadable {}: {}", LEVELS_FILE_NAME, err);
                    BTreeMap::new()
                }
            },
            Err(err) => {
                error!("Failed to read {}: {:#}", LEVELS_FILE_NAME, err);
                BTreeMap::new()
            }
        };
        Self {
            store,
            levels,
            has_unsaved_changes: false,
            last_save: None,
            save_failing: false,
        }
    }

    fn valid_levels(levels: BTreeMap<String, InputLevel>) -> BTreeMap<DeviceKey, InputLevel> {
        levels
            .into_iter()
            .filter(|(uid, level)| {
                let is_valid = !uid.is_empty() && is_valid_volume(level.volume);
                if !is_valid {
                    warn!("Ignoring invalid level for {} in {}", uid, LEVELS_FILE_NAME);
                }
                is_valid
            })
            .map(|(uid, level)| (DeviceKey::Uid(uid), level))
            .collect()
    }

    pub fn get(&self, key: &DeviceKey) -> Option<f32> {
        self.levels.get(key).map(|level| level.volume)
    }

    /// Records an audible level. Changes smaller than [`LEVEL_EPSILON`] are ignored.
    pub fn record(&mut self, key: &DeviceKey, name: &str, volume: f32) {
        if !is_valid_volume(volume) {
            return;
        }
        if let Some(level) = self.levels.get(key) {
            if (level.volume - volume).abs() < LEVEL_EPSILON {
                return;
            }
        }
        trace!("Recording input level {:.3} for {}", volume, name);
        self.levels.insert(
            key.clone(),
            InputLevel {
                name: name.to_string(),
                volume,
            },
        );
        if key.uid().is_some() {
            self.has_unsaved_changes = true;
        }
    }

    /// Saves pending changes unless the last save was less than [`LEVELS_SAVE_INTERVAL`] ago.
    pub fn save_if_due(&mut self, now: Instant) {
        let is_due = self
            .last_save
            .is_none_or(|last| now.saturating_duration_since(last) >= LEVELS_SAVE_INTERVAL);
        if is_due {
            self.save_at(now);
        }
    }

    /// Saves pending changes now, for exit.
    pub fn save(&mut self) {
        self.save_at(Instant::now());
    }

    fn save_at(&mut self, now: Instant) {
        if !self.has_unsaved_changes {
            return;
        }
        // Count failed attempts too, so a failing disk is retried once per interval.
        self.last_save = Some(now);
        match self.write() {
            Ok(()) => {
                self.has_unsaved_changes = false;
                if self.save_failing {
                    info!("Saved {} again", LEVELS_FILE_NAME);
                    self.save_failing = false;
                }
            }
            Err(err) => {
                if self.save_failing {
                    trace!("Still failing to save {}: {:#}", LEVELS_FILE_NAME, err);
                } else {
                    error!("Failed to save {}: {:#}", LEVELS_FILE_NAME, err);
                    self.save_failing = true;
                }
            }
        }
    }

    fn write(&self) -> Result<()> {
        let devices = self
            .levels
            .iter()
            .filter_map(|(key, level)| Some((key.uid()?.to_string(), level.clone())))
            .collect();
        let text = serde_json::to_string_pretty(&LevelsFile { devices })
            .with_context(|| format!("Failed to serialize {}", LEVELS_FILE_NAME))?;
        self.store.save(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    fn uid(uid: &str) -> DeviceKey {
        DeviceKey::Uid(uid.to_string())
    }

    #[test]
    fn journal_file_lists_changes_by_device_uid() {
        let store = MemoryStore::default();
        let mut journal = RestoreJournal::load(store.boxed());
        journal.record_native_mute(&uid("BuiltIn"), "MacBook Pro Microphone");
        journal.record_saved_volume(&uid("USB"), "USB Mic", 0.65);

        journal.save().unwrap();

        let saved: serde_json::Value = serde_json::from_str(&store.contents().unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({
                "devices": {
                    "BuiltIn": {
                        "name": "MacBook Pro Microphone",
                        "native_mute": true,
                        "saved_volume": null
                    },
                    "USB": { "name": "USB Mic", "native_mute": false, "saved_volume": 0.65 }
                }
            })
        );
    }

    #[test]
    fn journal_saves_only_when_its_content_changes() {
        let store = MemoryStore::default();
        let mut journal = RestoreJournal::load(store.boxed());
        journal.record_native_mute(&uid("BuiltIn"), "Built-in");
        journal.save().unwrap();

        assert!(!journal.record_native_mute(&uid("BuiltIn"), "Built-in"));
        journal.save().unwrap();
        journal.clear_saved_volume(&uid("BuiltIn"));
        journal.save().unwrap();

        assert_eq!(store.saves(), 1);
    }

    #[test]
    fn journal_keeps_the_first_saved_volume() {
        let mut journal = RestoreJournal::load(MemoryStore::default().boxed());

        assert!(journal.record_saved_volume(&uid("USB"), "USB Mic", 0.65));
        assert!(!journal.record_saved_volume(&uid("USB"), "USB Mic", 0.3));

        assert_eq!(journal.saved_volume(&uid("USB")), Some(0.65));
    }

    #[test]
    fn journal_file_is_deleted_once_every_change_is_cleared() {
        let store = MemoryStore::default();
        let mut journal = RestoreJournal::load(store.boxed());
        journal.record_native_mute(&uid("BuiltIn"), "Built-in");
        journal.record_saved_volume(&uid("BuiltIn"), "Built-in", 0.5);
        journal.save().unwrap();

        journal.clear_native_mute(&uid("BuiltIn"));
        journal.save().unwrap();
        assert!(store.contents().is_some());
        journal.clear_saved_volume(&uid("BuiltIn"));
        journal.save().unwrap();

        assert!(journal.is_empty());
        assert_eq!(store.contents(), None);
        assert_eq!(store.removes(), 1);
    }

    #[test]
    fn journal_loads_its_file_and_drops_invalid_entries() {
        let store = MemoryStore::with_contents(
            r#"{
                "devices": {
                    "BuiltIn": { "name": "Built-in", "native_mute": true },
                    "USB": { "name": "USB Mic", "saved_volume": 0.65, "future_field": 1 },
                    "Broken": { "name": "Broken", "saved_volume": 7.5 },
                    "": { "native_mute": true }
                }
            }"#,
        );

        let journal = RestoreJournal::load(store.boxed());

        assert_eq!(journal.len(), 2);
        assert!(journal.get(&uid("BuiltIn")).unwrap().native_mute);
        assert_eq!(journal.saved_volume(&uid("USB")), Some(0.65));
    }

    #[test]
    fn unreadable_journal_starts_empty_and_is_replaced_on_the_next_save() {
        let store = MemoryStore::with_contents("{ not json");
        let mut journal = RestoreJournal::load(store.boxed());
        assert!(journal.is_empty());

        journal.save().unwrap();

        assert_eq!(store.contents(), None);
    }

    #[test]
    fn journal_never_saves_devices_without_a_uid() {
        let store = MemoryStore::default();
        let mut journal = RestoreJournal::load(store.boxed());

        journal.record_native_mute(&DeviceKey::Session(42), "No UID");
        journal.save().unwrap();

        assert!(!journal.is_empty());
        assert_eq!(store.contents(), None);
        assert_eq!(store.saves(), 0);
    }

    #[test]
    fn journal_retries_a_failed_save() {
        let store = MemoryStore::default();
        let mut journal = RestoreJournal::load(store.boxed());
        journal.record_native_mute(&uid("BuiltIn"), "Built-in");
        store.set_fail_saves(true);
        assert!(journal.save().is_err());

        store.set_fail_saves(false);
        journal.save().unwrap();

        assert!(store.contents().unwrap().contains("BuiltIn"));
    }

    #[test]
    fn levels_ignore_changes_below_the_epsilon() {
        let store = MemoryStore::default();
        let mut levels = InputLevels::load(store.boxed());
        let start = Instant::now();
        levels.record(&uid("USB"), "USB Mic", 0.65);
        levels.save_if_due(start);

        levels.record(&uid("USB"), "USB Mic", 0.6505);
        levels.save_if_due(start + Duration::from_secs(5));

        assert_eq!(store.saves(), 1);
        assert_eq!(levels.get(&uid("USB")), Some(0.65));
    }

    #[test]
    fn levels_are_saved_at_most_once_per_interval() {
        let store = MemoryStore::default();
        let mut levels = InputLevels::load(store.boxed());
        let start = Instant::now();
        levels.record(&uid("USB"), "USB Mic", 0.65);
        levels.save_if_due(start);

        levels.record(&uid("USB"), "USB Mic", 0.4);
        levels.save_if_due(start + Duration::from_millis(400));
        assert_eq!(store.saves(), 1);
        levels.save_if_due(start + LEVELS_SAVE_INTERVAL);

        assert_eq!(store.saves(), 2);
        assert!(store.contents().unwrap().contains("0.4"));
    }

    #[test]
    fn levels_flush_pending_changes_on_exit() {
        let store = MemoryStore::default();
        let mut levels = InputLevels::load(store.boxed());
        let start = Instant::now();
        levels.record(&uid("USB"), "USB Mic", 0.65);
        levels.save_if_due(start);
        levels.record(&uid("USB"), "USB Mic", 0.4);
        levels.save_if_due(start + Duration::from_millis(100));

        levels.save();
        levels.save();

        assert_eq!(store.saves(), 2);
        let reloaded = InputLevels::load(store.boxed());
        assert_eq!(reloaded.get(&uid("USB")), Some(0.4));
    }

    #[test]
    fn levels_load_their_file_and_drop_invalid_entries() {
        let store = MemoryStore::with_contents(
            r#"{
                "devices": {
                    "USB": { "name": "USB Mic", "volume": 0.65 },
                    "Broken": { "name": "Broken", "volume": -1.0 }
                }
            }"#,
        );

        let levels = InputLevels::load(store.boxed());

        assert_eq!(levels.get(&uid("USB")), Some(0.65));
        assert_eq!(levels.get(&uid("Broken")), None);
    }

    #[test]
    fn levels_of_devices_without_a_uid_stay_in_memory() {
        let store = MemoryStore::default();
        let mut levels = InputLevels::load(store.boxed());

        levels.record(&DeviceKey::Session(42), "No UID", 0.65);
        levels.save();

        assert_eq!(levels.get(&DeviceKey::Session(42)), Some(0.65));
        assert_eq!(store.saves(), 0);
    }
}
