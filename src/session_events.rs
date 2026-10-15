//! macOS session events: the screen locking, the screen saver starting, the Mac or its displays
//! going to sleep, and a switch to another user. Each can swallow a held push-to-talk
//! shortcut's release, and the `mute_on_*` settings mute on them (`mute_triggers.rs`).
//!
//! An observer, an instance of a class declared with `ClassDecl`, receives them from
//! NSWorkspace's notification center and from the distributed notification center
//! (`com.apple.screenIsLocked`, `com.apple.screensaver.didstart`). Like About's buttons, its
//! methods only record the event; the event loop takes it with `SessionEvents::take` and acts.

use anyhow::{anyhow, bail, Result};
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

#[link(name = "AppKit", kind = "framework")]
extern "C" {
    static NSWorkspaceWillSleepNotification: id;
    static NSWorkspaceScreensDidSleepNotification: id;
    static NSWorkspaceSessionDidResignActiveNotification: id;
}

/// `NSNotificationSuspensionBehaviorDeliverImmediately = 4`, an `NSUInteger` (Foundation
/// `NSDistributedNotificationCenter.h`): delivered even while distributed notifications are
/// suspended for the app.
const DELIVER_IMMEDIATELY: u64 = 4;

/// The Objective-C class of the observer, declared at runtime.
const OBSERVER_CLASS_NAME: &str = "MicMuteSessionEventObserver";

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
    Workspace(id),
    /// The distributed notification center, with the notification's name.
    Distributed(&'static str),
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

    fn from_selector(selector: Sel) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|event| event.selector() == selector)
    }

    /// # Safety
    ///
    /// Reads AppKit's notification name constants.
    unsafe fn source(self) -> Source {
        match self {
            Self::ScreenLocked => Source::Distributed("com.apple.screenIsLocked"),
            Self::ScreenSaverStarted => Source::Distributed("com.apple.screensaver.didstart"),
            Self::WillSleep => Source::Workspace(NSWorkspaceWillSleepNotification),
            Self::ScreensDidSleep => Source::Workspace(NSWorkspaceScreensDidSleepNotification),
            Self::SessionResignedActive => {
                Source::Workspace(NSWorkspaceSessionDidResignActiveNotification)
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

/// The implementation of every observer method. The notification centers call it from their
/// dispatch, so it only records the event its selector names: no AppKit calls, no locks, and no
/// panics, which can't unwind into Objective-C and would abort the app.
extern "C" fn session_event_received(_observer: &Object, selector: Sel, _notification: id) {
    if let Some(event) = SessionEvent::from_selector(selector) {
        record(event);
    }
}

/// Whether `class` has a method for every event, so no notification reaches a missing one.
fn handles_every_event(class: &Class) -> bool {
    SessionEvent::ALL
        .into_iter()
        .all(|event| class.instance_method(event.selector()).is_some())
}

/// The observer class, declared and registered on first use.
fn observer_class() -> Result<&'static Class> {
    static CLASS: OnceLock<Option<&'static Class>> = OnceLock::new();
    CLASS
        .get_or_init(declare_observer_class)
        .ok_or_else(|| anyhow!("Failed to declare the Objective-C class {OBSERVER_CLASS_NAME}"))
}

fn declare_observer_class() -> Option<&'static Class> {
    let superclass = Class::get("NSObject")?;
    let Some(mut decl) = ClassDecl::new(OBSERVER_CLASS_NAME, superclass) else {
        // The name is taken. Use that class only if it handles every event.
        return Class::get(OBSERVER_CLASS_NAME).filter(|class| handles_every_event(class));
    };
    for event in SessionEvent::ALL {
        // SAFETY: `session_event_received` takes the receiver, the selector, and the one object
        // argument (the NSNotification) of every event's method, and returns nothing.
        unsafe {
            decl.add_method(
                event.selector(),
                session_event_received as extern "C" fn(&Object, Sel, id),
            );
        }
    }
    Some(decl.register())
}

/// NSWorkspace's notification center, or nil.
///
/// # Safety
///
/// Main thread only.
unsafe fn workspace_center() -> id {
    let Some(class) = Class::get("NSWorkspace") else {
        return nil;
    };
    let workspace: id = msg_send![class, sharedWorkspace];
    msg_send![workspace, notificationCenter]
}

/// The distributed notification center, or nil.
fn distributed_center() -> id {
    let Some(class) = Class::get("NSDistributedNotificationCenter") else {
        return nil;
    };
    // SAFETY: `defaultCenter` returns the shared center, which the caller doesn't own.
    unsafe { msg_send![class, defaultCenter] }
}

/// Watches the session events while it lives: it adds its observer to both notification
/// centers on creation and removes it on drop. Main thread only: it holds an Objective-C
/// object, so it is neither `Send` nor `Sync`.
pub struct SessionEvents {
    /// Retained; released on drop, once neither center refers to it.
    observer: id,
}

impl SessionEvents {
    pub fn new() -> Result<Self> {
        let class = observer_class()?;
        // SAFETY: `new` returns a retained instance, which `Drop` releases.
        let observer: id = unsafe { msg_send![class, new] };
        if observer.is_null() {
            bail!("Failed to create the session event observer");
        }
        let workspace = unsafe { workspace_center() };
        let distributed = distributed_center();
        if workspace.is_null() || distributed.is_null() {
            unsafe {
                let _: () = msg_send![observer, release];
            }
            bail!("Failed to find the workspace or the distributed notification center");
        }
        for event in SessionEvent::ALL {
            let selector = event.selector();
            // SAFETY: main thread; the observer implements `selector` (`handles_every_event`)
            // and stays registered no longer than it lives (`Drop`).
            unsafe {
                match event.source() {
                    Source::Workspace(name) => {
                        let _: () = msg_send![workspace, addObserver: observer
                                                         selector: selector
                                                             name: name
                                                           object: nil];
                    }
                    Source::Distributed(name) => {
                        let name = NSString::alloc(nil).init_str(name);
                        let _: () = msg_send![distributed, addObserver: observer
                                                           selector: selector
                                                               name: name
                                                             object: nil
                                                 suspensionBehavior: DELIVER_IMMEDIATELY];
                        let _: () = msg_send![name, release];
                    }
                }
            }
        }
        Ok(Self { observer })
    }

    /// The events received since the last call, each once.
    pub fn take(&self) -> Vec<SessionEvent> {
        take_received()
    }
}

impl Drop for SessionEvents {
    fn drop(&mut self) {
        // SAFETY: main thread (`SessionEvents` isn't `Send`); both centers forget the observer
        // before its release.
        unsafe {
            let workspace = workspace_center();
            let _: () = msg_send![workspace, removeObserver: self.observer];
            let _: () = msg_send![distributed_center(), removeObserver: self.observer];
            let _: () = msg_send![self.observer, release];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_has_its_own_selector_and_bit() {
        for event in SessionEvent::ALL {
            assert_eq!(
                SessionEvent::from_selector(event.selector()),
                Some(event),
                "{event:?}"
            );
        }
        let bits = SessionEvent::ALL.map(SessionEvent::bit);
        let combined = bits.iter().fold(0, |all, bit| all | bit);
        assert_eq!(combined.count_ones() as usize, bits.len());
    }

    #[test]
    fn other_selectors_name_no_event() {
        assert_eq!(SessionEvent::from_selector(sel!(description)), None);
    }

    #[test]
    fn the_observer_class_handles_every_event() {
        assert!(handles_every_event(observer_class().unwrap()));
    }

    // The only test that touches RECEIVED. It messages an observer directly, as the centers
    // do; `SessionEvents::new` never runs in tests, because it registers with the system's
    // notification centers.
    #[test]
    fn the_observer_records_each_event_once_until_taken() {
        let class = observer_class().unwrap();
        unsafe {
            let observer: id = msg_send![class, new];
            for event in SessionEvent::ALL {
                let _: id = msg_send![observer, performSelector: event.selector() withObject: nil];
            }
            let _: () = msg_send![observer, screenLocked: nil];
            let _: () = msg_send![observer, release];
        }

        assert_eq!(take_received(), SessionEvent::ALL);
        assert_eq!(take_received(), []);
    }
}
