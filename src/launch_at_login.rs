use anyhow::Result;
use std::path::PathBuf;

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

pub fn enable() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_path = exe.to_string_lossy();

    let path =
        plist_path().ok_or_else(|| anyhow::anyhow!("Cannot resolve LaunchAgents directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let plist = format!(
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
        exe = exe_path,
    );

    std::fs::write(&path, plist)?;
    log::trace!("Launch at login enabled: wrote {}", path.display());
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
