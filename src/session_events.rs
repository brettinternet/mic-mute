//! macOS session events: the screen locking, the screen saver starting, the Mac or its displays
//! going to sleep, and a switch to another user. Each can swallow a held push-to-talk
//! shortcut's release, and the `mute_on_*` settings mute on them (`mute_triggers.rs`).
//!
//! An observer, an instance of a class defined with `define_class!`, receives them from
//! NSWorkspace's notification center and from the distributed notification center
//! (`com.apple.screenIsLocked`, `com.apple.screensaver.didstart`). Like About's buttons, its
//! methods only record the event; the event loop takes it with `SessionEvents::take` and acts.

use anyhow::{bail, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, Sel};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{
    ns_string, NSDistributedNotificationCenter, NSNotification, NSNotificationCenter,
    NSNotificationName, NSNotificationSuspensionBehavior,
};
use std::sync::atomic::{AtomicU8, Ordering};

/// A session event the app reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// `com.apple.screenIsLocked`, posted when the screen locks.
    ScreenLocked,
    /// `com.apple.screensaver.didstart`.
    ScreenSaverStarted,
    /// `NSWorkspaceWillSleepNotification`: the Mac is going to sleep, also when its lid closes
    /// and no external display keeps it awake.
    WillSleep,
    /// `NSWorkspaceScreensDidSleepNotification`: every display went to sleep.
    ScreensDidSleep,
    /// `NSWorkspaceSessionDidResignActiveNotification`: fast user switching moved to another
    /// user's session.
    SessionResignedActive,
}

