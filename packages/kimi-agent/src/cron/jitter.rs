use crate::cron::{ParsedCron, next_fire};

pub const RECURRING_MAX_FRACTION_OF_PERIOD: f64 = 0.1;
pub const RECURRING_MAX_MS: i64 = 15 * 60_000;
pub const ONE_SHOT_MAX_MS: i64 = 90_000;

const MS_PER_DAY: i64 = 24 * 60 * 60_000;
const MS_PER_MINUTE: i64 = 60_000;
pub const STALE_THRESHOLD_MS: i64 = 7 * 24 * 60 * 60_000;
pub const MAX_COALESCE_ITERATIONS: u32 = 10_000;

pub fn fraction_from_id(id: &str) -> f64 {
    if id.len() == 8
        && id.chars().all(|c| c.is_ascii_hexdigit())
        && let Ok(n) = u32::from_str_radix(id, 16)
    {
        #[allow(clippy::cast_precision_loss)]
        return f64::from(n) / 4_294_967_296.0;
    }
    let mut hash: i32 = 5381;
    for byte in id.bytes() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_add(hash)
            .wrapping_add(byte as i32);
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    {
        (hash as u32 as f64) / 4_294_967_296.0
    }
}

pub fn jittered_next_run_ms(
    task_id: &str,
    parsed: &ParsedCron,
    ideal_ms: i64,
    tz_offset_minutes: i32,
    no_jitter: bool,
) -> i64 {
    if no_jitter {
        return ideal_ms;
    }
    let period = match next_fire(parsed, ideal_ms, tz_offset_minutes) {
        Some(next) if next > ideal_ms => next - ideal_ms,
        _ => MS_PER_DAY,
    };
    #[allow(clippy::cast_precision_loss)]
    let period_cap = period as f64 * RECURRING_MAX_FRACTION_OF_PERIOD;
    let cap = period_cap.min(RECURRING_MAX_MS as f64) as i64;
    if cap <= 0 {
        return ideal_ms;
    }
    #[allow(clippy::cast_possible_truncation)]
    let offset = (cap as f64 * fraction_from_id(task_id)) as i64;
    ideal_ms + offset
}

pub fn one_shot_jittered_run_ms(
    task_id: &str,
    created_at_ms: Option<i64>,
    ideal_ms: i64,
    tz_offset_minutes: i32,
    no_jitter: bool,
) -> i64 {
    if no_jitter {
        return ideal_ms;
    }
    if ideal_ms % MS_PER_MINUTE != 0 {
        return ideal_ms;
    }
    let minute_of_hour =
        ((ideal_ms + i64::from(tz_offset_minutes) * MS_PER_MINUTE) / MS_PER_MINUTE % 60 + 60) % 60;
    if minute_of_hour != 0 && minute_of_hour != 30 {
        return ideal_ms;
    }
    #[allow(clippy::cast_possible_truncation)]
    let offset = -(ONE_SHOT_MAX_MS as f64 * fraction_from_id(task_id)) as i64;
    let shifted = ideal_ms + offset;
    if let Some(created) = created_at_ms
        && shifted < created
    {
        return ideal_ms;
    }
    shifted
}

pub fn is_stale_at(created_at_ms: Option<i64>, now_ms: i64, no_stale: bool) -> bool {
    if no_stale {
        return false;
    }
    match created_at_ms {
        Some(created) => now_ms.saturating_sub(created) >= STALE_THRESHOLD_MS,
        None => false,
    }
}

