use serde::{Deserialize, Deserializer};

pub const CRON_SECTION: &str = "cron";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PollIntervalMs {
    #[default]
    Unspecified,
    Null,
    Value(i64),
}

impl<'de> Deserialize<'de> for PollIntervalMs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<i64>::deserialize(deserializer)?
            .map(Self::Value)
            .unwrap_or(Self::Null))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronConfig {
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub no_jitter: bool,
    #[serde(default)]
    pub no_stale: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub manual_tick: bool,
    #[serde(default)]
    pub clock: Option<String>,
    #[serde(default)]
    pub poll_interval_ms: PollIntervalMs,
}

pub const DEFAULT_CRON_CONFIG: CronConfig = CronConfig {
    debug: false,
    no_jitter: false,
    no_stale: false,
    disabled: false,
    manual_tick: false,
    clock: None,
    poll_interval_ms: PollIntervalMs::Unspecified,
};

fn on(raw: &str) -> bool {
    raw == "1"
}

pub fn parse_poll_interval_ms(raw: &str) -> Option<Option<i64>> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if value == "null" {
        return Some(None);
    }
    let parsed = if let Some(digits) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        parse_radix(digits, 16)
    } else if let Some(digits) = value.strip_prefix("0b").or_else(|| value.strip_prefix("0B")) {
        parse_radix(digits, 2)
    } else if let Some(digits) = value.strip_prefix("0o").or_else(|| value.strip_prefix("0O")) {
        parse_radix(digits, 8)
    } else {
        value.parse::<f64>().ok()
    }?;
    if !parsed.is_finite() || parsed.fract() != 0.0 || parsed < 0.0 || parsed > i64::MAX as f64 {
        return None;
    }
    Some(Some(parsed as i64))
}

fn parse_radix(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
        return None;
    }
    u128::from_str_radix(digits, radix)
        .ok()
        .map(|value| value as f64)
}

pub fn cron_env_bindings() -> [(&'static str, &'static str); 7] {
    [
        ("debug", "KIMI_CRON_DEBUG"),
        ("noJitter", "KIMI_CRON_NO_JITTER"),
        ("noStale", "KIMI_CRON_NO_STALE"),
        ("disabled", "KIMI_DISABLE_CRON"),
        ("manualTick", "KIMI_CRON_MANUAL_TICK"),
        ("clock", "KIMI_CRON_CLOCK"),
        ("pollIntervalMs", "KIMI_CRON_POLL_INTERVAL_MS"),
    ]
}

pub fn strip_cron_env() -> Option<serde_json::Value> {
    None
}
