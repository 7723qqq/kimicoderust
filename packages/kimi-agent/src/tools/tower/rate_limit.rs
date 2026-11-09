use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS: u64 = 2_000;
pub const RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS: u64 = 180_000;
pub const TOWER_SPAWN_PAUSE_MS: u64 = 60_000;
pub const TOWER_DEFAULT_MAX_RUNNING_AGENTS: usize = 256;

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerRateLimitSnapshot {
    pub budget: usize,
    pub inflight: usize,
    pub blocked_until: Option<u64>,
}

#[derive(Debug)]
struct TowerRateLimitInner {
    capacity: Option<usize>,
    inflight: usize,
    blocked_until: Option<u64>,
    last_rate_limit_at: Option<u64>,
    last_shrink_at: Option<u64>,
    last_recovery_at: Option<u64>,
}

pub struct TowerRateLimit {
    inner: Mutex<TowerRateLimitInner>,
    now: Clock,
}

impl Default for TowerRateLimit {
    fn default() -> Self {
        Self::new()
    }
}

impl TowerRateLimit {
    pub fn new() -> Self {
        Self::with_clock(now_ms)
    }

    fn with_clock(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            inner: Mutex::new(TowerRateLimitInner {
                capacity: None,
                inflight: 0,
                blocked_until: None,
                last_rate_limit_at: None,
                last_shrink_at: None,
                last_recovery_at: None,
            }),
            now: Arc::new(now),
        }
    }

    pub fn global() -> Arc<Self> {
        static GLOBAL: LazyLock<Arc<TowerRateLimit>> =
            LazyLock::new(|| Arc::new(TowerRateLimit::new()));
        GLOBAL.clone()
    }

    pub fn report_rate_limited(&self) {
        let now = (self.now)();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.inflight > 0 {
            match g.capacity {
                None => {
                    g.capacity = Some((g.inflight - 1).max(1));
                    g.last_shrink_at = Some(now);
                }
                Some(capacity)
                    if g.last_shrink_at.is_none()
                        || now.saturating_sub(g.last_shrink_at.unwrap_or(0))
                            >= RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS =>
                {
                    g.capacity = Some(capacity.saturating_sub(1).max(1));
                    g.last_shrink_at = Some(now);
                }
                Some(_) => {}
            }
        }
        g.last_rate_limit_at = Some(now);
        g.blocked_until = Some(now.saturating_add(TOWER_SPAWN_PAUSE_MS));
    }

    pub fn report_success(&self) {
        let now = (self.now)();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.blocked_until = None;
        Self::maybe_recover(&mut g, now);
    }

    fn maybe_recover(g: &mut TowerRateLimitInner, now: u64) {
        let Some(last_limit) = g.last_rate_limit_at else {
            return;
        };
        let next_recovery = last_limit
            .max(g.last_recovery_at.unwrap_or(0))
            .saturating_add(RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS);
        if now >= next_recovery {
            if let Some(capacity) = g.capacity {
                g.capacity = Some(capacity.saturating_add(1));
            }
            g.last_recovery_at = Some(now);
        }
    }

    fn budget_locked(g: &TowerRateLimitInner, max_running_tasks: Option<u32>) -> usize {
        let configured = max_running_tasks
            .filter(|value| *value > 0)
            .map(|value| value as usize)
            .unwrap_or(TOWER_DEFAULT_MAX_RUNNING_AGENTS);
        g.capacity.unwrap_or(configured).min(configured).max(1)
    }

    pub fn budget(&self, max_running_tasks: Option<u32>) -> usize {
        let now = (self.now)();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::maybe_recover(&mut g, now);
        Self::budget_locked(&g, max_running_tasks)
    }

    pub fn acquire(&self, max_running_tasks: Option<u32>) -> Result<(), String> {
        let now = (self.now)();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = g.blocked_until {
            if now < until {
                let retry_after_s = ((until - now) as f64 / 1000.0).ceil() as u64;
                return Err(format!(
                    "provider rate limit hit — new tower spawns paused for ~{retry_after_s}s. \
                     Successful requests lift the pause early; wait and retry, or let running agents finish first."
                ));
            }
            g.blocked_until = None;
        }
        Self::maybe_recover(&mut g, now);
        let budget = Self::budget_locked(&g, max_running_tasks);
        if g.inflight >= budget {
            return Err(format!(
                "tower concurrency budget exhausted ({}/{} agents running). \
                 Wait for a running agent to complete, then retry.",
                g.inflight, budget
            ));
        }
        g.inflight += 1;
        Ok(())
    }

    pub fn release(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.inflight = g.inflight.saturating_sub(1);
    }

    pub fn snapshot(&self, max_running_tasks: Option<u32>) -> TowerRateLimitSnapshot {
        let now = (self.now)();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::maybe_recover(&mut g, now);
        TowerRateLimitSnapshot {
            budget: Self::budget_locked(&g, max_running_tasks),
            inflight: g.inflight,
            blocked_until: g.blocked_until,
        }
    }

    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g = TowerRateLimitInner {
            capacity: None,
            inflight: 0,
            blocked_until: None,
            last_rate_limit_at: None,
            last_shrink_at: None,
            last_recovery_at: None,
        };
    }

    pub fn render_concurrency(&self, max_running_tasks: Option<u32>) -> String {
        Self::render_snapshot(&self.snapshot(max_running_tasks))
    }

    pub fn render_snapshot(snapshot: &TowerRateLimitSnapshot) -> String {
        let now = now_ms();
        let mut parts = vec![format!(
            "budget: {} agent(s) · inflight: {}",
            snapshot.budget, snapshot.inflight
        )];
        if let Some(until) = snapshot.blocked_until {
            if until > now {
                let rem_s = ((until - now) as f64 / 1000.0).ceil() as u64;
                parts.push(format!(
                    "spawns PAUSED for ~{rem_s}s (provider rate limit — successful requests lift the pause early)"
                ));
            } else {
                parts.push("spawn pause expired — budget probing resumes".to_string());
            }
        } else {
            parts.push("spawns open".to_string());
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn limiter() -> (TowerRateLimit, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock = now.clone();
        (
            TowerRateLimit::with_clock(move || clock.load(Ordering::Relaxed)),
            now,
        )
    }

    #[test]
    fn default_budget_is_256_and_explicit_cap_may_exceed_it() {
        let (rl, _) = limiter();
        assert_eq!(rl.budget(None), 256);
        assert_eq!(rl.budget(Some(300)), 300);
        assert_eq!(rl.budget(Some(2)), 2);
        for _ in 0..256 {
            assert!(rl.acquire(None).is_ok());
        }
        assert!(rl.acquire(None).unwrap_err().contains("256/256"));
    }

    #[test]
    fn configured_cap_limits_acquisition_and_release() {
        let (rl, _) = limiter();
        assert!(rl.acquire(Some(2)).is_ok());
        assert!(rl.acquire(Some(2)).is_ok());
        assert!(rl.acquire(Some(2)).unwrap_err().contains("2/2"));
        rl.release();
        assert!(rl.acquire(Some(2)).is_ok());
        rl.release();
        rl.release();
        rl.release();
        assert_eq!(rl.snapshot(Some(2)).inflight, 0);
    }

    #[test]
    fn first_429_anchors_capacity_to_inflight_minus_one() {
        let (rl, now) = limiter();
        for _ in 0..3 {
            assert!(rl.acquire(None).is_ok());
        }
        rl.report_rate_limited();
        assert_eq!(
            rl.snapshot(None),
            TowerRateLimitSnapshot {
                budget: 2,
                inflight: 3,
                blocked_until: Some(now.load(Ordering::Relaxed) + TOWER_SPAWN_PAUSE_MS),
            }
        );
    }

    #[test]
    fn subsequent_429s_shrink_no_more_than_once_per_two_seconds() {
        let (rl, now) = limiter();
        for _ in 0..3 {
            rl.acquire(None).unwrap();
        }
        rl.report_rate_limited();
        assert_eq!(rl.budget(None), 2);

        now.fetch_add(1_999, Ordering::Relaxed);
        rl.report_rate_limited();
        assert_eq!(rl.budget(None), 2);

        now.fetch_add(1, Ordering::Relaxed);
        rl.report_rate_limited();
        assert_eq!(rl.budget(None), 1);
    }

    #[test]
    fn pause_expires_and_success_lifts_it_early() {
        let (rl, now) = limiter();
        rl.report_rate_limited();
        assert!(rl.acquire(None).unwrap_err().contains("spawns paused"));
        now.fetch_add(TOWER_SPAWN_PAUSE_MS, Ordering::Relaxed);
        assert!(rl.acquire(None).is_ok());

        let (rl, _) = limiter();
        rl.report_rate_limited();
        rl.report_success();
        assert_eq!(rl.snapshot(None).blocked_until, None);
        assert!(rl.acquire(None).is_ok());
    }

    #[test]
    fn quiet_windows_recover_one_capacity_without_exceeding_configured_cap() {
        let (rl, now) = limiter();
        rl.acquire(Some(3)).unwrap();
        rl.acquire(Some(3)).unwrap();
        rl.acquire(Some(3)).unwrap();
        rl.report_rate_limited();
        assert_eq!(rl.budget(Some(3)), 2);

        now.fetch_add(
            RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS - 1,
            Ordering::Relaxed,
        );
        assert_eq!(rl.budget(Some(3)), 2);
        now.fetch_add(1, Ordering::Relaxed);
        assert_eq!(rl.budget(Some(3)), 3);
        now.fetch_add(RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS, Ordering::Relaxed);
        assert_eq!(rl.budget(Some(3)), 3);
        assert_eq!(rl.budget(None), 4);
    }

    #[test]
    fn reset_restores_infinite_capacity_and_clears_pause() {
        let (rl, _) = limiter();
        rl.acquire(None).unwrap();
        rl.report_rate_limited();
        rl.reset();
        assert_eq!(
            rl.snapshot(None),
            TowerRateLimitSnapshot {
                budget: 256,
                inflight: 0,
                blocked_until: None,
            }
        );
    }
}