pub fn count_coalesced(
    task_id: &str,
    recurring: bool,
    created_at_ms: Option<i64>,
    parsed: &ParsedCron,
    first_fire_ms: i64,
    now_ms: i64,
    tz_offset_minutes: i32,
) -> (u32, i64) {
    let mut count: u32 = 1;
    let mut cursor = first_fire_ms;
    let mut last_due_ms = first_fire_ms;
    while count < MAX_COALESCE_ITERATIONS {
        let Some(next) = next_fire(parsed, cursor, tz_offset_minutes) else {
            break;
        };
        if next > now_ms {
            break;
        }
        let jittered = if recurring {
            jittered_next_run_ms(task_id, parsed, next, tz_offset_minutes, false)
        } else {
            one_shot_jittered_run_ms(task_id, created_at_ms, next, tz_offset_minutes, false)
        };
        if jittered > now_ms {
            break;
        }
        count += 1;
        cursor = next;
        last_due_ms = next;
    }
    (count, last_due_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::parse;

    #[test]
    fn fraction_from_id_matches_v2_vectors() {
        assert_eq!(fraction_from_id("00000000"), 0.0);
        assert_eq!(fraction_from_id("80000000"), 0.5);
        assert!(fraction_from_id("ffffffff") > 0.99);
        let djb2_a = 177670f64 / 4_294_967_296.0;
        assert!((fraction_from_id("a") - djb2_a).abs() < 1e-12);
        assert_eq!(
            fraction_from_id("some-task-id"),
            fraction_from_id("some-task-id")
        );
        for id in ["", "x", "M1", "01ABCDEFGHJKMNPQRSTVWXYZ", "zzz-999"] {
            let f = fraction_from_id(id);
            assert!((0.0..1.0).contains(&f), "{id} -> {f}");
        }
    }

    fn hourly() -> ParsedCron {
        parse("0 * * * *").unwrap()
    }

    #[test]
    fn recurring_jitter_shifts_forward_within_cap() {
        let parsed = hourly();
        let ideal = 1_717_200_000_000;
        let jittered = jittered_next_run_ms("task-1", &parsed, ideal, 0, false);
        assert!(jittered >= ideal, "recurring jitter never shifts backward");
        assert!(
            jittered < ideal + 6 * 60_000,
            "bounded by 10% of the hourly period: {jittered} vs {ideal}"
        );
        assert_eq!(
            jittered_next_run_ms("task-1", &parsed, ideal, 0, false),
            jittered
        );
        assert_eq!(
            jittered_next_run_ms("task-1", &parsed, ideal, 0, true),
            ideal
        );
    }

    #[test]
    fn one_shot_jitter_only_on_hour_and_half_hour() {
        let midnight = 1_717_200_000_000;
        let shifted = one_shot_jittered_run_ms("task-1", None, midnight, 0, false);
        assert!(shifted <= midnight, "one-shot jitter never shifts forward");
        assert!(shifted > midnight - 90_000, "bounded by 90 s");
        assert_eq!(
            one_shot_jittered_run_ms("task-1", None, midnight, 330, false),
            one_shot_jittered_run_ms("task-1", None, midnight, 0, false),
            "whole- and half-hour zones agree on :00/:30 boundaries"
        );
        let quarter_past = midnight + 15 * 60_000;
        assert_eq!(
            one_shot_jittered_run_ms("task-1", None, quarter_past, 0, false),
            quarter_past
        );
        assert_eq!(
            one_shot_jittered_run_ms("task-1", Some(midnight), midnight, 0, false),
            midnight
        );
        assert_eq!(
            one_shot_jittered_run_ms("task-1", None, midnight, 0, true),
            midnight
        );
    }

    #[test]
    fn stale_threshold_is_seven_days() {
        let now = 1_717_200_000_000;
        assert!(!is_stale_at(Some(now - 6 * 24 * 60 * 60_000), now, false));
        assert!(is_stale_at(Some(now - 8 * 24 * 60 * 60_000), now, false));
        assert!(!is_stale_at(Some(now - 8 * 24 * 60 * 60_000), now, true));
        assert!(!is_stale_at(None, now, false));
    }

    #[test]
    fn coalesce_counts_missed_fires() {
        let parsed = parse("*/5 * * * *").unwrap();
        let first = 1_717_200_000_000 + 5 * 60_000;
        let (count, _) =
            count_coalesced("task-1", true, None, &parsed, first, first + 7 * 60_000, 0);
        assert_eq!(count, 2, "two ideal fires passed");
        let (single, _) = count_coalesced("task-1", true, None, &parsed, first, first + 60_000, 0);
        assert_eq!(single, 1, "nothing else passed yet");
    }
}
