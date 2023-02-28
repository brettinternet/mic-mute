use libc::c_void;
use std::sync::{Arc, RwLock};

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

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    unsafe {
        let e = CGEventCreate(0 as _);
        let point = CGEventGetLocation(e);
        CFRelease(e);
        Some((point.x as _, point.y as _))
    }
    // let mut pt: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    // let screen: id = unsafe { msg_send![class!(NSScreen), currentScreenForMouseLocation] };
    // let frame: NSRect = unsafe { msg_send![screen, frame] };
    // pt.x -= frame.origin.x;
    // pt.y -= frame.origin.y;
    // Some((pt.x as _, pt.y as _))
}

pub fn arc_lock<T>(value: T) -> Arc<RwLock<T>> {
    let rwlock = RwLock::new(value);
    Arc::new(rwlock)
}
