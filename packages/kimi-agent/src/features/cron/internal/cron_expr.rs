use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, TimeZone, Timelike};
use kimi_core::primitives::errors::{Error2, Error2Options};

const MS_PER_MINUTE: i64 = 60_000;
const CRON_EXPRESSION_INVALID: &str = "cron.expression_invalid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCronExpression {
    pub raw: String,
    pub minutes: BTreeSet<u32>,
    pub hours: BTreeSet<u32>,
    pub days_of_month: BTreeSet<u32>,
    pub months: BTreeSet<u32>,
    pub days_of_week: BTreeSet<u32>,
    pub days_of_month_wildcard: bool,
    pub days_of_week_wildcard: bool,
}

fn invalid(message: impl Into<String>) -> Error2 {
    Error2::new(CRON_EXPRESSION_INVALID, message)
}

fn invalid_with_details(message: impl Into<String>, details: serde_json::Value) -> Error2 {
    Error2::with_options(
        CRON_EXPRESSION_INVALID,
        message,
        Error2Options {
            details: details.as_object().cloned(),
            ..Error2Options::default()
        },
    )
}

pub fn parse_cron_expression(expr: &str) -> Result<ParsedCronExpression, Error2> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err(invalid("cron expression is empty"));
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(invalid_with_details(
            format!(
                "cron expression must have exactly 5 fields (minute hour day-of-month month day-of-week); got {}",
                fields.len()
            ),
            serde_json::json!({ "fieldCount": fields.len() }),
        ));
    }
    let minutes = parse_field(fields[0], 0, 59, "minute")?;
    let hours = parse_field(fields[1], 0, 23, "hour")?;
    let days_of_month = parse_field(fields[2], 1, 31, "day-of-month")?;
    let months = parse_field(fields[3], 1, 12, "month")?;
    let days_of_week = parse_field(fields[4], 0, 7, "day-of-week")?
        .into_iter()
        .map(|value| if value == 7 { 0 } else { value })
        .collect();
    Ok(ParsedCronExpression {
        raw: trimmed.to_string(),
        minutes,
        hours,
        days_of_month,
        months,
        days_of_week,
        days_of_month_wildcard: fields[2] == "*",
        days_of_week_wildcard: fields[4] == "*",
    })
}

fn parse_field(field: &str, min: u32, max: u32, name: &str) -> Result<BTreeSet<u32>, Error2> {
    if field.is_empty() {
        return Err(invalid_with_details(
            format!("cron {name} field is empty"),
            serde_json::json!({ "field": name }),
        ));
    }
    let mut out = BTreeSet::new();
    for term in field.split(',') {
        if term.is_empty() {
            return Err(invalid_with_details(
                format!("cron {name} field has empty term in list"),
                serde_json::json!({ "field": name }),
            ));
        }
        add_term(&mut out, term, min, max, name)?;
    }
    if out.is_empty() {
        return Err(invalid_with_details(
            format!("cron {name} field matches no values"),
            serde_json::json!({ "field": name }),
        ));
    }
    Ok(out)
}

fn parse_cron_int(raw: &str, name: &str, role: &str) -> Result<u32, Error2> {
    let raw_json = serde_json::to_string(raw).unwrap_or_else(|_| format!("{raw:?}"));
    if raw.is_empty() || !raw.chars().all(|character| character.is_ascii_digit()) {
        return Err(invalid_with_details(
            format!(
                "cron {name} {role} must be a non-negative integer with digits only (got {raw_json})"
            ),
            serde_json::json!({ "field": name, "role": role, "value": raw }),
        ));
    }
    raw.parse::<u32>().map_err(|_| {
        invalid_with_details(
            format!(
                "cron {name} {role} must be a non-negative integer with digits only (got {raw_json})"
            ),
            serde_json::json!({ "field": name, "role": role, "value": raw }),
        )
    })
}

