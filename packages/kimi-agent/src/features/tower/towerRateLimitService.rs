use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS: u64 = 2_000;
pub const RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS: u64 = 180_000;
pub const TOWER_SPAWN_PAUSE_MS: u64 = 60_000;
pub const TOWER_DEFAULT_MAX_RUNNING_AGENTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerRateLimitSnapshot {
    pub budget: usize,
    pub inflight: usize,
    pub blocked_until: Option<u64>,
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct RateLimitCapacityGovernor {
    capacity: Option<usize>,
    last_rate_limit_at: Option<u64>,
    last_shrink_at: Option<u64>,
    last_recovery_at: Option<u64>,
    now: Clock,
}

impl RateLimitCapacityGovernor {
    pub fn new() -> Self {
        Self::with_clock(now_ms)
    }

    pub fn with_clock(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            capacity: None,
            last_rate_limit_at: None,
            last_shrink_at: None,
            last_recovery_at: None,
            now: Arc::new(now),
        }
    }

    pub fn get_capacity(&self) -> Option<usize> {
        self.capacity
    }

    pub fn in_backoff(&self) -> bool {
        self.last_rate_limit_at.is_some()
    }

    pub fn last_rate_limited_at(&self) -> Option<u64> {
        self.last_rate_limit_at
    }

    pub fn note_rate_limited(&mut self, active_count: usize) {
        let now = (self.now)();
        if active_count > 0 {
            match self.capacity {
                None => {
                    self.capacity = Some((active_count - 1).max(1));
                    self.last_shrink_at = Some(now);
                }
                Some(capacity)
                    if self.last_shrink_at.is_none()
                        || now.saturating_sub(self.last_shrink_at.unwrap_or(0))
                            >= RATE_LIMIT_CAPACITY_SHRINK_INTERVAL_MS =>
                {
                    self.capacity = Some(capacity.saturating_sub(1).max(1));
                    self.last_shrink_at = Some(now);
                }
                Some(_) => {}
            }
        }
        self.last_rate_limit_at = Some(now);
    }

    pub fn maybe_recover(&mut self) -> bool {
        let now = (self.now)();
        if self.next_recovery_at().is_some_and(|next| next > now) {
            return false;
        }
        if self.last_rate_limit_at.is_none() {
            return false;
        }
        if let Some(capacity) = self.capacity {
            self.capacity = Some(capacity.saturating_add(1));
        }
        self.last_recovery_at = Some(now);
        true
    }

    pub fn next_recovery_at(&self) -> Option<u64> {
        self.last_rate_limit_at.map(|last| {
            last.max(self.last_recovery_at.unwrap_or(0))
                .saturating_add(RATE_LIMIT_CAPACITY_RECOVERY_INTERVAL_MS)
        })
    }

    pub fn reset(&mut self) {
        self.capacity = None;
        self.last_rate_limit_at = None;
        self.last_shrink_at = None;
        self.last_recovery_at = None;
    }
}

impl Default for RateLimitCapacityGovernor {
    fn default() -> Self {
        Self::new()
    }
}

struct TowerRateLimitInner {
    governor: RateLimitCapacityGovernor,
    inflight: usize,
    blocked_until: Option<u64>,
}

pub struct TowerRateLimitService {
    inner: Mutex<TowerRateLimitInner>,
    now: Clock,
}

impl TowerRateLimitService {
    pub fn new() -> Self {
        Self::with_clock(now_ms)
    }

    pub fn with_clock(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        let now: Clock = Arc::new(now);
        let governor_now = now.clone();
        Self {
            inner: Mutex::new(TowerRateLimitInner {
                governor: RateLimitCapacityGovernor::with_clock(move || governor_now()),
                inflight: 0,
                blocked_until: None,
            }),
            now,
        }
    }

    pub fn global() -> Arc<Self> {
        static GLOBAL: LazyLock<Arc<TowerRateLimitService>> =
            LazyLock::new(|| Arc::new(TowerRateLimitService::new()));
        GLOBAL.clone()
    }

    pub fn report_rate_limited(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.governor.note_rate_limited(inner.inflight);
        inner.blocked_until = Some((self.now)().saturating_add(TOWER_SPAWN_PAUSE_MS));
    }

    pub fn report_success(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.blocked_until = None;
        inner.governor.maybe_recover();
    }

    fn budget_locked(governor: &RateLimitCapacityGovernor, max_running_tasks: Option<u32>) -> usize {
        let configured = max_running_tasks
            .map(|value| value as usize)
            .unwrap_or(TOWER_DEFAULT_MAX_RUNNING_AGENTS);
        governor
            .capacity
            .unwrap_or(configured)
            .min(configured)
            .max(1)
    }

    pub fn budget(&self, max_running_tasks: Option<u32>) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.governor.maybe_recover();
        Self::budget_locked(&inner.governor, max_running_tasks)
    }

    pub fn acquire(&self, max_running_tasks: Option<u32>) -> Result<(), String> {
        let now = (self.now)();
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(until) = inner.blocked_until {
            if now < until {
                let retry_after_s = ((until - now) as f64 / 1000.0).ceil() as u64;
                return Err(format!(
                    "provider rate limit hit — new tower spawns paused for ~{retry_after_s}s. \
                     Successful requests lift the pause early; wait and retry, or let running agents finish first."
                ));
            }
            inner.blocked_until = None;
        }
        inner.governor.maybe_recover();
        let budget = Self::budget_locked(&inner.governor, max_running_tasks);
        if inner.inflight >= budget {
            return Err(format!(
                "tower concurrency budget exhausted ({}/{} agents running). \
                 Wait for a running agent to complete, then retry.",
                inner.inflight, budget
            ));
        }
        inner.inflight += 1;
        Ok(())
    }

    pub fn release(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.inflight = inner.inflight.saturating_sub(1);
    }

    pub fn snapshot(&self, max_running_tasks: Option<u32>) -> TowerRateLimitSnapshot {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.governor.maybe_recover();
        TowerRateLimitSnapshot {
            budget: Self::budget_locked(&inner.governor, max_running_tasks),
            inflight: inner.inflight,
            blocked_until: inner.blocked_until,
        }
    }

    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.governor.reset();
        inner.inflight = 0;
        inner.blocked_until = None;
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

impl Default for TowerRateLimitService {
    fn default() -> Self {
        Self::new()
    }
}
