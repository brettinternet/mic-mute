//! macOS notifications on a mute change (`show_notification`), posted through the
//! UserNotifications framework (`UNUserNotificationCenter`).
//!
//! macOS attributes a notification to the app bundle that posts it, so only the installed
//! `Mic Mute.app` can post one. Outside a bundle, as in a `cargo run` build,
//! `+[UNUserNotificationCenter currentNotificationCenter]` raises an Objective-C exception that
//! aborts the app, so `Notifier` never calls it there (`runs_from_app_bundle`). The installed
//! app's ad-hoc signature is enough: macOS keys the permission on the bundle identifier, which
//! `codesign --sign -` on the bundle also makes the signature's identifier.
//!
//! macOS asks the user once whether Mic Mute may show notifications; until they allow it, and
//! after they decline, none appear. Focus modes hide them too, as does sharing the screen
//! unless System Settings → Notifications allows them then. While Mic Mute is the active app
//! (its About window has focus), macOS doesn't show its banners.
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{msg_send, ClassType};
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNUserNotificationCenter,
};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

/// The identifier of every mute-change notification: each one replaces the one before, so they
/// don't pile up in Notification Center.
const REQUEST_IDENTIFIER: &str = "com.brettinternet.mic-mute.mic-state";

/// What macOS last said about Mic Mute's notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permission {
    /// Not asked in this process yet.
    Unknown,
    /// Asked; macOS hasn't answered yet, e.g. while it shows the user the prompt.
    Pending,
    Granted,
    /// Declined, turned off in System Settings, or a notification failed to post.
    Denied,
}

impl Permission {
    const ALL: [Self; 4] = [Self::Unknown, Self::Pending, Self::Granted, Self::Denied];

    fn to_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Pending => 1,
            Self::Granted => 2,
            Self::Denied => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        Self::ALL
            .into_iter()
            .find(|permission| permission.to_u8() == value)
            .unwrap_or(Self::Unknown)
    }
}

/// A `Permission` the completion handlers set from a background queue and the main thread reads.
#[derive(Clone)]
struct SharedPermission(Arc<AtomicU8>);

impl Default for SharedPermission {
    fn default() -> Self {
        Self(Arc::new(AtomicU8::new(Permission::Unknown.to_u8())))
    }
}

impl SharedPermission {
    fn get(&self) -> Permission {
        Permission::from_u8(self.0.load(Ordering::Acquire))
    }

    fn set(&self, permission: Permission) {
        self.0.store(permission.to_u8(), Ordering::Release);
    }
}

/// The notification title for the microphones' state, the popup's text.
fn title(muted: bool) -> &'static str {
    crate::popup_content::get_mic_mute_description_text(muted)
}

/// Whether a main bundle at `path` with `identifier` is an app bundle macOS can attribute
/// notifications to. A bare executable's main bundle is its directory, without an identifier.
fn is_app_bundle(identifier: Option<&str>, path: &str) -> bool {
    identifier.is_some_and(|identifier| !identifier.is_empty())
        && Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "app")
}

/// Whether this process runs from an app bundle, as the installed app does. It can't change
/// while the process runs, so it's checked once.
fn runs_from_app_bundle() -> bool {
    static IS_BUNDLED: OnceLock<bool> = OnceLock::new();
    *IS_BUNDLED.get_or_init(|| {
        // SAFETY: methods without arguments that return an object. Each is fetched as an
        // `Option`: `mainBundle` is documented to return nil when it can't make the bundle, and
        // the bindings would panic on a nil they declare impossible.
        let bundle: Option<Retained<NSBundle>> =
            unsafe { msg_send![NSBundle::class(), mainBundle] };
        let Some(bundle) = bundle else {
            return false;
        };
        let identifier: Option<Retained<NSString>> =
            unsafe { msg_send![&bundle, bundleIdentifier] };
        let path: Option<Retained<NSString>> = unsafe { msg_send![&bundle, bundlePath] };
        is_app_bundle(
            identifier
                .map(|identifier| identifier.to_string())
                .as_deref(),
            &path.map(|path| path.to_string()).unwrap_or_default(),
        )
    })
}

/// The notification center, or `None` outside an app bundle, where asking for it would abort
/// the app.
fn notification_center() -> Option<Retained<UNUserNotificationCenter>> {
    if !runs_from_app_bundle() {
        return None;
    }
    // SAFETY: `currentNotificationCenter` takes no arguments and returns an object; inside an
    // app bundle it doesn't raise. An `Option`, so a nil center can't panic in the event loop.
    unsafe { msg_send![UNUserNotificationCenter::class(), currentNotificationCenter] }
}

/// Posts a notification on each mute change the user makes, while `show_notification` is on.
/// Main thread only, like the event loop that owns it.
#[derive(Default)]
pub struct Notifier {
    is_enabled: bool,
    permission: SharedPermission,
    /// The permission last logged, so each change is logged once.
    logged: Option<Permission>,
    /// Whether the log already says this copy can't post notifications.
    has_warned_unbundled: bool,
}

impl Notifier {
    pub fn is_enabled(&self) -> bool {
        self.is_enabled
    }

