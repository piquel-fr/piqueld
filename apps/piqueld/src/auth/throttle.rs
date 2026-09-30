//! Per-peer admission limits for public ceremony/device starts, shared by all
//! listeners. There is deliberately no daemon-wide cap: one caller must not be
//! able to block sign-in for everyone else. The daemon is never exposed to the
//! internet; pending state stays bounded by the ceremony and device capacities.
use super::{AuthError, Result};
use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_mins(1);
const PER_PEER: u32 = 30;

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

#[derive(Default)]
pub(super) struct Throttle {
    peers: HashMap<Option<IpAddr>, Window>,
}
impl Throttle {
    // Unix sockets share the None bucket. Never trust caller-supplied forwarding
    // headers.
    pub(super) fn admit(&mut self, peer: Option<IpAddr>, now: Instant) -> Result<()> {
        self.peers
            .retain(|_, window| now.duration_since(window.started) < WINDOW);
        let window = self
            .peers
            .entry(peer.map(bucket))
            .or_insert_with(|| Window::new(now));
        if window.count >= PER_PEER {
            return Err(AuthError::Busy);
        }
        window.count += 1;
        Ok(())
    }
}

/// Groups IPv6 peers by /64, the smallest prefix normally assigned to one
/// client, so rotating addresses within it cannot multiply the allowance.
fn bucket(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V4(_) => peer,
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
            || IpAddr::V6((address.to_bits() & !u128::from(u64::MAX)).into()),
            IpAddr::V4,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolates_peers_without_a_shared_cap_and_recovers_after_window() {
        let mut throttle = Throttle::default();
        let now = Instant::now();
        let a = Some("192.0.2.1".parse().unwrap());
        for _ in 0..PER_PEER {
            throttle.admit(a, now).unwrap();
        }
        assert!(throttle.admit(a, now).is_err());
        // Exhausted peers never consume anyone else's allowance.
        for host in 2..10 {
            let peer = Some(format!("192.0.2.{host}").parse().unwrap());
            for _ in 0..PER_PEER {
                throttle.admit(peer, now).unwrap();
            }
        }
        throttle.admit(None, now).unwrap();
        throttle.admit(a, now + WINDOW).unwrap();
        assert_eq!(throttle.peers.len(), 1);
    }

    #[test]
    fn groups_ipv6_peers_by_prefix() {
        let mut throttle = Throttle::default();
        let now = Instant::now();
        for host in 0..PER_PEER {
            let peer = format!("2001:db8:0:1::{host:x}").parse().unwrap();
            throttle.admit(Some(peer), now).unwrap();
        }
        let same_prefix = "2001:db8:0:1:ffff::1".parse().unwrap();
        assert!(throttle.admit(Some(same_prefix), now).is_err());
        let other_prefix = "2001:db8:0:2::1".parse().unwrap();
        throttle.admit(Some(other_prefix), now).unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(bucket(mapped), "192.0.2.1".parse::<IpAddr>().unwrap());
    }
}
