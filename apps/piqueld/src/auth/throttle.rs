//! Admission limits for public ceremony/device starts, shared by all listeners.
use super::{AuthError, Result};
use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(60);
const PER_PEER: u32 = 30;
const GLOBAL: u32 = 60;

struct Window {
    started: Instant,
    count: u32,
}
impl Window {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            count: 0,
        }
    }
}

pub(super) struct Throttle {
    global: Window,
    peers: HashMap<Option<IpAddr>, Window>,
}
impl Default for Throttle {
    fn default() -> Self {
        Self {
            global: Window::new(Instant::now()),
            peers: HashMap::new(),
        }
    }
}
impl Throttle {
    // Unix sockets share the None bucket. Never trust caller-supplied forwarding
    // headers. The global budget also bounds this map and pending-state growth:
    // at most 660 admissions within the longest (ten-minute) pending lifetime.
    pub(super) fn admit(&mut self, peer: Option<IpAddr>, now: Instant) -> Result<()> {
        if now.duration_since(self.global.started) >= WINDOW {
            self.global = Window::new(now);
        }
        self.peers
            .retain(|_, window| now.duration_since(window.started) < WINDOW);
        if self.global.count >= GLOBAL {
            return Err(AuthError::Busy);
        }
        let window = self.peers.entry(peer).or_insert_with(|| Window::new(now));
        if window.count >= PER_PEER {
            return Err(AuthError::Busy);
        }
        window.count += 1;
        self.global.count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolates_peers_bounds_global_admission_and_recovers_after_window() {
        let mut throttle = Throttle::default();
        let now = Instant::now();
        let a = Some("192.0.2.1".parse().unwrap());
        let b = Some("192.0.2.2".parse().unwrap());
        for _ in 0..PER_PEER {
            throttle.admit(a, now).unwrap();
        }
        assert!(throttle.admit(a, now).is_err());
        for _ in 0..PER_PEER {
            throttle.admit(b, now).unwrap();
        }
        assert!(throttle.admit(None, now).is_err());
        throttle.admit(a, now + WINDOW).unwrap();
        assert_eq!(throttle.peers.len(), 1);
    }
}
