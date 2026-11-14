use chrono::{TimeZone, Timelike};

use crate::features::cron::cron_task::CronTask;
use crate::features::cron::internal::cron_expr::{ParsedCronExpression, compute_next_cron_run};

#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    pub recurring_max_fraction_of_period: f64,
    pub recurring_max_ms: i64,
    pub one_shot_max_ms: i64,
}

pub const DEFAULT_CRON_JITTER_CONFIG: JitterConfig = JitterConfig {
    recurring_max_fraction_of_period: 0.1,
    recurring_max_ms: 15 * 60_000,
    one_shot_max_ms: 90_000,
};

const MS_PER_DAY: i64 = 24 * 60 * 60_000;
const MS_PER_MINUTE: i64 = 60_000;

fn fraction_from_id(id: &str) -> f64 {
    if id.len() == 8 && id.chars().all(|character| character.is_ascii_hexdigit()) {
        if let Ok(value) = u32::from_str_radix(id, 16) {
            return value as f64 / 4_294_967_296.0;
        }
    }
    let mut hash = 5381i32;
    for code in id.encode_utf16() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_add(hash)
            .wrapping_add(code as i32);
    }
    (hash as u32) as f64 / 4_294_967_296.0
}

pub fn jittered_next_cron_run_ms(
    task: &CronTask,
    parsed: &ParsedCronExpression,
    ideal_ms: i64,
    config: Option<JitterConfig>,
    no_jitter: bool,
) -> i64 {
    if no_jitter {
        return ideal_ms;
    }
    let config = config.unwrap_or(DEFAULT_CRON_JITTER_CONFIG);
    let period = compute_next_cron_run(parsed, ideal_ms)
        .filter(|next| *next > ideal_ms)
        .map(|next| next - ideal_ms)
        .unwrap_or(MS_PER_DAY);
    let period_cap = period as f64 * config.recurring_max_fraction_of_period;
    let cap = period_cap.min(config.recurring_max_ms as f64);
    if cap <= 0.0 {
        return ideal_ms;
    }
    ideal_ms + (cap * fraction_from_id(&task.id)) as i64
}

pub fn one_shot_jittered_next_cron_run_ms(
    task: &CronTask,
    ideal_ms: i64,
    config: Option<JitterConfig>,
    no_jitter: bool,
) -> i64 {
    if no_jitter || ideal_ms % MS_PER_MINUTE != 0 {
        return ideal_ms;
    }
    let Some(minute_of_hour) = chrono::Local
        .timestamp_millis_opt(ideal_ms)
        .single()
        .map(|date| date.minute())
    else {
        return ideal_ms;
    };
    if minute_of_hour != 0 && minute_of_hour != 30 {
        return ideal_ms;
    }
    let config = config.unwrap_or(DEFAULT_CRON_JITTER_CONFIG);
    if config.one_shot_max_ms <= 0 {
        return ideal_ms;
    }
    let shifted = ideal_ms - (config.one_shot_max_ms as f64 * fraction_from_id(&task.id)) as i64;
    if shifted < task.created_at {
        return ideal_ms;
    }
    shifted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cron::internal::cron_expr::parse_cron_expression;

    fn local_date(hour: u32, minute: u32, second: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(2024, 6, 1, hour, minute, second)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn task(id: &str, created_at: i64) -> CronTask {
        CronTask {
            id: id.to_string(),
            cron: "*/5 * * * *".to_string(),
            prompt: String::new(),
            created_at,
            ..Default::default()
        }
    }

    #[test]
    fn recurring_offsets_are_deterministic_bounded_and_id_specific() {
        let parsed = parse_cron_expression("*/5 * * * *").unwrap();
        let ideal = local_date(12, 5, 0);
        let a = jittered_next_cron_run_ms(&task("aaaaaaaa", 0), &parsed, ideal, None, false);
        let b = jittered_next_cron_run_ms(&task("11111111", 0), &parsed, ideal, None, false);
        assert_ne!(a, b);
        assert_eq!(
            a,
            jittered_next_cron_run_ms(&task("aaaaaaaa", 0), &parsed, ideal, None, false)
        );
        assert!((0..=30_000).contains(&(a - ideal)));
        assert_eq!(
            jittered_next_cron_run_ms(&task("aaaaaaaa", 0), &parsed, ideal, None, true),
            ideal
        );
    }

    #[test]
    fn daily_offsets_respect_the_recurring_cap() {
        let parsed = parse_cron_expression("0 9 * * *").unwrap();
        let ideal = local_date(9, 0, 0);
        let next = jittered_next_cron_run_ms(&task("aaaaaaaa", 0), &parsed, ideal, None, false);
        assert!((60_000..=15 * 60_000).contains(&(next - ideal)));
    }

    #[test]
    fn one_shot_pulls_only_round_minutes_forward() {
        let task = task("aaaaaaaa", 0);
        let hour = local_date(14, 0, 0);
        let next = one_shot_jittered_next_cron_run_ms(&task, hour, None, false);
        assert!((-90_000..0).contains(&(next - hour)));
        assert_eq!(
            one_shot_jittered_next_cron_run_ms(&task, local_date(14, 7, 0), None, false),
            local_date(14, 7, 0)
        );
        assert_eq!(
            one_shot_jittered_next_cron_run_ms(&task, local_date(14, 0, 12), None, false),
            local_date(14, 0, 12)
        );
        assert_eq!(
            one_shot_jittered_next_cron_run_ms(&task, hour, None, true),
            hour
        );
    }

    #[test]
    fn one_shot_never_moves_before_creation() {
        let ideal = local_date(9, 0, 0);
        let insufficient = task("ffffffff", ideal - 30_000);
        assert_eq!(
            one_shot_jittered_next_cron_run_ms(&insufficient, ideal, None, false),
            ideal
        );
        let permitted = task("ffffffff", ideal - 5 * 60_000);
        let next = one_shot_jittered_next_cron_run_ms(&permitted, ideal, None, false);
        assert!(next >= permitted.created_at && next < ideal && ideal - next <= 90_000);
    }
}
