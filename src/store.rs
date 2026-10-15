//! Storage for the small JSON state files that live next to `settings.json`.

use anyhow::{anyhow, Context, Result};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// The folder that holds `settings.json` and the app's state files.
pub fn state_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("mic-mute"))
}

/// The store for `file_name` in [`state_dir`].
pub fn state_file(file_name: &str) -> Box<dyn StateStore> {
    match state_dir() {
        Some(dir) => Box::new(FileStore::new(dir.join(file_name))),
        None => Box::new(MissingStateDir),
    }
}

/// Where one state file's text is kept. Tests use an in-memory store, never the real files.
pub trait StateStore: Send + Sync {
    /// Returns the stored text, or `None` when nothing is stored.
    fn load(&self) -> Result<Option<String>>;
    /// Replaces the stored text.
    fn save(&self, contents: &str) -> Result<()>;
    /// Deletes the stored text; succeeds when nothing is stored.
    fn remove(&self) -> Result<()>;
}

/// A file replaced atomically: readers see the old or the new contents, never a mix.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl StateStore for FileStore {
    fn load(&self) -> Result<Option<String>> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("Failed to read {}", self.path.display())),
        }
    }

    fn save(&self, contents: &str) -> Result<()> {
        write_atomically(&self.path, contents)
    }

    fn remove(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => {
                Err(err).with_context(|| format!("Failed to delete {}", self.path.display()))
            }
        }
    }
}

/// Stands in when the home folder is unknown: nothing is stored, and saving fails.
struct MissingStateDir;

impl StateStore for MissingStateDir {
    fn load(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn save(&self, _contents: &str) -> Result<()> {
        Err(anyhow!("the settings folder is unknown"))
    }

    fn remove(&self) -> Result<()> {
        Ok(())
    }
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".tmp");
    Ok(path.with_file_name(temp_name))
}

/// Writes `<name>.tmp` in the same folder, flushes it, then renames it over `path`.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent folder", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let temp = temp_path(path)?;
    let result = write_flushed(&temp, contents).and_then(|()| {
        fs::rename(&temp, path)
            .with_context(|| format!("Failed to rename {} to {}", temp.display(), path.display()))
    });
    if result.is_err() {
        // Best-effort cleanup; the write error above is what the caller needs.
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_flushed(path: &Path, contents: &str) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    // Plain fsync, not `File::sync_all`: on macOS that issues F_FULLFSYNC, which blocks the
    // event loop for ~7 ms (up to ~50 ms) on every mute toggle. fsync is enough to keep a
    // crash from leaving a renamed but empty file.
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to flush {}", path.display()));
    }
    Ok(())
}

/// An in-memory store that counts writes, for tests.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct MemoryStore {
    state: std::sync::Arc<parking_lot::Mutex<MemoryState>>,
}

#[cfg(test)]
#[derive(Default)]
struct MemoryState {
    contents: Option<String>,
    saves: usize,
    removes: usize,
    fail_saves: bool,
}

#[cfg(test)]
impl MemoryStore {
    pub fn with_contents(contents: &str) -> Self {
        let store = Self::default();
        store.state.lock().contents = Some(contents.to_string());
        store
    }

    pub fn contents(&self) -> Option<String> {
        self.state.lock().contents.clone()
    }

    pub fn saves(&self) -> usize {
        self.state.lock().saves
    }

    pub fn removes(&self) -> usize {
        self.state.lock().removes
    }

    pub fn set_fail_saves(&self, fail: bool) {
        self.state.lock().fail_saves = fail;
    }

    pub fn boxed(&self) -> Box<dyn StateStore> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
impl StateStore for MemoryStore {
    fn load(&self) -> Result<Option<String>> {
        Ok(self.contents())
    }

    fn save(&self, contents: &str) -> Result<()> {
        let mut state = self.state.lock();
        if state.fail_saves {
            return Err(anyhow!("fake save failure"));
        }
        state.contents = Some(contents.to_string());
        state.saves += 1;
        Ok(())
    }

    fn remove(&self) -> Result<()> {
        let mut state = self.state.lock();
        state.contents = None;
        state.removes += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh folder under the system temp dir, deleted on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mic-mute-store-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_store_round_trips_and_leaves_no_temp_file() {
        let dir = TempDir::new();
        let store = FileStore::new(dir.0.join("nested").join("restore.json"));

        assert_eq!(store.load().unwrap(), None);
        store.save("{\"devices\":{}}").unwrap();

        assert_eq!(store.load().unwrap().as_deref(), Some("{\"devices\":{}}"));
        assert_eq!(
            fs::read_dir(dir.0.join("nested"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec!["restore.json"]
        );
    }

    #[test]
    fn file_store_replaces_the_file_instead_of_rewriting_it_in_place() {
        let dir = TempDir::new();
        let path = dir.0.join("restore.json");
        fs::write(&path, "old").unwrap();
        let old_inode = fs::metadata(&path).unwrap().ino();
        let store = FileStore::new(path.clone());

        store.save("new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_ne!(fs::metadata(&path).unwrap().ino(), old_inode);
        assert_eq!(dir.entries(), vec!["restore.json"]);
    }

    #[test]
    fn file_store_failed_save_keeps_the_previous_contents() {
        let dir = TempDir::new();
        let path = dir.0.join("restore.json");
        fs::write(&path, "old").unwrap();
        // A folder where the temp file goes makes creating the temp file fail.
        fs::create_dir(dir.0.join("restore.json.tmp")).unwrap();
        let store = FileStore::new(path.clone());

        assert!(store.save("new").is_err());

        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    }

    #[test]
    fn file_store_overwrites_a_stale_temp_file() {
        let dir = TempDir::new();
        fs::write(dir.0.join("restore.json.tmp"), "left by a crash").unwrap();
        let store = FileStore::new(dir.0.join("restore.json"));

        store.save("new").unwrap();

        assert_eq!(store.load().unwrap().as_deref(), Some("new"));
        assert_eq!(dir.entries(), vec!["restore.json"]);
    }

    #[test]
    fn file_store_remove_deletes_and_tolerates_a_missing_file() {
        let dir = TempDir::new();
        let store = FileStore::new(dir.0.join("restore.json"));
        store.save("contents").unwrap();

        store.remove().unwrap();
        store.remove().unwrap();

        assert_eq!(store.load().unwrap(), None);
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn memory_store_counts_saves_and_removes() {
        let store = MemoryStore::default();

        store.save("a").unwrap();
        store.save("b").unwrap();
        store.remove().unwrap();

        assert_eq!(store.saves(), 2);
        assert_eq!(store.removes(), 1);
        assert_eq!(store.contents(), None);
    }
}
