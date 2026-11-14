use std::io::Read;
use std::sync::{Arc, LazyLock, OnceLock};

#[derive(Clone)]
pub struct ClockSources {
    wall_now: Arc<dyn Fn() -> i64 + Send + Sync>,
    mono_now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl ClockSources {
    pub fn wall_now(&self) -> i64 {
        (self.wall_now)()
    }

    pub fn mono_now_ms(&self) -> i64 {
        (self.mono_now_ms)()
    }
}

fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn mono_now_ms() -> i64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as i64
}

pub static SYSTEM_CLOCKS: LazyLock<ClockSources> = LazyLock::new(|| ClockSources {
    wall_now: Arc::new(wall_now),
    mono_now_ms: Arc::new(mono_now_ms),
});

pub fn resolve_clock_sources(spec: Option<&str>, debug: bool) -> ClockSources {
    let Some(spec) = spec.filter(|spec| !spec.is_empty() && *spec != "system") else {
        return SYSTEM_CLOCKS.clone();
    };
    if let Some(file_path) = spec.strip_prefix("file:") {
        if file_path.is_empty() {
            debug_invalid_spec(spec, "empty file path", debug);
            return SYSTEM_CLOCKS.clone();
        }
        let file_path = file_path.to_string();
        return ClockSources {
            wall_now: Arc::new(move || read_file_wall(&file_path)),
            mono_now_ms: Arc::new(mono_now_ms),
        };
    }
    debug_invalid_spec(spec, "unrecognised scheme", debug);
    SYSTEM_CLOCKS.clone()
}

fn read_file_wall(file_path: &str) -> i64 {
    let Ok(file) = std::fs::File::open(file_path) else {
        return wall_now();
    };
    let mut bytes = Vec::new();
    if file.take(64).read_to_end(&mut bytes).is_err() {
        return wall_now();
    }
    let raw = String::from_utf8_lossy(&bytes);
    let first_line = raw.split('\n').next().unwrap_or_default().trim();
    if first_line.is_empty() {
        return wall_now();
    }
    first_line
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| value as i64)
        .unwrap_or_else(wall_now)
}

fn debug_invalid_spec(spec: &str, reason: &str, debug: bool) {
    if debug {
        let spec = serde_json::to_string(spec).unwrap_or_else(|_| format!("{spec:?}"));
        eprintln!(
            "[cron/clock] invalid KIMI_CRON_CLOCK spec {spec}: {reason} — falling back to system clock"
        );
    }
}