    /// Turning notifications on asks macOS for permission, which prompts the user only the
    /// first time. Repeating the value in use does nothing.
    pub fn set_enabled(&mut self, is_enabled: bool) {
        if is_enabled == self.is_enabled {
            return;
        }
        self.is_enabled = is_enabled;
        if is_enabled {
            self.request_permission();
        }
    }

    /// Posts "Mic off" or "Mic on" for `muted`. Returns at once; macOS shows it.
    pub fn post(&mut self, muted: bool) {
        let Some(center) = notification_center() else {
            self.warn_unbundled();
            return;
        };
        let permission = self.permission.get();
        self.log_permission(permission);
        match permission {
            Permission::Unknown => self.request_permission(),
            Permission::Pending => {
                log::trace!("No mute notification: macOS hasn't answered the permission request")
            }
            // Posting while denied finds out whether the user has allowed it since.
            Permission::Granted | Permission::Denied => {
                if let Err(err) = add_request(&center, muted, &self.permission) {
                    log::error!("Failed to post the mute notification: {}", err);
                }
            }
        }
    }

    fn request_permission(&mut self) {
        let Some(center) = notification_center() else {
            self.warn_unbundled();
            return;
        };
        self.permission.set(Permission::Pending);
        let permission = self.permission.clone();
        // Runs on a background queue: it only records the answer.
        let handler = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
            permission.set(if granted.as_bool() {
                Permission::Granted
            } else {
                Permission::Denied
            });
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert,
            &handler,
        );
    }

    fn log_permission(&mut self, permission: Permission) {
        if self.logged == Some(permission) {
            return;
        }
        self.logged = Some(permission);
        match permission {
            Permission::Granted => log::info!("macOS shows Mic Mute's notifications"),
            Permission::Denied => log::warn!(
                "macOS doesn't show Mic Mute's notifications; allow them in System Settings → \
                 Notifications → Mic Mute"
            ),
            Permission::Unknown | Permission::Pending => {}
        }
    }

    fn warn_unbundled(&mut self) {
        if !self.has_warned_unbundled {
            self.has_warned_unbundled = true;
            log::warn!(
                "show_notification works only in the installed Mic Mute.app: this copy doesn't \
                 run from an app bundle, so it posts no notifications"
            );
        }
    }
}

/// Hands macOS a notification that shows at once, replacing the previous one. Its outcome
/// arrives later, in `permission`.
fn add_request(
    center: &UNUserNotificationCenter,
    muted: bool,
    permission: &SharedPermission,
) -> Result<(), &'static str> {
    // SAFETY: `new` takes no arguments; an `Option`, so a failed allocation can't panic.
    let content: Option<Retained<UNMutableNotificationContent>> =
        unsafe { msg_send![UNMutableNotificationContent::class(), new] };
    let content = content.ok_or("UNMutableNotificationContent returned no content")?;
    content.setTitle(&NSString::from_str(title(muted)));
    let identifier = NSString::from_str(REQUEST_IDENTIFIER);
    let no_trigger: Option<&AnyObject> = None;
    // SAFETY: `requestWithIdentifier:content:trigger:` takes a string, the content, and a
    // nullable trigger (none: show it now), and returns a request. Sent directly: the binding
    // needs the UNNotificationTrigger feature, which adds NSCalendar to objc2-foundation.
    let request: Option<Retained<UNNotificationRequest>> = unsafe {
        msg_send![
            UNNotificationRequest::class(),
            requestWithIdentifier: &*identifier,
            content: &*content,
            trigger: no_trigger
        ]
    };
    let request = request.ok_or("UNNotificationRequest returned no request")?;
    let permission = permission.clone();
    // Runs on a background queue: it only records whether macOS took the notification.
    let handler = RcBlock::new(move |error: *mut NSError| {
        permission.set(if error.is_null() {
            Permission::Granted
        } else {
            Permission::Denied
        });
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&*handler));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_app_bundle_with_an_identifier_can_post() {
        assert!(is_app_bundle(
            Some("com.brettinternet.mic-mute"),
            "/Applications/Mic Mute.app"
        ));
        // `cargo run`: the main bundle is the executable's directory.
        assert!(!is_app_bundle(None, "/Users/me/mic-mute/target/debug"));
        // An executable with an embedded Info.plist outside a bundle.
        assert!(!is_app_bundle(Some("com.example.tool"), "/usr/local/bin"));
        assert!(!is_app_bundle(None, "/Applications/Mic Mute.app"));
        assert!(!is_app_bundle(Some(""), "/Applications/Mic Mute.app"));
    }

    #[test]
    fn permissions_survive_the_atomic_round_trip() {
        let shared = SharedPermission::default();
        assert_eq!(shared.get(), Permission::Unknown);
        for permission in Permission::ALL {
            shared.set(permission);
            assert_eq!(shared.get(), permission);
            assert_eq!(Permission::from_u8(permission.to_u8()), permission);
        }
        assert_eq!(Permission::from_u8(u8::MAX), Permission::Unknown);
    }

    #[test]
    fn notifications_say_mic_off_and_mic_on() {
        assert_eq!(title(true), "Mic off");
        assert_eq!(title(false), "Mic on");
    }

    #[test]
    fn a_new_notifier_is_off_and_asks_nothing() {
        let notifier = Notifier::default();
        assert!(!notifier.is_enabled());
        assert_eq!(notifier.permission.get(), Permission::Unknown);
    }
}