fn add_term(
    out: &mut BTreeSet<u32>,
    term: &str,
    min: u32,
    max: u32,
    name: &str,
) -> Result<(), Error2> {
    let (range_part, step, stepped) = match term.split_once('/') {
        Some((range, step_raw)) => {
            if step_raw.is_empty() {
                return Err(invalid_with_details(
                    format!("cron {name} step is empty in \"{term}\""),
                    serde_json::json!({ "field": name, "term": term }),
                ));
            }
            let step = parse_cron_int(step_raw, name, "step")?;
            if step == 0 {
                return Err(invalid_with_details(
                    format!("cron {name} step must be a positive integer (got \"{step_raw}\")"),
                    serde_json::json!({ "field": name, "term": term, "step": step_raw }),
                ));
            }
            if range.is_empty() {
                return Err(invalid_with_details(
                    format!("cron {name} step needs a range or \"*\" before \"/\" in \"{term}\""),
                    serde_json::json!({ "field": name, "term": term }),
                ));
            }
            (range, step, true)
        }
        None => (term, 1, false),
    };
    let (lo, hi) = if range_part == "*" {
        (min, max)
    } else if let Some((lo_raw, hi_raw)) = range_part.split_once('-') {
        let lo = parse_cron_int(lo_raw, name, "range lower bound")?;
        let hi = parse_cron_int(hi_raw, name, "range upper bound")?;
        if lo < min || hi > max || lo > hi {
            return Err(invalid_with_details(
                format!(
                    "cron {name} range {lo}-{hi} out of bounds (must be {min}..{max}, ascending)"
                ),
                serde_json::json!({ "field": name, "lo": lo, "hi": hi, "min": min, "max": max }),
            ));
        }
        (lo, hi)
    } else {
        let single = parse_cron_int(range_part, name, "value")?;
        if single < min || single > max {
            return Err(invalid_with_details(
                format!("cron {name} value {single} out of range {min}..{max}"),
                serde_json::json!({ "field": name, "value": single, "min": min, "max": max }),
            ));
        }
        if !stepped {
            out.insert(single);
            return Ok(());
        }
        (single, max)
    };
    for value in (lo..=hi).step_by(step as usize) {
        out.insert(value);
    }
    Ok(())
}

pub fn compute_next_cron_run(expr: &ParsedCronExpression, from_ms: i64) -> Option<i64> {
    next_run_within_minutes(expr, from_ms, 5 * 366 * 24 * 60)
}

pub fn has_fire_within_years(expr: &ParsedCronExpression, years: f64, from_ms: i64) -> bool {
    let cap = ((years * 366.0 * 24.0 * 60.0).floor() as i64).max(1);
    next_run_within_minutes(expr, from_ms, cap).is_some()
}

fn local_from_naive(naive: NaiveDateTime) -> DateTime<Local> {
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(date) => date,
        chrono::LocalResult::Ambiguous(date, _) => date,
        chrono::LocalResult::None => naive.and_utc().with_timezone(&Local),
    }
}

fn next_run_within_minutes(
    expr: &ParsedCronExpression,
    from_ms: i64,
    cap_minutes: i64,
) -> Option<i64> {
    let start = Local.timestamp_millis_opt(from_ms).single()?;
    let start = start
        .with_second(0)
        .and_then(|date| date.with_nanosecond(0))
        .unwrap_or(start);
    let mut date = local_from_naive(start.naive_local() + Duration::minutes(1));
    let deadline_ms = from_ms + cap_minutes * MS_PER_MINUTE;
    let mut iterations = 0u32;
    while date.timestamp_millis() <= deadline_ms && iterations < 10_000_000 {
        iterations += 1;
        if !expr.months.contains(&date.month()) {
            advance_month(&mut date);
            continue;
        }
        if !day_matches(expr, &date) {
            advance_day(&mut date);
            continue;
        }
        if !expr.hours.contains(&date.hour()) {
            advance_hour(&mut date);
            continue;
        }
        if !expr.minutes.contains(&date.minute()) {
            advance_minute(&mut date);
            continue;
        }
        return Some(date.timestamp_millis());
    }
    None
}

fn day_matches(expr: &ParsedCronExpression, date: &DateTime<Local>) -> bool {
    let dom_ok = expr.days_of_month.contains(&date.day());
    let dow_ok = expr
        .days_of_week
        .contains(&date.weekday().num_days_from_sunday());
    if expr.days_of_month_wildcard && expr.days_of_week_wildcard {
        return true;
    }
    if expr.days_of_month_wildcard {
        return dow_ok;
    }
    if expr.days_of_week_wildcard {
        return dom_ok;
    }
    dom_ok || dow_ok
}

fn advance_month(date: &mut DateTime<Local>) {
    let mut year = date.year();
    let mut month = date.month() + 1;
    if month > 12 {
        month = 1;
        year += 1;
    }
    let naive = date
        .naive_local()
        .with_day(1)
        .unwrap()
        .with_year(year)
        .unwrap()
        .with_month(month)
        .unwrap()
        .with_hour(0)
        .unwrap()
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    *date = local_from_naive(naive);
}

