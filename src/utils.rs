use libc::c_void;
use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

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
}

pub fn arc_lock<T>(value: T) -> Arc<RwLock<T>> {
    let rwlock = RwLock::new(value);
    Arc::new(rwlock)
}

// Modified from https://github.com/SOF3/throttle
pub struct Throttle {
    timeout: Duration,
    deque: VecDeque<Instant>,
}

impl Throttle {
    pub fn new(timeout: Duration) -> Throttle {
        Throttle {
            timeout,
            deque: Default::default(),
        }
    }

    fn flush(&mut self) {
        while let Some(first) = self.deque.front() {
            if first.elapsed() >= self.timeout.clone() {
                self.deque.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn size(&mut self) -> usize {
        self.flush();
        self.deque.len()
    }

    pub fn available(&mut self) -> bool {
        self.size() < 1
    }

    pub fn accept(&mut self) -> Result<(), Instant> {
        self.flush();
        if self.deque.len() >= 1 {
            return Err(self.deque.front().unwrap().clone() + self.timeout.clone());
        }

        self.deque.push_back(Instant::now());
        Ok(())
    }
}
