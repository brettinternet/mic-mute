use libc::c_void;
use parking_lot::RwLock;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;

type CGFloat = f64;

#[repr(C)]
struct CGPoint {
    pub x: CGFloat,
    pub y: CGFloat,
}

extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CGEventCreate(r: *const c_void) -> *const c_void;
    fn CGEventGetLocation(e: *const c_void) -> CGPoint;
}

pub fn get_cursor_pos() -> Option<(f64, f64)> {
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }

        let point = CGEventGetLocation(event);
        CFRelease(event);
        Some((point.x, point.y))
    }
}

/// Replaces `path` with `contents` atomically: writes `<name>.tmp` in the same directory,
/// flushes it to disk, then renames it over `path`, so readers never see a partial file.
///
/// A symlinked `path` keeps its link: the file it points to is replaced. Missing parent
/// directories are created. Used for settings.json and the state files in `store.rs`.
pub fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    // Replace the link's target rather than the link (e.g. a settings file kept in a dotfiles
    // repository). A missing file has nothing to resolve.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", target.display()),
        )
    })?;
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = target.with_file_name(tmp_name);

    let result = write_synced(&tmp, contents).and_then(|()| fs::rename(&tmp, &target));
    if result.is_err() {
        // Best effort: the write error is the one worth reporting.
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_synced(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(contents)?;
    // Plain fsync, not `File::sync_all`: on macOS that issues F_FULLFSYNC, which blocks the
    // event loop for ~7 ms (up to ~50 ms) on every mute toggle that writes restore.json.
    // fsync is enough to keep a crash from leaving a renamed but empty file.
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn arc_lock<T>(value: T) -> Arc<RwLock<T>> {
    let rwlock = RwLock::new(value);
    Arc::new(rwlock)
}
