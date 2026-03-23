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
            if first.elapsed() >= self.timeout {
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
        if !self.deque.is_empty() {
            return Err(*self.deque.front().unwrap() + self.timeout);
        }

        self.deque.push_back(Instant::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_throttle_available_initially() {
        let mut t = Throttle::new(Duration::from_millis(100));
        assert!(t.available());
        assert_eq!(t.size(), 0);
    }

    #[test]
    fn test_throttle_not_available_after_accept() {
        let mut t = Throttle::new(Duration::from_millis(1000));
        assert!(t.available());
        t.accept().unwrap();
        assert!(!t.available());
    }

    #[test]
    fn test_throttle_accept_twice_fails() {
        let mut t = Throttle::new(Duration::from_millis(1000));
        t.accept().unwrap();
        assert!(t.accept().is_err());
    }

    #[test]
    fn test_throttle_available_after_timeout() {
        let mut t = Throttle::new(Duration::from_millis(1));
        t.accept().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(t.available());
    }
}
