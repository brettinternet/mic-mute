//! The logger. Every record that passes the `RUST_LOG` filter goes to two sinks: stderr, in
//! env_logger's format, and the unified log under [`SUBSYSTEM`], where Console.app and
//! `log stream` read it. The installed app's stderr goes nowhere, so the unified log is how its
//! lines can be read.
//!
//! Both sinks get every line, whether stderr is a terminal or not: a pipe, a file, or an IDE's
//! run console reads stderr without being a terminal, and the unified log costs nothing for
//! lines nobody asked for (it drops debug lines before they're formatted, unless something like
//! `log stream --level debug` wants them). os_log doesn't echo to stderr, so a terminal shows
//! each line once.
//!
//! The unified log is written through `_os_log_impl`, which `os_log_with_type` expands to; the
//! macro itself can't be called from Rust. This takes four libSystem symbols and no C compiler,
//! unlike the `oslog` crate (unreleased since 2022), which builds a C wrapper and, for its
//! logger, adds `dashmap`.

use env_logger::{Builder, Env};
use log::{Level, Log, Metadata, Record, SetLoggerError};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::panic::PanicHookInfo;
use std::ptr::NonNull;
use std::sync::OnceLock;

/// The unified log's subsystem: the app's bundle identifier.
const SUBSYSTEM: &str = "com.brettinternet.mic-mute";

/// The log target, and so the category, of a panic; under the crate, so that `RUST_LOG`
/// directives for `mic_mute` cover it.
const PANIC_TARGET: &str = concat!(env!("CARGO_CRATE_NAME"), "::panic");

/// Log targets that get their own category. The app and its dependencies use a few dozen;
/// targets after these share [`OVERFLOW_CATEGORY`], so the map stays bounded.
const MAX_CATEGORIES: usize = 64;
const OVERFLOW_CATEGORY: &str = "other";

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Installs the logger. `RUST_LOG` replaces the default filter for both sinks.
///
/// # Errors
///
/// Fails if a logger is already installed.
pub fn init() -> Result<(), SetLoggerError> {
    // Release builds skip the poll's per-tick trace output.
    let default_filter = if cfg!(debug_assertions) {
        "trace"
    } else {
        "info"
    };
    let logger = LOGGER.get_or_init(|| Logger {
        stderr: Builder::from_env(Env::default().default_filter_or(default_filter)).build(),
        unified: UnifiedLog::new(SUBSYSTEM),
    });
    log::set_logger(logger)?;
    log::set_max_level(logger.stderr.filter());
    Ok(())
}

/// Writes a panic to the unified log. The default panic hook prints it to stderr only, which
/// the installed app doesn't have. `RUST_LOG` filters it as an `error!` with [`PANIC_TARGET`].
pub fn log_panic(info: &PanicHookInfo<'_>) {
    let Some(logger) = LOGGER.get() else {
        return;
    };
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    // One expression: the record borrows the temporaries of `format_args!`.
    logger.log_to_unified_log(
        &Record::builder()
            .level(Level::Error)
            .target(PANIC_TARGET)
            .args(format_args!("thread '{name}' {info}"))
            .build(),
    );
}

struct Logger {
    /// The `RUST_LOG` filter for both sinks, and the stderr sink.
    stderr: env_logger::Logger,
    unified: UnifiedLog,
}

impl Logger {
    fn log_to_unified_log(&self, record: &Record<'_>) {
        if self.stderr.matches(record) {
            self.unified.log(record);
        }
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.stderr.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if self.stderr.matches(record) {
            self.stderr.log(record);
            self.unified.log(record);
        }
    }

    fn flush(&self) {
        self.stderr.flush();
    }
}

/// The unified log, with a category per log target (`mic_mute::mic`, `tao::…`), as env_logger
/// names them.
struct UnifiedLog {
    subsystem: CString,
    /// Created on first use and never removed. Nothing inside the lock can panic, so a panic
    /// hook that logs never finds it held by its own thread.
    categories: Mutex<HashMap<String, Category>>,
}

struct Category {
    /// `None` if `os_log_create` failed: the target's lines then go to stderr only.
    handle: Option<OsLog>,
    /// The log object may point at its category's name, so the name lives as long as it does.
    _name: CString,
}

