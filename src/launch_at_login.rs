//! Launch at Login.
//!
//! - macOS 13 and later: the app registers itself as a login item with `SMAppService.mainApp`,
//!   listed under System Settings → General → Login Items, which finds the app wherever it is
//!   moved. At startup, a LaunchAgent plist from an earlier version is replaced by the login
//!   item. A LaunchAgent is written only as a fallback when the login item can't be registered.
//! - macOS 11 and 12: a LaunchAgent plist in ~/Library/LaunchAgents starts the app, repointed
//!   at the app bundle at startup when the app has moved.
//!
//! Only an app bundle changes either. A binary outside an app bundle (`target/debug/mic-mute`,
//! a Gatekeeper-translocated copy) never registers or unregisters a login item and never
//! writes or deletes the plist: `set` logs instead.

use crate::utils::write_atomically;
use anyhow::{anyhow, Context, Result};
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::ClassType;
use objc2_foundation::NSError;
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// The LaunchAgent's label, which is also the bundle identifier.
const PLIST_LABEL: &str = "com.brettinternet.mic-mute";

/// The first macOS with `SMAppService`.
const LOGIN_ITEM_MIN_VERSION: OsVersion = OsVersion {
    major: 13,
    minor: 0,
};

const MENU_TITLE: &str = "Launch at Login";
const MENU_TITLE_NEEDS_APPROVAL: &str = "Launch at Login (Approve in System Settings)";
const APPROVAL_WARNING: &str = "Launch at login waits for your approval: allow Mic Mute in \
                                System Settings → General → Login Items";

/// Whether the last check found a login item that macOS opens only after the user approves it.
static NEEDS_APPROVAL: AtomicBool = AtomicBool::new(false);

/// What opens the app at login, as macOS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginItemState {
    /// macOS opens the app at login: the login item, or a LaunchAgent.
    Enabled,
    /// The login item is registered, but macOS opens the app at login only once the user
    /// allows it in System Settings → General → Login Items. macOS also reports this after the
    /// user revokes their approval there.
    RequiresApproval,
    /// Nothing opens the app at login.
    Disabled,
}

impl LoginItemState {
    /// The `launch_at_login` setting that mirrors this state: on while the login item is
    /// registered, even before it's approved.
    pub fn is_on(self) -> bool {
        self != Self::Disabled
    }
}

/// Startup: what opens the app at login, for the `launch_at_login` setting to mirror; `None`
/// when this process can't tell (a binary outside an app bundle, on macOS 13 and later).
///
/// An app bundle also tidies up: on macOS 13 and later it replaces a LaunchAgent from an
/// earlier version with a login item, and on macOS 11 and 12 it repoints the LaunchAgent at
/// itself when it has moved. Never opens System Settings.
pub fn reconcile() -> Option<LoginItemState> {
    let exe = match running_bundle_exe() {
        Ok(Some(exe)) => exe,
        Ok(None) => return state_seen_outside_bundle(mechanism(), has_plist()),
        Err(e) => {
            log::error!("Failed to check launch at login: {:#}", e);
            return None;
        }
    };
    let state = match mechanism() {
        Mechanism::LaunchAgent => reconcile_launch_agent(&exe),
        Mechanism::LoginItem => reconcile_login_item(&exe),
    };
    publish(state);
    state
}

/// Turns Launch at Login on or off for the running app bundle. A binary outside an app bundle
/// changes nothing and logs instead.
///
/// On macOS 13 and later, when turning it on leaves the new login item waiting for the user's
/// approval, this opens System Settings → General → Login Items for them.
pub fn set(enabled: bool) -> Result<()> {
    let Some(exe) = running_bundle_exe()? else {
        log::info!(
            "Launch at login: not changing the login item, because this binary isn't inside an \
             app bundle"
        );
        return Ok(());
    };
    match mechanism() {
        Mechanism::LaunchAgent => {
            if enabled {
                point_at(&exe)
            } else {
                remove_plist()
            }
        }
        Mechanism::LoginItem => {
            let service = main_app_service().context("macOS returned no main app service")?;
            let before = read_state(&service);
            let update = update_login_item(&service, before, enabled, &exe);
            publish(Some(update.state));
            if update.registered_now && update.state == LoginItemState::RequiresApproval {
                log::warn!("{}", APPROVAL_WARNING);
                // The user just turned it on: show them where to allow it.
                // SAFETY: mechanism() checked that the class exists.
                unsafe { SMAppService::openSystemSettingsLoginItems() };
            }
            update.error.map_or(Ok(()), Err)
        }
    }
}

