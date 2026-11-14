use chrono::{Datelike, TimeZone, Timelike};

use crate::agent::context_memory::types::CronJobOrigin;

pub fn format_local_iso_with_offset(ms: i64) -> String {
    let date = chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| chrono::Local.timestamp_millis_opt(0).single().unwrap());
    let offset_min = date.offset().local_minus_utc() / 60;
    let sign = if offset_min >= 0 { '+' } else { '-' };
    let abs_offset = offset_min.abs();
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{}{:02}:{:02}",
        date.year(),
        date.month(),
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
        date.timestamp_subsec_millis(),
        sign,
        abs_offset / 60,
        abs_offset % 60
    )
}

pub fn render_cron_fire_xml(origin: &CronJobOrigin, prompt: &str) -> String {
    let job_id = string_attr(&origin.job_id, "unknown");
    let cron = string_attr(&origin.cron, "unknown");
    let recurring = if origin.recurring { "true" } else { "false" };
    let stale = if origin.stale { "true" } else { "false" };
    format!(
        "<cron-fire jobId=\"{job_id}\" cron=\"{cron}\" recurring=\"{recurring}\" coalescedCount=\"{}\" stale=\"{stale}\">\n<prompt>\n{prompt}\n</prompt>\n</cron-fire>",
        origin.coalesced_count
    )
}

fn string_attr(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        return fallback.to_string();
    }
    value.replace('&', "&amp;").replace('"', "&quot;")
}