impl UnifiedLog {
    fn new(subsystem: &str) -> Self {
        Self {
            subsystem: c_string(subsystem),
            categories: Mutex::new(HashMap::new()),
        }
    }

    fn log(&self, record: &Record<'_>) {
        let kind = os_log_type(record.level());
        let Some(handle) = self.handle(record.target()) else {
            return;
        };
        if handle.is_enabled(kind) {
            handle.write(kind, &record.args().to_string());
        }
    }

    fn handle(&self, target: &str) -> Option<OsLog> {
        let mut categories = self.categories.lock();
        if let Some(category) = categories.get(target) {
            return category.handle;
        }
        let name = if categories.len() < MAX_CATEGORIES {
            target
        } else {
            OVERFLOW_CATEGORY
        };
        categories
            .entry(name.to_owned())
            .or_insert_with(|| {
                let name = c_string(name);
                Category {
                    handle: OsLog::create(&self.subsystem, &name),
                    _name: name,
                }
            })
            .handle
    }
}

/// The unified log's type for each `log` level. It has no trace type, and its fault type is
/// for failures across processes. macOS stores default and error lines on disk, may keep info
/// lines in memory only, and keeps debug lines only while something asks for them.
fn os_log_type(level: Level) -> OsLogType {
    match level {
        Level::Error => OS_LOG_TYPE_ERROR,
        Level::Warn => OS_LOG_TYPE_DEFAULT,
        Level::Info => OS_LOG_TYPE_INFO,
        Level::Debug | Level::Trace => OS_LOG_TYPE_DEBUG,
    }
}

/// `text` as a C string. os_log would end it at the first NUL, so interior NULs are dropped.
fn c_string(text: &str) -> CString {
    CString::new(text).unwrap_or_else(|err| {
        let mut bytes = err.into_vec();
        bytes.retain(|&byte| byte != 0);
        CString::new(bytes).unwrap_or_default()
    })
}

/// `os_log_type_t` (`<os/log.h>`), a `uint8_t`.
type OsLogType = u8;
const OS_LOG_TYPE_DEFAULT: OsLogType = 0x00;
const OS_LOG_TYPE_INFO: OsLogType = 0x01;
const OS_LOG_TYPE_DEBUG: OsLogType = 0x02;
const OS_LOG_TYPE_ERROR: OsLogType = 0x10;

// libsystem_trace, part of libSystem, which every binary links (`<os/log.h>`, macOS 10.12).
extern "C" {
    /// This executable's Mach-O header, which the linker defines. os_log records a line's
    /// format string by its offset in the image this points at.
    #[link_name = "__dso_handle"]
    static DSO_HANDLE: u8;

    fn os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;

    fn os_log_type_enabled(log: *mut c_void, kind: OsLogType) -> bool;

    /// What `os_log_with_type(log, kind, format, …)` calls once `os_log_type_enabled` agrees.
    fn _os_log_impl(
        dso: *const c_void,
        log: *mut c_void,
        kind: OsLogType,
        format: *const c_char,
        buffer: *mut u8,
        size: u32,
    );
}

/// The format of every line: one string argument, shown as is. A plain `%s` would show every
/// line as `<private>`; the messages carry device names, paths, and errors, never secrets.
/// os_log stores the format string's offset, not the string, so the
/// string sits where clang puts the macro's, in this image's `__oslogstring` section.
#[link_section = "__TEXT,__oslogstring,cstring_literals"]
static PUBLIC_STRING_FORMAT: [u8; 11] = *b"%{public}s\0";

/// The argument buffer clang's `__builtin_os_log_format` builds for [`PUBLIC_STRING_FORMAT`],
/// 16-byte aligned like the macro's: a summary byte (`0x02`: an argument that isn't a scalar),
/// the argument count, then the argument's descriptor (`0x22`: a public string), its size, and
/// the pointer.
#[repr(C, align(16))]
struct PublicStringArguments([u8; 12]);

impl PublicStringArguments {
    fn new(string: *const c_char) -> Self {
        let [p0, p1, p2, p3, p4, p5, p6, p7] = string.addr().to_ne_bytes();
        Self([0x02, 0x01, 0x22, 0x08, p0, p1, p2, p3, p4, p5, p6, p7])
    }
}

