//! What Mic Mute remembers about microphones between runs, next to `settings.json`:
//! the restore journal (`restore.json`).

use crate::store::StateStore;
use anyhow::{Context, Result};
use log::{error, info, trace, warn};
use objc2_core_audio::AudioDeviceID;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const JOURNAL_FILE_NAME: &str = "restore.json";

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
}
