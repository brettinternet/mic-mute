use crate::utils::write_atomically;
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const PLIST_LABEL: &str = "com.brettinternet.mic-mute";

fn plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", PLIST_LABEL))
    })
}

pub fn is_enabled() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

/// Makes the LaunchAgent start the running executable at login, creating it if needed.
///
/// Writes only when the plist is missing or starts another program. A binary outside an app
/// bundle (such as `target/debug/mic-mute`) never repoints a LaunchAgent that starts an app
/// bundle, so a development run can't take over the installed app's login item.
pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("Failed to resolve the running executable")?;
    point_at(&exe)
}

/// At startup, repoints an existing LaunchAgent at the running app bundle when the app has
/// moved. Does nothing outside an app bundle or when Launch at Login is off.
pub fn refresh() -> Result<()> {
    let exe = std::env::current_exe().context("Failed to resolve the running executable")?;
    if !is_inside_app_bundle(&exe) {
        log::trace!(
            "{} isn't inside an app bundle; leaving the LaunchAgent unchanged",
            exe.display()
        );
        return Ok(());
    }
    if !is_enabled() {
        return Ok(());
    }
    point_at(&exe)
}

fn point_at(exe: &Path) -> Result<()> {
    let path = plist_path().context("Cannot resolve LaunchAgents directory")?;
    let existing = read_plist(&path)?;
    match plan_enable(existing.as_deref(), exe) {
        PlistAction::Keep => {
            log::trace!("Launch at login already starts {}", exe.display());
        }
        PlistAction::KeepBundle(program) => log::info!(
            "Launch at login keeps starting {}: {} isn't inside an app bundle",
            program,
            exe.display()
        ),
        PlistAction::KeepUnknown => log::info!(
            "Launch at login left {} unchanged: can't tell what it starts, and {} isn't inside an app bundle",
            path.display(),
            exe.display()
        ),
        PlistAction::Write => {
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
        }
    }
    Ok(())
}

pub fn disable() -> Result<()> {
    if let Some(path) = plist_path() {
        if path.exists() {
            std::fs::remove_file(&path)?;
            log::trace!("Launch at login disabled: removed {}", path.display());
        }
    }
    Ok(())
}

pub fn set(enabled: bool) -> Result<()> {
    if enabled {
        enable()
    } else {
        disable()
    }
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

/// The LaunchAgent plist's contents, or `None` when there is none.
fn read_plist(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// What `enable` does with the LaunchAgent plist.
#[derive(Debug, PartialEq, Eq)]
enum PlistAction {
    /// Write a plist that starts the running executable.
    Write,
    /// The plist already starts the running executable.
    Keep,
    /// The plist starts this app bundle executable, and the running one isn't in a bundle.
    KeepBundle(String),
    /// The plist's program can't be read, and the running executable isn't in a bundle.
    KeepUnknown,
}

fn plan_enable(existing: Option<&str>, exe: &Path) -> PlistAction {
    let Some(existing) = existing else {
        return PlistAction::Write;
    };
    let program = program_path(existing);
    if program.as_deref() == Some(&*exe.to_string_lossy()) {
        return PlistAction::Keep;
    }
    if is_inside_app_bundle(exe) {
        return PlistAction::Write;
    }
    match program {
        Some(program) if is_inside_app_bundle(Path::new(&program)) => {
            PlistAction::KeepBundle(program)
        }
        Some(_) => PlistAction::Write,
        None => PlistAction::KeepUnknown,
    }
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

    const BUNDLE_EXE: &str = "/Applications/Mic Mute.app/Contents/MacOS/mic-mute";
    const OTHER_BUNDLE_EXE: &str = "/Users/me/Applications/Mic Mute.app/Contents/MacOS/mic-mute";
    const DEV_EXE: &str = "/Users/me/src/mic-mute/target/debug/mic-mute";
    const OTHER_DEV_EXE: &str = "/Users/me/src/mic-mute/target/release/mic-mute";
    const TRANSLOCATED_EXE: &str = "/private/var/folders/ab/xyz/T/AppTranslocation/0A1B2C3D/d/Mic Mute.app/Contents/MacOS/mic-mute";

    #[test]
    fn test_plist_path_is_in_launch_agents() {
        let path = plist_path().unwrap();
        assert!(path.to_string_lossy().contains("LaunchAgents"));
        assert!(path.to_string_lossy().ends_with(".plist"));
    }

    #[test]
    fn test_is_enabled_returns_bool() {
        // Just verify it doesn't panic and returns a bool
        let _ = is_enabled();
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
    fn plan_enable_writes_when_there_is_no_plist() {
        assert_eq!(plan_enable(None, Path::new(DEV_EXE)), PlistAction::Write);
        assert_eq!(plan_enable(None, Path::new(BUNDLE_EXE)), PlistAction::Write);
    }

    #[test]
    fn plan_enable_keeps_a_plist_that_already_starts_the_executable() {
        for exe in [BUNDLE_EXE, DEV_EXE] {
            let plist = render_plist(exe);
            assert_eq!(plan_enable(Some(&plist), Path::new(exe)), PlistAction::Keep);
        }
    }

    #[test]
    fn plan_enable_never_points_a_bundle_plist_at_a_dev_binary() {
        let plist = render_plist(BUNDLE_EXE);

        assert_eq!(
            plan_enable(Some(&plist), Path::new(DEV_EXE)),
            PlistAction::KeepBundle(BUNDLE_EXE.to_string())
        );
        assert_eq!(
            plan_enable(Some(&plist), Path::new(TRANSLOCATED_EXE)),
            PlistAction::KeepBundle(BUNDLE_EXE.to_string())
        );
    }

    #[test]
    fn plan_enable_follows_a_moved_bundle() {
        let plist = render_plist(BUNDLE_EXE);

        assert_eq!(
            plan_enable(Some(&plist), Path::new(OTHER_BUNDLE_EXE)),
            PlistAction::Write
        );
    }

    #[test]
    fn plan_enable_replaces_dev_and_translocated_paths() {
        for stale in [DEV_EXE, TRANSLOCATED_EXE] {
            let plist = render_plist(stale);
            assert_eq!(
                plan_enable(Some(&plist), Path::new(BUNDLE_EXE)),
                PlistAction::Write
            );
            assert_eq!(
                plan_enable(Some(&plist), Path::new(OTHER_DEV_EXE)),
                PlistAction::Write
            );
        }
    }

    #[test]
    fn plan_enable_leaves_an_unreadable_plist_to_bundles() {
        let plist = "bplist00binary";

        assert_eq!(
            plan_enable(Some(plist), Path::new(DEV_EXE)),
            PlistAction::KeepUnknown
        );
        assert_eq!(
            plan_enable(Some(plist), Path::new(BUNDLE_EXE)),
            PlistAction::Write
        );
    }
}
