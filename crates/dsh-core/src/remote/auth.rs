//! Passwords, sessions, and login rate limiting for the remote-access proxy.
//!
//! Everything here is in-memory: sessions deliberately do not survive a
//! launcher restart, so a stolen token expires with the process that issued
//! it. Passwords are owned and persisted by `RemoteService`.

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    time::{Duration, Instant},
};

/// Numeric password length shown on the remote page and typed on the phone.
pub const PASSWORD_LENGTH: usize = 8;
const PASSWORD_SPACE: u64 = 100_000_000;

const MAX_ATTEMPTS_PER_IP: u32 = 5;
const IP_LOCKOUT: Duration = Duration::from_secs(60);
const IP_FAILURE_RETENTION: Duration = Duration::from_secs(10 * 60);
const GLOBAL_WINDOW: Duration = Duration::from_secs(60);
const GLOBAL_MAX_FAILURES: usize = 30;
const GLOBAL_LOCKOUT: Duration = Duration::from_secs(60);

/// Eight-digit numeric password derived from a random UUID (CSPRNG-backed).
/// Leading zeros are preserved so every position stays uniform.
pub fn generate_password() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let value =
        u64::from_be_bytes(bytes[..8].try_into().expect("uuid is 16 bytes")) % PASSWORD_SPACE;
    format!("{value:0PASSWORD_LENGTH$}")
}

/// True when the value is exactly eight ASCII digits.
pub fn is_valid_password(value: &str) -> bool {
    value.len() == PASSWORD_LENGTH && value.bytes().all(|b| b.is_ascii_digit())
}

/// Length-independent constant-time comparison; length equality is checked
/// first and is not secret.
pub fn password_matches(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in expected.bytes().zip(candidate.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Opaque session tokens issued after a successful login.
#[derive(Debug, Default)]
pub struct SessionStore {
    issued: HashMap<String, Instant>,
}

impl SessionStore {
    pub fn create(&mut self) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.issued.insert(token.clone(), Instant::now());
        token
    }

    pub fn validate(&self, token: &str) -> bool {
        self.issued.contains_key(token)
    }

    pub fn revoke(&mut self, token: &str) {
        self.issued.remove(token);
    }

    /// Drops every session, e.g. after the password rotates.
    pub fn clear(&mut self) {
        self.issued.clear();
    }
}

#[derive(Debug)]
struct IpFailures {
    count: u32,
    locked_until: Option<Instant>,
    last_failure: Instant,
}

/// Login brute-force protection: per-IP lockout after repeated failures plus
/// a short global lockout when failures arrive from many sources at once.
#[derive(Debug, Default)]
pub struct RateLimiter {
    per_ip: HashMap<IpAddr, IpFailures>,
    recent_failures: VecDeque<Instant>,
    global_locked_until: Option<Instant>,
}

impl RateLimiter {
    /// True when a login attempt from this address is allowed right now.
    pub fn allowed(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.global_locked_until.is_some_and(|until| until > now) {
            return false;
        }
        if let Some(entry) = self.per_ip.get_mut(&ip) {
            if entry.locked_until.is_some_and(|until| until > now) {
                return false;
            }
            if entry.locked_until.is_some() {
                // Lock expired; start a fresh window.
                entry.count = 0;
                entry.locked_until = None;
            }
        }
        true
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        // A public listener can see an effectively unbounded set of visitor
        // addresses. Drop inactive histories so random failed logins cannot
        // grow this process-lifetime map forever.
        self.per_ip.retain(|_, entry| {
            entry.locked_until.is_some_and(|until| until > now)
                || now.saturating_duration_since(entry.last_failure) <= IP_FAILURE_RETENTION
        });
        let entry = self.per_ip.entry(ip).or_insert(IpFailures {
            count: 0,
            locked_until: None,
            last_failure: now,
        });
        entry.count = entry.count.saturating_add(1);
        entry.last_failure = now;
        if entry.count >= MAX_ATTEMPTS_PER_IP {
            entry.locked_until = Some(now + IP_LOCKOUT);
            entry.count = 0;
        }
        while self
            .recent_failures
            .front()
            .is_some_and(|at| now.duration_since(*at) > GLOBAL_WINDOW)
        {
            self.recent_failures.pop_front();
        }
        self.recent_failures.push_back(now);
        if self.recent_failures.len() >= GLOBAL_MAX_FAILURES {
            self.global_locked_until = Some(now + GLOBAL_LOCKOUT);
            self.recent_failures.clear();
        }
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.per_ip.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn generated_passwords_are_eight_digits() {
        for _ in 0..100 {
            let password = generate_password();
            assert!(is_valid_password(&password), "{password}");
        }
    }

    #[test]
    fn password_validation_is_strict() {
        assert!(!is_valid_password("1234567"));
        assert!(!is_valid_password("123456789"));
        assert!(!is_valid_password("1234567a"));
        assert!(!is_valid_password("１２３４５６７８"));
        assert!(is_valid_password("00000000"));
    }

    #[test]
    fn comparison_does_not_leak_through_length() {
        assert!(password_matches("12345678", "12345678"));
        assert!(!password_matches("12345678", "12345679"));
        assert!(!password_matches("12345678", "1234567"));
        assert!(!password_matches("12345678", "123456780"));
    }

    #[test]
    fn sessions_issue_validate_revoke_and_clear() {
        let mut store = SessionStore::default();
        let token = store.create();
        assert!(store.validate(&token));
        store.revoke(&token);
        assert!(!store.validate(&token));
        let token = store.create();
        store.clear();
        assert!(!store.validate(&token));
    }

    #[test]
    fn repeated_failures_lock_the_source_ip() {
        let mut limiter = RateLimiter::default();
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
        let start = Instant::now();
        for _ in 0..MAX_ATTEMPTS_PER_IP {
            assert!(limiter.allowed(ip, start));
            limiter.record_failure(ip, start);
        }
        assert!(!limiter.allowed(ip, start));
        assert!(limiter.allowed(ip, start + IP_LOCKOUT + Duration::from_secs(1)));
    }

    #[test]
    fn success_resets_the_failure_window() {
        let mut limiter = RateLimiter::default();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let now = Instant::now();
        for _ in 0..MAX_ATTEMPTS_PER_IP - 1 {
            limiter.record_failure(ip, now);
        }
        limiter.record_success(ip);
        for _ in 0..MAX_ATTEMPTS_PER_IP - 1 {
            assert!(limiter.allowed(ip, now));
            limiter.record_failure(ip, now);
        }
        assert!(limiter.allowed(ip, now));
    }

    #[test]
    fn distributed_failures_trigger_a_global_lockout() {
        let mut limiter = RateLimiter::default();
        let now = Instant::now();
        for index in 0..GLOBAL_MAX_FAILURES {
            let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, index as u8));
            limiter.record_failure(ip, now);
        }
        let fresh = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9));
        assert!(!limiter.allowed(fresh, now));
        assert!(limiter.allowed(fresh, now + GLOBAL_LOCKOUT + Duration::from_secs(1)));
    }

    #[test]
    fn inactive_source_histories_are_pruned() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();
        let stale = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1));
        let current = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 2));
        limiter.record_failure(stale, start);

        limiter.record_failure(
            current,
            start + IP_FAILURE_RETENTION + Duration::from_secs(1),
        );

        assert!(!limiter.per_ip.contains_key(&stale));
        assert!(limiter.per_ip.contains_key(&current));
    }
}