/// Whether the last `reconcile` or `set` found a login item that waits for the user's approval.
pub fn needs_approval() -> bool {
    NEEDS_APPROVAL.load(Ordering::Relaxed)
}

/// The tray item's title; it points to System Settings while the login item waits for approval.
pub fn menu_title(needs_approval: bool) -> &'static str {
    if needs_approval {
        MENU_TITLE_NEEDS_APPROVAL
    } else {
        MENU_TITLE
    }
}

fn publish(state: Option<LoginItemState>) {
    let needs_approval = state == Some(LoginItemState::RequiresApproval);
    NEEDS_APPROVAL.store(needs_approval, Ordering::Relaxed);
}

/// Toggle the app's dock icon visibility at runtime.
///
/// `true`  → NSApplicationActivationPolicyRegular (shows in Dock + Cmd-Tab)
/// `false` → NSApplicationActivationPolicyAccessory (no Dock icon, default)
pub fn set_dock_visible(visible: bool) {
    // NSApplicationActivationPolicyRegular = 0
    // NSApplicationActivationPolicyAccessory = 1
    let policy: i64 = if visible { 0 } else { 1 };
    unsafe {
        let app: cocoa::base::id = objc::msg_send![objc::class!(NSApplication), sharedApplication];
        let _: () = objc::msg_send![app, setActivationPolicy: policy];
    }
}

/// What starts the app at login on this Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mechanism {
    /// `SMAppService.mainApp` (macOS 13 and later), with a LaunchAgent as the fallback.
    LoginItem,
    /// A LaunchAgent plist in ~/Library/LaunchAgents (macOS 11 and 12).
    LaunchAgent,
}

/// A macOS version; the patch level doesn't matter here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OsVersion {
    major: u32,
    minor: u32,
}

/// Picked once per process, from the running macOS version.
fn mechanism() -> Mechanism {
    static MECHANISM: OnceLock<Mechanism> = OnceLock::new();
    *MECHANISM.get_or_init(|| {
        let version = os_version();
        let mechanism = mechanism_for(version);
        // objc2 panics on a missing class; a misread version must not crash the app.
        if mechanism == Mechanism::LoginItem && AnyClass::get(c"SMAppService").is_none() {
            log::warn!(
                "Launch at login: SMAppService is missing on macOS {:?}; using the LaunchAgent",
                version
            );
            return Mechanism::LaunchAgent;
        }
        log::trace!("Launch at login: {:?} on macOS {:?}", mechanism, version);
        mechanism
    })
}

/// The login item on macOS 13 and later; the LaunchAgent on earlier or unreadable versions,
/// which works everywhere.
fn mechanism_for(version: Option<OsVersion>) -> Mechanism {
    match version {
        Some(version) if version >= LOGIN_ITEM_MIN_VERSION => Mechanism::LoginItem,
        _ => Mechanism::LaunchAgent,
    }
}

/// The running macOS version, from `kern.osproductversion`.
fn os_version() -> Option<OsVersion> {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated, `buf` has `len` writable bytes, and nothing is set.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        log::warn!(
            "Failed to read the macOS version: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    let text = std::str::from_utf8(buf.get(..len)?).ok()?;
    parse_os_version(text.trim_end_matches('\0'))
}

/// Parses a version such as `15.7.9`, `13.0`, or `26`.
fn parse_os_version(text: &str) -> Option<OsVersion> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = match parts.next() {
        Some(minor) => minor.parse().ok()?,
        None => 0,
    };
    Some(OsVersion { major, minor })
}

/// What a binary outside an app bundle can tell without changing anything. A LaunchAgent
/// starts the installed app; on macOS 13 and later, without one, the installed app's login
/// item is out of reach, so the setting is left as it is.
fn state_seen_outside_bundle(mechanism: Mechanism, has_plist: bool) -> Option<LoginItemState> {
    match (mechanism, has_plist) {
        (_, true) => Some(LoginItemState::Enabled),
        (Mechanism::LaunchAgent, false) => Some(LoginItemState::Disabled),
        (Mechanism::LoginItem, false) => None,
    }
}