/// An `os_log_t` from `os_log_create`, kept for the life of the process.
#[derive(Clone, Copy)]
struct OsLog(NonNull<c_void>);

// SAFETY: a log object doesn't change after `os_log_create`, and os_log takes it on any thread:
// `OS_LOG_DEFAULT` is one object for the whole process, and Swift's `Logger`, which wraps one,
// is `Sendable`.
unsafe impl Send for OsLog {}
unsafe impl Sync for OsLog {}

impl OsLog {
    fn create(subsystem: &CString, category: &CString) -> Option<Self> {
        // SAFETY: two NUL-terminated strings that outlive the log object (see `Category`).
        NonNull::new(unsafe { os_log_create(subsystem.as_ptr(), category.as_ptr()) }).map(Self)
    }

    fn is_enabled(self, kind: OsLogType) -> bool {
        // SAFETY: a log object from `os_log_create`, which is never released.
        unsafe { os_log_type_enabled(self.0.as_ptr(), kind) }
    }

    /// Writes `message` as `os_log_with_type(log, kind, "%{public}s", message)` does.
    fn write(self, kind: OsLogType, message: &str) {
        let message = c_string(message);
        let mut arguments = PublicStringArguments::new(message.as_ptr());
        // SAFETY: the macro's call. This image's header, a live log object, a constant format
        // string in this image's `__oslogstring` section, and an argument buffer in the layout
        // that format takes, whose string outlives the call (os_log copies it).
        unsafe {
            _os_log_impl(
                std::ptr::addr_of!(DSO_HANDLE).cast(),
                self.0.as_ptr(),
                kind,
                PUBLIC_STRING_FORMAT.as_ptr().cast(),
                arguments.0.as_mut_ptr(),
                arguments.0.len() as u32,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_unified_log_types() {
        assert_eq!(os_log_type(Level::Error), OS_LOG_TYPE_ERROR);
        assert_eq!(os_log_type(Level::Warn), OS_LOG_TYPE_DEFAULT);
        assert_eq!(os_log_type(Level::Info), OS_LOG_TYPE_INFO);
        assert_eq!(os_log_type(Level::Debug), OS_LOG_TYPE_DEBUG);
        assert_eq!(os_log_type(Level::Trace), OS_LOG_TYPE_DEBUG);
    }

    #[test]
    fn arguments_match_clangs_buffer_for_one_public_string() {
        // clang's __builtin_os_log_format(buf, "%{public}s", text) on arm64 and x86_64.
        let text = c"Mic off";
        let arguments = PublicStringArguments::new(text.as_ptr());
        assert_eq!(arguments.0[..4], [0x02, 0x01, 0x22, 0x08]);
        assert_eq!(arguments.0[4..], text.as_ptr().addr().to_ne_bytes());
        assert_eq!(std::mem::align_of::<PublicStringArguments>(), 16);
    }

    #[test]
    fn format_is_one_public_string() {
        assert_eq!(&PUBLIC_STRING_FORMAT, b"%{public}s\0");
    }

    #[test]
    fn c_string_keeps_the_text_and_drops_interior_nuls() {
        assert_eq!(c_string("Mic Mute 🎙").to_str(), Ok("Mic Mute 🎙"));
        assert_eq!(c_string("a\0b\0").to_str(), Ok("ab"));
    }

    #[test]
    fn panic_target_is_under_the_crate() {
        assert_eq!(PANIC_TARGET, "mic_mute::panic");
    }

    #[test]
    fn categories_stop_growing_at_the_limit() {
        // Creates log objects for a test subsystem; nothing is written to the unified log.
        let log = UnifiedLog::new("com.brettinternet.mic-mute.tests");
        for index in 0..MAX_CATEGORIES + 10 {
            log.handle(&format!("mic_mute::target{index}"));
        }
        let categories = log.categories.lock();
        assert_eq!(categories.len(), MAX_CATEGORIES + 1);
        assert!(categories.contains_key(OVERFLOW_CATEGORY));
        assert!(categories.contains_key("mic_mute::target0"));
        assert!(!categories.contains_key(&format!("mic_mute::target{MAX_CATEGORIES}")));
    }
}
