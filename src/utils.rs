use anyhow::bail;
use cocoa::base::{id, nil, BOOL, NO};
use cocoa::foundation::NSString;
use libc::c_void;
use parking_lot::RwLock;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;

/// Sound settings at its Input tab, then the Sound pane alone. System Settings (macOS 13 and
/// later) maps this legacy pane id to its Sound extension (`legacyBundleIdentifier` in the
/// extension's Info.plist) and selects its `input` anchor; System Preferences (macOS 11–12)
/// knows both. NSWorkspace reports only whether an app took the URL, not whether it found the
/// anchor, so the second URL helps only if the first is refused outright.
const SOUND_INPUT_SETTINGS_URLS: [&str; 2] = [
    "x-apple.systempreferences:com.apple.preference.sound?input",
    "x-apple.systempreferences:com.apple.preference.sound",
];

/// Opens System Settings at Sound → Input. NSWorkspace opens it without a child process to
/// reap. Main thread only, like every AppKit call.
pub fn open_sound_input_settings() -> anyhow::Result<()> {
    for url in SOUND_INPUT_SETTINGS_URLS {
        if open_url(url)? {
            return Ok(());
        }
        log::warn!("No app opened {}", url);
    }
    bail!("No app opened the Sound settings")
}

/// Asks NSWorkspace to open `url`; returns whether an app took it.
fn open_url(url: &str) -> anyhow::Result<bool> {
    objc::rc::autoreleasepool(|| unsafe {
        let ns_url = ns_url(url)?;
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let is_opened: BOOL = msg_send![workspace, openURL: ns_url];
        Ok(is_opened != NO)
    })
}

/// `url` as an autoreleased `NSURL`.
///
/// # Safety
///
/// Call it inside an autorelease pool, which owns the result.
unsafe fn ns_url(url: &str) -> anyhow::Result<id> {
    let string = NSString::alloc(nil).init_str(url);
    let ns_url: id = msg_send![class!(NSURL), URLWithString: string];
    let _: () = msg_send![string, release];
    if ns_url.is_null() {
        bail!("{} isn't a valid URL", url);
    }
    Ok(ns_url)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_settings_open_at_the_input_tab_then_fall_back_to_the_pane() {
        let [input_tab, pane] = SOUND_INPUT_SETTINGS_URLS;
        assert_eq!(
            input_tab,
            "x-apple.systempreferences:com.apple.preference.sound?input"
        );
        assert_eq!(input_tab.strip_suffix("?input"), Some(pane));
    }

    #[test]
    fn sound_settings_urls_are_valid_urls() {
        // Only builds the NSURLs; nothing is opened.
        for url in SOUND_INPUT_SETTINGS_URLS {
            let is_valid = objc::rc::autoreleasepool(|| unsafe { ns_url(url).is_ok() });
            assert!(is_valid, "{url}");
        }
    }
}