/// The running executable when it is an app bundle's; `None` for any other binary.
fn running_bundle_exe() -> Result<Option<PathBuf>> {
    let exe = std::env::current_exe().context("Failed to resolve the running executable")?;
    if is_inside_app_bundle(&exe) {
        Ok(Some(exe))
    } else {
        log::trace!(
            "{} isn't inside an app bundle; launch at login stays as it is",
            exe.display()
        );
        Ok(None)
    }
}

fn reconcile_launch_agent(exe: &Path) -> Option<LoginItemState> {
    if !has_plist() {
        return Some(LoginItemState::Disabled);
    }
    // The app may have moved since the LaunchAgent was written.
    if let Err(e) = point_at(exe) {
        log::error!("Failed to repoint the LaunchAgent: {:#}", e);
    }
    Some(LoginItemState::Enabled)
}

fn reconcile_login_item(exe: &Path) -> Option<LoginItemState> {
    let Some(service) = main_app_service() else {
        log::error!("Failed to check launch at login: macOS returned no main app service");
        return None;
    };
    let before = read_state(&service);
    let is_on = has_plist() || before.is_some_and(LoginItemState::is_on);
    if !is_on {
        return before;
    }
    // Moves a LaunchAgent from an earlier version to the login item; changes nothing when the
    // login item is all there is.
    let update = update_login_item(&service, before, true, exe);
    if let Some(e) = update.error {
        log::error!("Failed to update launch at login: {:#}", e);
    }
    if update.state == LoginItemState::RequiresApproval {
        log::warn!("{}", APPROVAL_WARNING);
    }
    Some(update.state)
}

/// `SMAppService.mainApp`, the running app as a login item. Only for an app bundle on macOS 13
/// and later (`mechanism()`); outside a bundle it would stand for the binary's directory.
fn main_app_service() -> Option<Retained<SMAppService>> {
    // Not `SMAppService::mainAppService()`: the generated binding panics if macOS returns nil,
    // and a panic would end the app without restoring the microphones.
    // SAFETY: mechanism() checked that the class exists; the getter takes no arguments.
    unsafe { objc2::msg_send![SMAppService::class(), mainAppService] }
}

/// The login item's state; `None` for a status this build doesn't know.
fn read_state(service: &SMAppService) -> Option<LoginItemState> {
    // SAFETY: a status query; this runs on the main thread.
    let status = unsafe { service.status() };
    let state = login_item_state(status);
    if state.is_none() {
        log::warn!("Launch at login: unknown login item status {}", status.0);
    }
    state
}

fn login_item_state(status: SMAppServiceStatus) -> Option<LoginItemState> {
    match status {
        SMAppServiceStatus::Enabled => Some(LoginItemState::Enabled),
        SMAppServiceStatus::RequiresApproval => Some(LoginItemState::RequiresApproval),
        // Some macOS versions report NotFound for an app that never registered.
        SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => {
            Some(LoginItemState::Disabled)
        }
        _ => None,
    }
}

/// What `update_login_item` did.
struct LoginUpdate {
    /// What opens the app at login now.
    state: LoginItemState,
    /// Whether this update registered the login item.
    registered_now: bool,
    /// The first failure; the steps after it still ran.
    error: Option<anyhow::Error>,
}

