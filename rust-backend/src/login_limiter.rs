use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_FAILS_BEFORE_LOCK: u32 = 5;
const LOCK_STEPS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1_800),
];
const FAIL_WINDOW: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
struct AttemptState {
    fails: u32,
    lock_until: Option<Instant>,
    lock_level: usize,
    last_fail_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockStatus {
    pub locked: bool,
    pub retry_after_secs: u64,
}

static ATTEMPTS: OnceLock<Mutex<HashMap<IpAddr, AttemptState>>> = OnceLock::new();

fn attempts() -> &'static Mutex<HashMap<IpAddr, AttemptState>> {
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn locked_attempts() -> std::sync::MutexGuard<'static, HashMap<IpAddr, AttemptState>> {
    attempts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn purge_expired(map: &mut HashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) {
    let remove = map.get(&ip).is_some_and(|entry| {
        now.duration_since(entry.last_fail_at) > FAIL_WINDOW
            && entry.lock_until.is_none_or(|until| now >= until)
    });
    if remove {
        map.remove(&ip);
    }
}

pub fn check_lock(ip: IpAddr) -> LockStatus {
    let now = Instant::now();
    let mut map = locked_attempts();
    purge_expired(&mut map, ip, now);
    let Some(entry) = map.get_mut(&ip) else {
        return LockStatus {
            locked: false,
            retry_after_secs: 0,
        };
    };
    let Some(until) = entry.lock_until else {
        return LockStatus {
            locked: false,
            retry_after_secs: 0,
        };
    };
    if now >= until {
        entry.lock_until = None;
        return LockStatus {
            locked: false,
            retry_after_secs: 0,
        };
    }
    let remaining = until.saturating_duration_since(now);
    LockStatus {
        locked: true,
        retry_after_secs: remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() > 0)),
    }
}

pub fn record_failure(ip: IpAddr) -> u32 {
    let now = Instant::now();
    let mut map = locked_attempts();
    purge_expired(&mut map, ip, now);
    let entry = map.entry(ip).or_insert(AttemptState {
        fails: 0,
        lock_until: None,
        lock_level: 0,
        last_fail_at: now,
    });
    entry.fails = entry.fails.saturating_add(1);
    entry.last_fail_at = now;
    if entry.fails >= MAX_FAILS_BEFORE_LOCK {
        let duration = LOCK_STEPS[entry.lock_level.min(LOCK_STEPS.len() - 1)];
        entry.lock_until = now.checked_add(duration);
        entry.lock_level = entry.lock_level.saturating_add(1);
        entry.fails = 0;
    }
    MAX_FAILS_BEFORE_LOCK.saturating_sub(entry.fails)
}

pub fn record_success(ip: IpAddr) {
    locked_attempts().remove(&ip);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifth_failure_starts_a_lock_and_success_clears_it() {
        let ip: IpAddr = "198.51.100.42".parse().expect("test IP");
        record_success(ip);
        for expected in [4, 3, 2, 1] {
            assert_eq!(record_failure(ip), expected);
            assert!(!check_lock(ip).locked);
        }
        assert_eq!(record_failure(ip), 5);
        let lock = check_lock(ip);
        assert!(lock.locked);
        assert!((1..=30).contains(&lock.retry_after_secs));
        record_success(ip);
        assert!(!check_lock(ip).locked);
    }
}
