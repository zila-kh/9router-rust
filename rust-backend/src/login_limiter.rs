use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_FAILS_BEFORE_LOCK: u32 = 5;
const MAX_TRACKED_IPS: usize = 10_000;
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

fn is_stale(entry: &AttemptState, now: Instant) -> bool {
    now.saturating_duration_since(entry.last_fail_at) > FAIL_WINDOW
        && entry.lock_until.is_none_or(|until| now >= until)
}

fn purge_expired(map: &mut HashMap<IpAddr, AttemptState>, ip: IpAddr, now: Instant) {
    if map.get(&ip).is_some_and(|entry| is_stale(entry, now)) {
        map.remove(&ip);
    }
}

fn make_room_for(
    map: &mut HashMap<IpAddr, AttemptState>,
    incoming: IpAddr,
    now: Instant,
    limit: usize,
) {
    if map.contains_key(&incoming) || map.len() < limit {
        return;
    }

    map.retain(|_, entry| !is_stale(entry, now));
    if map.len() < limit {
        return;
    }

    // Prefer evicting the oldest currently-unlocked entry. If every tracked IP
    // is locked, evict the oldest entry so untrusted proxy headers cannot grow
    // this process-global map without bound.
    let victim = map
        .iter()
        .filter(|(_, entry)| entry.lock_until.is_none_or(|until| now >= until))
        .min_by_key(|(_, entry)| entry.last_fail_at)
        .map(|(ip, _)| *ip)
        .or_else(|| {
            map.iter()
                .min_by_key(|(_, entry)| entry.last_fail_at)
                .map(|(ip, _)| *ip)
        });
    if let Some(victim) = victim {
        map.remove(&victim);
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
        retry_after_secs: remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0)),
    }
}

pub fn record_failure(ip: IpAddr) -> u32 {
    let now = Instant::now();
    let mut map = locked_attempts();
    purge_expired(&mut map, ip, now);
    make_room_for(&mut map, ip, now, MAX_TRACKED_IPS);
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

    fn state(last_fail_at: Instant, lock_until: Option<Instant>) -> AttemptState {
        AttemptState {
            fails: 1,
            lock_until,
            lock_level: 0,
            last_fail_at,
        }
    }

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

    #[test]
    fn capacity_cleanup_prefers_stale_then_oldest_unlocked_entries() {
        let now = Instant::now();
        let stale_ip: IpAddr = "198.51.100.1".parse().unwrap();
        let old_ip: IpAddr = "198.51.100.2".parse().unwrap();
        let locked_ip: IpAddr = "198.51.100.3".parse().unwrap();
        let incoming: IpAddr = "198.51.100.4".parse().unwrap();
        let mut map = HashMap::from([
            (
                stale_ip,
                state(
                    now.checked_sub(FAIL_WINDOW + Duration::from_secs(1))
                        .unwrap(),
                    None,
                ),
            ),
            (
                old_ip,
                state(now.checked_sub(Duration::from_secs(20)).unwrap(), None),
            ),
            (
                locked_ip,
                state(
                    now.checked_sub(Duration::from_secs(10)).unwrap(),
                    now.checked_add(Duration::from_secs(30)),
                ),
            ),
        ]);

        make_room_for(&mut map, incoming, now, 3);
        assert!(!map.contains_key(&stale_ip));
        assert!(map.contains_key(&old_ip));
        assert!(map.contains_key(&locked_ip));

        map.insert(incoming, state(now, None));
        let next: IpAddr = "198.51.100.5".parse().unwrap();
        make_room_for(&mut map, next, now, 3);
        assert!(!map.contains_key(&old_ip));
        assert!(map.contains_key(&locked_ip));
        assert!(map.contains_key(&incoming));
    }
}