/// macOS 13 and later: registers or unregisters the login item for `enabled`, then keeps,
/// writes, or deletes the LaunchAgent to match (see `plist_plan`).
fn update_login_item(
    service: &SMAppService,
    before: Option<LoginItemState>,
    enabled: bool,
    exe: &Path,
) -> LoginUpdate {
    let mut error = None;
    let mut registered_now = false;
    let after = match login_item_action(enabled, before) {
        ItemAction::Keep => before,
        ItemAction::Register => {
            // SAFETY: as in `read_state`.
            match unsafe { service.registerAndReturnError() } {
                Ok(()) => {
                    registered_now = true;
                    log::info!(
                        "Launch at login: registered {} as a login item",
                        exe.display()
                    );
                }
                // Not an error yet: the LaunchAgent fallback below can still start the app.
                Err(err) => log::warn!(
                    "Launch at login: failed to register the login item: {}",
                    describe(&err)
                ),
            }
            read_state(service)
        }
        ItemAction::Unregister => {
            // SAFETY: as in `read_state`.
            match unsafe { service.unregisterAndReturnError() } {
                Ok(()) => log::info!("Launch at login: unregistered the login item"),
                Err(err) => {
                    error = Some(anyhow!(
                        "Failed to unregister the login item: {}",
                        describe(&err)
                    ))
                }
            }
            read_state(service)
        }
    };
    let plist = match plist_plan(enabled, after) {
        PlistPlan::Remove => remove_plist(),
        PlistPlan::KeepExisting if has_plist() => point_at(exe),
        PlistPlan::KeepExisting => Ok(()),
        PlistPlan::Write => {
            log::warn!("Launch at login: using the LaunchAgent instead of a login item");
            point_at(exe)
        }
    };
    if let Err(e) = plist {
        error.get_or_insert(e);
    }
    LoginUpdate {
        state: resulting_state(after, has_plist()),
        registered_now,
        error,
    }
}

/// What `update_login_item` does with the login item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemAction {
    Register,
    Unregister,
    Keep,
}

fn login_item_action(enabled: bool, state: Option<LoginItemState>) -> ItemAction {
    use LoginItemState::{Disabled, Enabled, RequiresApproval};
    match (enabled, state) {
        // Only the user can approve a waiting login item: registering again changes nothing.
        (true, Some(Enabled | RequiresApproval)) | (false, Some(Disabled)) => ItemAction::Keep,
        // An unknown status gets the call: an unneeded one only fails.
        (true, Some(Disabled) | None) => ItemAction::Register,
        (false, Some(Enabled | RequiresApproval) | None) => ItemAction::Unregister,
    }
}

/// What `update_login_item` does with the LaunchAgent, given the login item's state afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlistPlan {
    /// Delete it: nothing should start the app, or the login item does, and a LaunchAgent
    /// would start it a second time.
    Remove,
    /// Keep one that exists, pointed at the running app, without creating one: it starts the
    /// app until the user approves the login item. A new one would override a user who
    /// revoked their approval.
    KeepExisting,
    /// Create or repoint it: the login item isn't registered, so the LaunchAgent starts the
    /// app instead.
    Write,
}

fn plist_plan(enabled: bool, login_item: Option<LoginItemState>) -> PlistPlan {
    match (enabled, login_item) {
        (false, _) | (true, Some(LoginItemState::Enabled)) => PlistPlan::Remove,
        (true, Some(LoginItemState::RequiresApproval)) => PlistPlan::KeepExisting,
        (true, Some(LoginItemState::Disabled) | None) => PlistPlan::Write,
    }
}

/// What opens the app at login, from the login item's state and whether a LaunchAgent exists.
fn resulting_state(login_item: Option<LoginItemState>, has_plist: bool) -> LoginItemState {
    match login_item {
        Some(state @ (LoginItemState::Enabled | LoginItemState::RequiresApproval)) => state,
        _ if has_plist => LoginItemState::Enabled,
        _ => LoginItemState::Disabled,
    }
}

fn describe(err: &NSError) -> String {
    format!(
        "{} ({} error {})",
        err.localizedDescription(),
        err.domain(),
        err.code()
    )
}

fn plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", PLIST_LABEL))
    })
}

fn has_plist() -> bool {
    plist_path().is_some_and(|p| p.exists())
}

/// Makes the LaunchAgent start `exe`, an app bundle's executable. Writes only when the plist
/// is missing or starts another program.
fn point_at(exe: &Path) -> Result<()> {
    let path = plist_path().context("Cannot resolve LaunchAgents directory")?;
    let existing = read_plist(&path)?;
    if plist_starts(existing.as_deref(), exe) {
        log::trace!("Launch at login already starts {}", exe.display());
        return Ok(());
    }
    let plist = render_plist(&exe.to_string_lossy());
    write_atomically(&path, plist.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    match existing.as_deref().and_then(program_path) {
        Some(previous) => log::info!(
            "Launch at login now starts {} (was {})",
            exe.display(),
            previous
        ),
        None => log::info!(
            "Launch at login enabled: {} starts {}",
            path.display(),
            exe.display()
        ),
    }
    Ok(())
}

fn remove_plist() -> Result<()> {
    let path = plist_path().context("Cannot resolve LaunchAgents directory")?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            log::info!("Launch at login: deleted {}", path.display());
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to delete {}", path.display())),
    }
}