/// Where a session event's notification comes from.
enum Source {
    /// NSWorkspace's notification center, with AppKit's name for the notification.
    Workspace(&'static NSNotificationName),
    /// The distributed notification center, with the notification's name.
    Distributed(&'static NSNotificationName),
}

impl SessionEvent {
    pub const ALL: [Self; 5] = [
        Self::ScreenLocked,
        Self::ScreenSaverStarted,
        Self::WillSleep,
        Self::ScreensDidSleep,
        Self::SessionResignedActive,
    ];

    /// What happened, for the log.
    pub fn description(self) -> &'static str {
        match self {
            Self::ScreenLocked => "The screen locked",
            Self::ScreenSaverStarted => "The screen saver started",
            Self::WillSleep => "The Mac is going to sleep",
            Self::ScreensDidSleep => "The displays went to sleep",
            Self::SessionResignedActive => "Another user's session became active",
        }
    }

    /// The observer's method for the event; the notification is its one argument.
    fn selector(self) -> Sel {
        match self {
            Self::ScreenLocked => sel!(screenLocked:),
            Self::ScreenSaverStarted => sel!(screenSaverStarted:),
            Self::WillSleep => sel!(willSleep:),
            Self::ScreensDidSleep => sel!(screensDidSleep:),
            Self::SessionResignedActive => sel!(sessionResignedActive:),
        }
    }

    fn source(self) -> Source {
        // SAFETY (each `unsafe`): reads one of AppKit's notification name constants, which
        // every supported macOS defines.
        match self {
            Self::ScreenLocked => Source::Distributed(ns_string!("com.apple.screenIsLocked")),
            Self::ScreenSaverStarted => {
                Source::Distributed(ns_string!("com.apple.screensaver.didstart"))
            }
            Self::WillSleep => Source::Workspace(unsafe { NSWorkspaceWillSleepNotification }),
            Self::ScreensDidSleep => {
                Source::Workspace(unsafe { NSWorkspaceScreensDidSleepNotification })
            }
            Self::SessionResignedActive => {
                Source::Workspace(unsafe { NSWorkspaceSessionDidResignActiveNotification })
            }
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The events received since the event loop last took them, one bit each. An atomic rather
/// than a main-thread slot, so an event still counts if a center delivers it on another thread.
static RECEIVED: AtomicU8 = AtomicU8::new(0);

fn record(event: SessionEvent) {
    RECEIVED.fetch_or(event.bit(), Ordering::Relaxed);
}

/// The events received since the last call, each once, in [`SessionEvent::ALL`] order.
fn take_received() -> Vec<SessionEvent> {
    let bits = RECEIVED.swap(0, Ordering::Relaxed);
    SessionEvent::ALL
        .into_iter()
        .filter(|event| bits & event.bit() != 0)
        .collect()
}

define_class!(
    /// Receives the session events' notifications, one method per event. The centers call it
    /// from their dispatch, so each method only records its event: no AppKit calls, no locks,
    /// and no panics, which would abort the app inside Objective-C.
    // SAFETY: NSObject has no subclassing requirements, and the class implements no `Drop`.
    // Unnamed: objc2 names it after this module and reuses a class already registered under
    // that name, which can only be this one, so registering it never panics.
    #[unsafe(super(NSObject))]
    struct SessionEventObserver;

    impl SessionEventObserver {
        #[unsafe(method(screenLocked:))]
        fn screen_locked(&self, _notification: Option<&NSNotification>) {
            record(SessionEvent::ScreenLocked);
        }

        #[unsafe(method(screenSaverStarted:))]
        fn screen_saver_started(&self, _notification: Option<&NSNotification>) {
            record(SessionEvent::ScreenSaverStarted);
        }

        #[unsafe(method(willSleep:))]
        fn will_sleep(&self, _notification: Option<&NSNotification>) {
            record(SessionEvent::WillSleep);
        }

        #[unsafe(method(screensDidSleep:))]
        fn screens_did_sleep(&self, _notification: Option<&NSNotification>) {
            record(SessionEvent::ScreensDidSleep);
        }

        #[unsafe(method(sessionResignedActive:))]
        fn session_resigned_active(&self, _notification: Option<&NSNotification>) {
            record(SessionEvent::SessionResignedActive);
        }
    }
);

impl SessionEventObserver {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: NSObject's initializer; the class adds no state to set up.
        unsafe { msg_send![super(this), init] }
    }
}

/// Whether `class` has a method for every event: a center that sends a selector the observer
/// doesn't implement raises an exception, which aborts the app.
fn handles_every_event(class: &AnyClass) -> bool {
    SessionEvent::ALL
        .into_iter()
        .all(|event| class.instance_method(event.selector()).is_some())
}

/// Watches the session events while it lives: it adds its observer to both notification
/// centers on creation and removes it from both on drop. The event loop creates it on the main
/// thread.
pub struct SessionEvents {
    /// Released after `Drop` has removed it from both centers.
    observer: Retained<SessionEventObserver>,
    workspace_center: Retained<NSNotificationCenter>,
    distributed_center: Retained<NSDistributedNotificationCenter>,
}

impl SessionEvents {
    pub fn new() -> Result<Self> {
        if !handles_every_event(SessionEventObserver::class()) {
            bail!("The session event observer lacks a method for an event");
        }
        let observer = SessionEventObserver::new();
        let workspace_center = NSWorkspace::sharedWorkspace().notificationCenter();
        let distributed_center = NSDistributedNotificationCenter::defaultCenter();
        for event in SessionEvent::ALL {
            let selector = event.selector();
            // SAFETY: the observer implements `selector` (`handles_every_event`), which takes
            // the notification, and stays registered no longer than it lives (`Drop`).
            unsafe {
                match event.source() {
                    Source::Workspace(name) => workspace_center.addObserver_selector_name_object(
                        &observer,
                        selector,
                        Some(name),
                        None,
                    ),
                    // `NSNotificationSuspensionBehaviorDeliverImmediately` (4): delivered even
                    // while distributed notifications are suspended for the app.
                    Source::Distributed(name) => distributed_center
                        .addObserver_selector_name_object_suspensionBehavior(
                            &observer,
                            selector,
                            Some(name),
                            None,
                            NSNotificationSuspensionBehavior::DeliverImmediately,
                        ),
                }
            }
        }
        Ok(Self {
            observer,
            workspace_center,
            distributed_center,
        })
    }

    /// The events received since the last call, each once.
    pub fn take(&self) -> Vec<SessionEvent> {
        take_received()
    }
}

impl Drop for SessionEvents {
    fn drop(&mut self) {
        // SAFETY: both centers forget the observer before its release, which follows this body.
        unsafe {
            self.workspace_center.removeObserver(&self.observer);
            self.distributed_center.removeObserver(&self.observer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Explicit: `super::*` and main.rs's `#[macro_use] extern crate objc` both offer one.
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    #[test]
    fn every_event_has_its_own_selector_and_bit() {
        for (index, event) in SessionEvent::ALL.into_iter().enumerate() {
            for other in SessionEvent::ALL.into_iter().skip(index + 1) {
                assert_ne!(event.selector(), other.selector(), "{event:?}, {other:?}");
            }
        }
        let bits = SessionEvent::ALL.map(SessionEvent::bit);
        let combined = bits.iter().fold(0, |all, bit| all | bit);
        assert_eq!(combined.count_ones() as usize, bits.len());
    }

    #[test]
    fn the_observer_class_handles_every_event() {
        assert!(handles_every_event(SessionEventObserver::class()));
    }

    /// Sends `selector` to `observer` as a center does, with a nil notification.
    fn deliver(observer: &SessionEventObserver, selector: Sel) {
        // SAFETY: every event's method takes one object and returns nothing; the result of
        // `performSelector:withObject:` is never read.
        let _: *mut AnyObject = unsafe {
            msg_send![observer, performSelector: selector, withObject: None::<&AnyObject>]
        };
    }

    // The only test that touches RECEIVED. It messages an observer directly, as the centers
    // do; `SessionEvents::new` never runs in tests, because it registers with the system's
    // notification centers.
    #[test]
    fn the_observer_records_each_event_once_until_taken() {
        let observer = SessionEventObserver::new();
        for event in SessionEvent::ALL {
            deliver(&observer, event.selector());
            assert_eq!(take_received(), [event], "{event:?}");
        }

        for event in SessionEvent::ALL {
            deliver(&observer, event.selector());
        }
        // SAFETY: as in `deliver`.
        let _: () = unsafe { msg_send![&observer, screenLocked: None::<&NSNotification>] };

        assert_eq!(take_received(), SessionEvent::ALL);
        assert_eq!(take_received(), []);
    }
}
