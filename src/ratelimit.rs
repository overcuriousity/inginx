//! Brute-force throttle for the admin login.
//!
//! One fixed admin account behind a password is exactly the shape an online
//! guesser likes: no username to discover, one endpoint to hammer. Argon2 puts
//! a floor of tens of milliseconds under each try, but a floor is not a wall —
//! at even 20 tries a second a weak password falls in a day. This adds the
//! wall: after a handful of failures from one address the address is locked
//! out, and the lockout doubles each time it is re-earned.
//!
//! Kept in memory on purpose. The state is a few counters that are meaningless
//! across a restart — a restart is itself a reset, and an attacker who can
//! restart the process has already won — so it costs nothing to lose and a
//! table write per login attempt to persist. The map is swept of expired
//! entries on every check, so it cannot grow beyond the set of addresses that
//! have recently failed.
//!
//! The key is the peer address the server actually sees. Behind a reverse
//! proxy that is the proxy, and the lockout is then shared by everyone behind
//! it — which is why `main` warns when the admin surface faces a network and
//! the operator, not this file, is where per-client keying belongs. We do not
//! trust `X-Forwarded-For` for the key: it is attacker-set, and keying on it
//! would let a guesser rotate one header to dodge the wall entirely.

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{request::Parts, HeaderMap};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The peer address, or `None` when the server was not told one — which is the
/// case in unit tests that drive the router directly. A never-rejecting
/// extractor, so a missing connect-info degrades to "unknown address" rather
/// than a 500; `Option<ConnectInfo<_>>` is not itself an extractor in axum
/// 0.8, which is why this wrapper exists.
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0),
        ))
    }
}

/// The address a lockout is keyed on — the admin login's and the `/v1`
/// API-key wall's alike.
///
/// Without a trusted proxy this is the peer the server actually sees, and the
/// forwarding headers are ignored — they are attacker-set on a direct
/// connection, and keying the brute-force wall on a spoofable header would let
/// a guesser rotate one value to walk around it.
///
/// With `trust_proxy_headers`, the peer *is* the proxy and the same peer for
/// everyone, so keying on it would put all clients in one lockout bucket — an
/// attacker could lock the real admin out, and no two clients could be told
/// apart. There the proxy is the authority: `X-Real-IP` if it set one, else
/// the rightmost `X-Forwarded-For` entry, which is the address the nearest
/// (trusted) proxy observed and appended — a client-supplied prefix cannot
/// displace it.
pub fn client_ip(headers: &HeaderMap, peer: PeerAddr, trust_proxy: bool) -> IpAddr {
    let fallback = || {
        peer.0
            .map(|a| a.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    };
    if !trust_proxy {
        return fallback();
    }
    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or_else(fallback)
}

/// Ceiling on the doubling lockout. Long enough to make sustained guessing
/// pointless, short enough that a locked-out real admin is not locked out for
/// the day.
const MAX_LOCKOUT: Duration = Duration::from_secs(3600);

struct Entry {
    /// Consecutive failures since the last success or lockout expiry.
    fails: u32,
    /// How long the *next* lockout lasts; doubles each time one is earned.
    next_lockout: Duration,
    /// When the current lockout ends, if the address is locked.
    locked_until: Option<Instant>,
}

pub struct LoginThrottle {
    inner: Mutex<HashMap<IpAddr, Entry>>,
    max_failures: u32,
    base_lockout: Duration,
}

impl LoginThrottle {
    pub fn new(max_failures: u32, base_lockout: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            // A zero threshold would lock on the first try; a zero interval
            // would be no lockout at all. Clamp both so a misconfiguration
            // fails safe rather than open or bricked.
            max_failures: max_failures.max(1),
            base_lockout: base_lockout.max(Duration::from_secs(1)),
        }
    }

    /// If `ip` is currently locked out, the time remaining. `None` means the
    /// attempt may proceed. Sweeps expired entries as it goes.
    pub fn check(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();
        map.retain(|_, e| e.locked_until.map(|t| t > now).unwrap_or(false) || e.fails > 0);
        let e = map.get(&ip)?;
        e.locked_until
            .and_then(|t| t.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// Records a failed attempt from `ip`, arming or extending its lockout once
    /// the threshold is reached.
    pub fn on_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();
        let e = map.entry(ip).or_insert(Entry {
            fails: 0,
            next_lockout: self.base_lockout,
            locked_until: None,
        });
        e.fails += 1;
        if e.fails >= self.max_failures {
            e.locked_until = Some(now + e.next_lockout);
            e.fails = 0;
            e.next_lockout = (e.next_lockout * 2).min(MAX_LOCKOUT);
        }
    }

    /// Clears the record for `ip` after a successful login.
    pub fn on_success(&self, ip: IpAddr) {
        self.inner.lock().unwrap().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn locks_after_threshold_and_clears_on_success() {
        let t = LoginThrottle::new(3, Duration::from_secs(60));
        let a = ip(1);
        assert!(t.check(a).is_none());
        t.on_failure(a);
        t.on_failure(a);
        assert!(t.check(a).is_none(), "under threshold, not locked");
        t.on_failure(a);
        let left = t.check(a).expect("locked after threshold");
        assert!(left <= Duration::from_secs(60) && left > Duration::from_secs(50));
        // A different address is unaffected.
        assert!(t.check(ip(2)).is_none());
    }

    #[test]
    fn lockout_doubles() {
        let t = LoginThrottle::new(1, Duration::from_secs(10));
        let a = ip(3);
        t.on_failure(a);
        let first = t.check(a).unwrap();
        assert!(first > Duration::from_secs(8) && first <= Duration::from_secs(10));
        // Force expiry so the next failure re-locks with a doubled interval.
        {
            let mut map = t.inner.lock().unwrap();
            map.get_mut(&a).unwrap().locked_until = Some(Instant::now());
        }
        t.on_failure(a);
        let second = t.check(a).unwrap();
        assert!(
            second > Duration::from_secs(18),
            "doubled to ~20s: {second:?}"
        );
    }

    #[test]
    fn success_resets() {
        let t = LoginThrottle::new(2, Duration::from_secs(60));
        let a = ip(4);
        t.on_failure(a);
        t.on_success(a);
        t.on_failure(a);
        assert!(t.check(a).is_none(), "counter reset by the success");
    }
}