/// The LaunchAgent plist's contents, or `None` when there is none.
fn read_plist(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// Whether `plist`, a LaunchAgent's contents, already starts `exe`.
fn plist_starts(plist: Option<&str>, exe: &Path) -> bool {
    plist.and_then(program_path).as_deref() == Some(&*exe.to_string_lossy())
}

/// Whether `exe` is an app bundle's executable (`…/Name.app/Contents/MacOS/<binary>`).
/// Gatekeeper's translocated copies don't count: their random path changes on every launch.
fn is_inside_app_bundle(exe: &Path) -> bool {
    let Some(macos) = exe.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    let Some(bundle) = contents.parent() else {
        return false;
    };
    let is_translocated = exe
        .components()
        .any(|c| c.as_os_str() == OsStr::new("AppTranslocation"));
    macos.file_name() == Some(OsStr::new("MacOS"))
        && contents.file_name() == Some(OsStr::new("Contents"))
        && bundle.extension() == Some(OsStr::new("app"))
        && !is_translocated
}

/// The LaunchAgent plist that starts `exe` at login. For paths without XML special
/// characters, it is byte-identical to what earlier versions wrote.
fn render_plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
</dict>
</plist>
"#,
        label = PLIST_LABEL,
        exe = xml_escape(exe),
    )
}