fn advance_day(date: &mut DateTime<Local>) {
    let naive = date
        .date_naive()
        .succ_opt()
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    *date = local_from_naive(naive);
}

fn advance_hour(date: &mut DateTime<Local>) {
    let naive = date
        .naive_local()
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap()
        + Duration::hours(1);
    *date = local_from_naive(naive);
}

fn advance_minute(date: &mut DateTime<Local>) {
    let naive = date
        .naive_local()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap()
        + Duration::minutes(1);
    *date = local_from_naive(naive);
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub fn cron_to_human(expr: &ParsedCronExpression) -> String {
    let all_min = is_full_range(&expr.minutes, 0, 59);
    let all_hour = is_full_range(&expr.hours, 0, 23);
    let all_dom = expr.days_of_month_wildcard;
    let all_month = is_full_range(&expr.months, 1, 12);
    let all_dow = expr.days_of_week_wildcard;
    if all_hour && all_dom && all_month && all_dow {
        let step = detect_step(&expr.minutes, 0, 59);
        if let Some(step) = step.filter(|step| *step > 1) {
            return format!("every {step} minutes");
        }
        if all_min {
            return "every minute".to_string();
        }
        if expr.minutes.len() == 1 {
            return format!("at minute {} of every hour", first(&expr.minutes));
        }
    }
    if expr.minutes.len() == 1 && all_dom && all_month && all_dow {
        let minute = first(&expr.minutes);
        if let Some(step) = detect_step(&expr.hours, 0, 23).filter(|step| *step > 1) {
            return format!("every {step} hours at minute {}", pad(minute));
        }
    }
    if expr.minutes.len() == 1 && expr.hours.len() == 1 && all_dom && all_month {
        let hour = first(&expr.hours);
        let minute = first(&expr.minutes);
        if all_dow {
            return format!("at {}:{} every day", pad(hour), pad(minute));
        }
        if let Some(days) = format_dows(&expr.days_of_week) {
            return format!("at {}:{} on {days}", pad(hour), pad(minute));
        }
    }
    if expr.minutes.len() == 1
        && expr.hours.len() == 1
        && expr.days_of_month.len() == 1
        && !expr.days_of_month_wildcard
        && expr.months.len() == 1
        && all_dow
    {
        let month = first(&expr.months) as usize;
        return format!(
            "at {}:{} on day {} of {}",
            pad(first(&expr.hours)),
            pad(first(&expr.minutes)),
            first(&expr.days_of_month),
            MONTH_NAMES[month - 1]
        );
    }
    expr.raw.clone()
}

fn first(set: &BTreeSet<u32>) -> u32 {
    *set.iter().next().unwrap()
}

fn is_full_range(set: &BTreeSet<u32>, min: u32, max: u32) -> bool {
    set.len() == (max - min + 1) as usize && (min..=max).all(|value| set.contains(&value))
}

fn detect_step(set: &BTreeSet<u32>, min: u32, max: u32) -> Option<u32> {
    let values: Vec<u32> = set.iter().copied().collect();
    if values.len() < 2 || values[0] != min {
        return None;
    }
    let step = values[1] - values[0];
    if step == 0 {
        return None;
    }
    let mut expected = min;
    for value in values {
        if value != expected {
            return None;
        }
        expected += step;
    }
    if expected - step > max {
        return None;
    }
    Some(step)
}

fn format_dows(set: &BTreeSet<u32>) -> Option<String> {
    let values: Vec<u32> = set.iter().copied().collect();
    if values.is_empty() {
        return None;
    }
    if values == [1, 2, 3, 4, 5] {
        return Some("weekdays".to_string());
    }
    if values == [0, 6] {
        return Some("weekends".to_string());
    }
    Some(
        values
            .into_iter()
            .map(|value| DAY_NAMES[value as usize])
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn pad(value: u32) -> String {
    format!("{value:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_date(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn parses_wildcards_ranges_lists_steps_and_sunday() {
        let wildcards = parse_cron_expression("* * * * *").unwrap();
        assert_eq!(wildcards.minutes.len(), 60);
        assert_eq!(wildcards.hours.len(), 24);
        assert_eq!(wildcards.days_of_month.len(), 31);
        assert_eq!(wildcards.months.len(), 12);
        assert_eq!(wildcards.days_of_week.len(), 7);
        assert!(wildcards.days_of_month_wildcard);
        assert!(wildcards.days_of_week_wildcard);

        let parsed = parse_cron_expression("0 9-17 * * 1-5").unwrap();
        assert_eq!(parsed.hours, (9..=17).collect());
        let list = parse_cron_expression("0 9,12,17 * * *").unwrap();
        assert_eq!(list.hours, [9, 12, 17].into_iter().collect());
        let step = parse_cron_expression("*/5 * * * *").unwrap();
        assert_eq!(step.minutes, (0..=55).step_by(5).collect());
        let sunday = parse_cron_expression("0 0 * * 7").unwrap();
        assert_eq!(sunday.days_of_week, [0].into_iter().collect());
    }

    #[test]
    fn rejects_malformed_fields_and_non_integer_tokens() {
        for expression in ["", "* * * *", "60 * * * *", "*/0 * * * *", "1,,3 * * * *"] {
            assert!(parse_cron_expression(expression).is_err(), "{expression}");
        }
        for expression in ["-5 * * * *", "1e1 * * * *", "0x10 * * * *", "+5 * * * *"] {
            assert!(parse_cron_expression(expression).is_err(), "{expression}");
        }
    }

    #[test]
    fn computes_matching_minutes_weekdays_and_year_boundaries() {
        let step = parse_cron_expression("*/5 * * * *").unwrap();
        let next = compute_next_cron_run(&step, local_date(2024, 6, 1, 12, 0, 30)).unwrap();
        assert_eq!(next, local_date(2024, 6, 1, 12, 5, 0));

        let weekdays = parse_cron_expression("0 9 * * 1-5").unwrap();
        let next = compute_next_cron_run(&weekdays, local_date(2024, 6, 1, 9, 0, 0)).unwrap();
        assert_eq!(next, local_date(2024, 6, 3, 9, 0, 0));

        let yearly = parse_cron_expression("0 12 1 1 *").unwrap();
        let next = compute_next_cron_run(&yearly, local_date(2024, 6, 1, 0, 0, 0)).unwrap();
        assert_eq!(next, local_date(2025, 1, 1, 12, 0, 0));
    }

    #[test]
    fn handles_never_dates_leap_years_and_or_semantics() {
        let never = parse_cron_expression("0 0 31 2 *").unwrap();
        assert_eq!(
            compute_next_cron_run(&never, local_date(2024, 1, 1, 0, 0, 0)),
            None
        );
        assert!(!has_fire_within_years(
            &never,
            5.0,
            local_date(2024, 1, 1, 0, 0, 0)
        ));

        let leap = parse_cron_expression("0 0 29 2 *").unwrap();
        assert_eq!(
            compute_next_cron_run(&leap, local_date(2023, 1, 1, 0, 0, 0)),
            Some(local_date(2024, 2, 29, 0, 0, 0))
        );

        let either = parse_cron_expression("0 0 1 * 1").unwrap();
        let mut cursor = local_date(2024, 6, 1, 0, 0, 0) - 1;
        let mut saw_weekday = false;
        let mut saw_month_day = false;
        for _ in 0..12 {
            let next = compute_next_cron_run(&either, cursor).unwrap();
            let date = Local.timestamp_millis_opt(next).single().unwrap();
            assert!(date.weekday().num_days_from_sunday() == 1 || date.day() == 1);
            saw_weekday |= date.weekday().num_days_from_sunday() == 1 && date.day() != 1;
            saw_month_day |= date.day() == 1;
            cursor = next;
        }
        assert!(saw_weekday && saw_month_day);
    }

    #[test]
    fn renders_human_schedules() {
        assert_eq!(
            cron_to_human(&parse_cron_expression("* * * * *").unwrap()),
            "every minute"
        );
        assert_eq!(
            cron_to_human(&parse_cron_expression("*/5 * * * *").unwrap()),
            "every 5 minutes"
        );
        assert_eq!(
            cron_to_human(&parse_cron_expression("0 9 * * *").unwrap()),
            "at 09:00 every day"
        );
        assert_eq!(
            cron_to_human(&parse_cron_expression("0 */6 * * *").unwrap()),
            "every 6 hours at minute 00"
        );
        assert_eq!(
            cron_to_human(&parse_cron_expression("0 9 * * 1-5").unwrap()),
            "at 09:00 on weekdays"
        );
        assert_eq!(
            cron_to_human(&parse_cron_expression("0 10 * * 0,6").unwrap()),
            "at 10:00 on weekends"
        );
        assert_eq!(
            cron_to_human(&parse_cron_expression("0 12 1 1 *").unwrap()),
            "at 12:00 on day 1 of January"
        );
    }
}