/// The program an XML LaunchAgent plist starts: its first `ProgramArguments` string.
/// `None` for anything else, such as a binary plist or one without `ProgramArguments`.
fn program_path(plist: &str) -> Option<String> {
    let (_, rest) = plist.split_once("<key>ProgramArguments</key>")?;
    let rest = rest.trim_start().strip_prefix("<array>")?;
    let rest = rest.trim_start().strip_prefix("<string>")?;
    let (program, _) = rest.split_once("</string>")?;
    Some(xml_unescape(program))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use LoginItemState::{Disabled, Enabled, RequiresApproval};

    const BUNDLE_EXE: &str = "/Applications/Mic Mute.app/Contents/MacOS/mic-mute";
    const OTHER_BUNDLE_EXE: &str = "/Users/me/Applications/Mic Mute.app/Contents/MacOS/mic-mute";
    const DEV_EXE: &str = "/Users/me/src/mic-mute/target/debug/mic-mute";
    const TRANSLOCATED_EXE: &str = "/private/var/folders/ab/xyz/T/AppTranslocation/0A1B2C3D/d/Mic Mute.app/Contents/MacOS/mic-mute";

    /// Every login item state, and an unknown status.
    const STATES: [Option<LoginItemState>; 4] =
        [Some(Enabled), Some(RequiresApproval), Some(Disabled), None];

    fn version(major: u32, minor: u32) -> Option<OsVersion> {
        Some(OsVersion { major, minor })
    }

    #[test]
    fn test_plist_path_is_in_launch_agents() {
        let path = plist_path().unwrap();
        assert!(path.to_string_lossy().contains("LaunchAgents"));
        assert!(path.to_string_lossy().ends_with(".plist"));
    }

    #[test]
    fn parse_os_version_reads_product_versions() {
        assert_eq!(parse_os_version("15.7.9"), version(15, 7));
        assert_eq!(parse_os_version("13.0"), version(13, 0));
        assert_eq!(parse_os_version("12.7.6\n"), version(12, 7));
        assert_eq!(parse_os_version("26"), version(26, 0));
        assert_eq!(parse_os_version("10.16"), version(10, 16));
    }

    #[test]
    fn parse_os_version_rejects_other_text() {
        for text in ["", "macOS", "15.x", ".1", "-13.0"] {
            assert_eq!(parse_os_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn mechanism_is_the_login_item_from_macos_13() {
        for v in [
            version(13, 0),
            version(13, 7),
            version(15, 7),
            version(26, 0),
        ] {
            assert_eq!(mechanism_for(v), Mechanism::LoginItem, "{v:?}");
        }
    }

    #[test]
    fn mechanism_is_the_launch_agent_before_macos_13_or_on_an_unknown_version() {
        // 10.16 is what macOS 11 and later report to binaries built with an old SDK.
        for v in [version(11, 0), version(12, 7), version(10, 16), None] {
            assert_eq!(mechanism_for(v), Mechanism::LaunchAgent, "{v:?}");
        }
    }

    #[test]
    fn login_item_status_maps_to_the_setting() {
        let cases = [
            (SMAppServiceStatus::Enabled, Some(Enabled), true),
            (
                SMAppServiceStatus::RequiresApproval,
                Some(RequiresApproval),
                true,
            ),
            (SMAppServiceStatus::NotRegistered, Some(Disabled), false),
            (SMAppServiceStatus::NotFound, Some(Disabled), false),
        ];
        for (status, state, is_on) in cases {
            assert_eq!(login_item_state(status), state, "{status:?}");
            assert_eq!(state.map(LoginItemState::is_on), Some(is_on), "{status:?}");
        }
    }

    #[test]
    fn unknown_login_item_status_keeps_the_setting() {
        assert_eq!(login_item_state(SMAppServiceStatus(4)), None);
        assert_eq!(login_item_state(SMAppServiceStatus(-1)), None);
    }

    #[test]
    fn turning_on_registers_only_an_unregistered_login_item() {
        let actions = STATES.map(|state| login_item_action(true, state));
        assert_eq!(
            actions,
            [
                ItemAction::Keep,
                ItemAction::Keep,
                ItemAction::Register,
                ItemAction::Register
            ]
        );
    }

    #[test]
    fn turning_off_unregisters_a_registered_login_item() {
        let actions = STATES.map(|state| login_item_action(false, state));
        assert_eq!(
            actions,
            [
                ItemAction::Unregister,
                ItemAction::Unregister,
                ItemAction::Keep,
                ItemAction::Unregister
            ]
        );
    }

    #[test]
    fn turning_off_removes_the_launch_agent() {
        for state in STATES {
            assert_eq!(plist_plan(false, state), PlistPlan::Remove, "{state:?}");
        }
    }

    #[test]
    fn an_enabled_login_item_replaces_the_launch_agent() {
        assert_eq!(plist_plan(true, Some(Enabled)), PlistPlan::Remove);
    }

    #[test]
    fn a_login_item_waiting_for_approval_keeps_but_never_creates_the_launch_agent() {
        assert_eq!(
            plist_plan(true, Some(RequiresApproval)),
            PlistPlan::KeepExisting
        );
    }

    #[test]
    fn the_launch_agent_starts_the_app_when_registering_failed() {
        assert_eq!(plist_plan(true, Some(Disabled)), PlistPlan::Write);
        assert_eq!(plist_plan(true, None), PlistPlan::Write);
    }

    #[test]
    fn resulting_state_counts_the_launch_agent_as_enabled() {
        assert_eq!(resulting_state(Some(Disabled), true), Enabled);
        assert_eq!(resulting_state(None, true), Enabled);
        assert_eq!(resulting_state(Some(Disabled), false), Disabled);
        assert_eq!(resulting_state(None, false), Disabled);
    }

    #[test]
    fn resulting_state_reports_the_login_item_first() {
        for has_plist in [true, false] {
            assert_eq!(resulting_state(Some(Enabled), has_plist), Enabled);
            assert_eq!(
                resulting_state(Some(RequiresApproval), has_plist),
                RequiresApproval
            );
        }
    }

    #[test]
    fn a_binary_outside_a_bundle_sees_only_the_launch_agent() {
        for mechanism in [Mechanism::LoginItem, Mechanism::LaunchAgent] {
            assert_eq!(state_seen_outside_bundle(mechanism, true), Some(Enabled));
        }
        assert_eq!(
            state_seen_outside_bundle(Mechanism::LaunchAgent, false),
            Some(Disabled)
        );
        assert_eq!(state_seen_outside_bundle(Mechanism::LoginItem, false), None);
    }

    #[test]
    fn menu_title_points_to_system_settings_while_approval_is_pending() {
        assert_eq!(menu_title(false), "Launch at Login");
        assert_eq!(
            menu_title(true),
            "Launch at Login (Approve in System Settings)"
        );
    }

    #[test]
    fn render_plist_matches_the_format_earlier_versions_wrote() {
        let expected = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
            "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
            "<plist version=\"1.0\">\n",
            "<dict>\n",
            "\t<key>Label</key>\n",
            "\t<string>com.brettinternet.mic-mute</string>\n",
            "\t<key>ProgramArguments</key>\n",
            "\t<array>\n",
            "\t\t<string>/Applications/Mic Mute.app/Contents/MacOS/mic-mute</string>\n",
            "\t</array>\n",
            "\t<key>RunAtLoad</key>\n",
            "\t<true/>\n",
            "\t<key>KeepAlive</key>\n",
            "\t<false/>\n",
            "</dict>\n",
            "</plist>\n",
        );

        assert_eq!(render_plist(BUNDLE_EXE), expected);
    }

    #[test]
    fn program_path_round_trips_through_render_plist() {
        for exe in [
            BUNDLE_EXE,
            DEV_EXE,
            "/Applications/Mic & <Mute>.app/Contents/MacOS/mic-mute",
        ] {
            assert_eq!(program_path(&render_plist(exe)).as_deref(), Some(exe));
        }
    }

    #[test]
    fn render_plist_escapes_xml_special_characters() {
        let plist = render_plist("/Apps/A & B <x>.app/Contents/MacOS/mic-mute");

        assert!(plist
            .contains("<string>/Apps/A &amp; B &lt;x&gt;.app/Contents/MacOS/mic-mute</string>"));
    }

    #[test]
    fn program_path_reads_plutil_formatted_plists() {
        let plist = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>KeepAlive</key>\n\t<false/>\n\t<key>Label</key>\n\t<string>com.brettinternet.mic-mute</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/Applications/Mic Mute.app/Contents/MacOS/mic-mute</string>\n\t\t<string>--flag</string>\n\t</array>\n</dict>\n</plist>\n";

        assert_eq!(program_path(plist).as_deref(), Some(BUNDLE_EXE));
    }

    #[test]
    fn program_path_is_none_for_plists_it_cannot_read() {
        for plist in [
            "bplist00\u{1}\u{2}binary",
            "<plist><dict><key>Program</key><string>/bin/x</string></dict></plist>",
            "<plist><dict><key>ProgramArguments</key><array/></dict></plist>",
            "<plist><dict><key>ProgramArguments</key><array></array></dict></plist>",
            "",
        ] {
            assert_eq!(program_path(plist), None, "{plist:?}");
        }
    }

    #[test]
    fn is_inside_app_bundle_accepts_bundle_executables() {
        assert!(is_inside_app_bundle(Path::new(BUNDLE_EXE)));
        assert!(is_inside_app_bundle(Path::new(OTHER_BUNDLE_EXE)));
    }

    #[test]
    fn is_inside_app_bundle_rejects_other_locations() {
        for exe in [
            DEV_EXE,
            "/Users/me/src/mic-mute/target/aarch64-apple-darwin/release/mic-mute",
            "/Users/me/.cargo/bin/mic-mute",
            TRANSLOCATED_EXE,
            "/Applications/Mic Mute.app",
            "/Applications/Mic Mute.app/Contents/Resources/mic-mute",
            "/Applications/Mic Mute/Contents/MacOS/mic-mute",
            "mic-mute",
            "/",
        ] {
            assert!(!is_inside_app_bundle(Path::new(exe)), "{exe}");
        }
    }

    #[test]
    fn plist_starts_only_the_executable_it_names() {
        let plist = render_plist(BUNDLE_EXE);
        assert!(plist_starts(Some(&plist), Path::new(BUNDLE_EXE)));
        assert!(!plist_starts(Some(&plist), Path::new(OTHER_BUNDLE_EXE)));
    }

    #[test]
    fn plist_starts_nothing_without_a_readable_plist() {
        let exe = Path::new(BUNDLE_EXE);
        assert!(!plist_starts(None, exe));
        assert!(!plist_starts(Some("bplist00binary"), exe));
        assert!(!plist_starts(Some(""), exe));
    }
}
