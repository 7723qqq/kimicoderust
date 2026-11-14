use std::collections::HashSet;
use std::sync::LazyLock;

use kimi_core::primitives::errors::Error2;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::errors::SESSION_QUERY_INVALID_FILTER;
use super::events::SessionEventResultFilter;
use super::types::{SessionAvailability, SessionResultFilter, SessionResultRange};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchInput {
    pub query: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub session_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub created_at_from: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub created_at_to: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub parent_session_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub include_root_sessions: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub availability: Option<Vec<SessionAvailability>>,
    #[serde(default, deserialize_with = "deserialize_sequence")]
    pub event_seq_from: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_sequence")]
    pub event_seq_to: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub event_time_from: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub event_time_to: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub event_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventSearchInput {
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub session_id: Option<String>,
    pub query: String,
    #[serde(default, deserialize_with = "deserialize_sequence")]
    pub seq_from: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_sequence")]
    pub seq_to: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub time_from: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub time_to: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub event_types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTraceInput {
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub session_id: Option<String>,
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn deserialize_sequence<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let Value::Number(number) = value else {
        return Err(serde::de::Error::custom(
            "sequence must be a non-negative integer",
        ));
    };
    if let Some(value) = number.as_u64() {
        return Ok(Some(value));
    }
    let value = number.as_f64().ok_or_else(|| {
        serde::de::Error::custom("sequence must be a non-negative integer")
    })?;
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(serde::de::Error::custom(
            "sequence must be a non-negative integer",
        ));
    }
    Ok(Some(value as u64))
}

#[derive(Debug, Clone, Default)]
pub struct EventFilterInput {
    pub seq_from: Option<u64>,
    pub seq_to: Option<u64>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    pub event_types: Option<Vec<String>>,
}

pub fn build_session_filters(
    args: &SessionSearchInput,
) -> Result<Vec<SessionResultFilter>, Error2> {
    let mut filters = Vec::new();
    if let Some(values) = &args.session_ids {
        assert_non_empty_array("session_ids", values)?;
        filters.push(SessionResultFilter::Id {
            values: values.clone(),
        });
    }
    if let Some(range) = timestamp_range(
        "created_at",
        args.created_at_from.as_deref(),
        args.created_at_to.as_deref(),
    )? {
        filters.push(SessionResultFilter::CreatedAt { range });
    }
    if let Some(parents) = &args.parent_session_ids {
        assert_non_empty_array("parent_session_ids", parents)?;
        let mut seen = HashSet::new();
        let mut values: Vec<Option<String>> = parents
            .iter()
            .filter(|parent| seen.insert((*parent).clone()))
            .cloned()
            .map(Some)
            .collect();
        if args.include_root_sessions == Some(true) {
            values.push(None);
        }
        filters.push(SessionResultFilter::Parent { values });
    } else if args.include_root_sessions == Some(true) {
        filters.push(SessionResultFilter::Parent {
            values: vec![None],
        });
    }
    if let Some(values) = &args.availability {
        assert_non_empty_array("availability", values)?;
        filters.push(SessionResultFilter::Availability {
            values: values.clone(),
        });
    }
    Ok(filters)
}

pub fn build_event_filters(
    input: &EventFilterInput,
) -> Result<Vec<SessionEventResultFilter>, Error2> {
    let mut filters = Vec::new();
    let seq = sequence_range(input.seq_from, input.seq_to)?;
    if seq.from.is_some() || seq.to.is_some() {
        filters.push(SessionEventResultFilter::Seq { range: seq });
    }
    if let Some(range) = timestamp_range(
        "time",
        input.time_from.as_deref(),
        input.time_to.as_deref(),
    )? {
        filters.push(SessionEventResultFilter::Time { range });
    }
    if let Some(values) = &input.event_types {
        assert_non_empty_array("event_types", values)?;
        filters.push(SessionEventResultFilter::Type {
            values: values.clone(),
        });
    }
    Ok(filters)
}

pub fn normalize_query(value: &str) -> Result<String, Error2> {
    let query = collapse_js_whitespace(trim_js(value));
    if query.is_empty() {
        return Err(invalid_query(
            "session-search query must contain non-whitespace text",
        ));
    }
    if query.contains('\0') {
        return Err(invalid_query("session-search query must not contain NUL"));
    }
    Ok(query)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn collapse_js_whitespace(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_whitespace = false;
    for character in value.chars() {
        if is_js_whitespace(character) {
            if !in_whitespace {
                output.push(' ');
                in_whitespace = true;
            }
        } else {
            output.push(character);
            in_whitespace = false;
        }
    }
    output
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn sequence_range(from: Option<u64>, to: Option<u64>) -> Result<SessionResultRange, Error2> {
    if let Some(from) = from {
        assert_non_negative_safe_integer("sequence lower bound", from)?;
    }
    if let Some(to) = to {
        assert_non_negative_safe_integer("sequence upper bound", to)?;
    }
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        return Err(invalid_range(
            "sequence",
            "from must be less than or equal to to",
        ));
    }
    Ok(SessionResultRange {
        from: from.map(|value| value as i64),
        to: to.map(|value| value as i64),
    })
}

fn assert_non_negative_safe_integer(name: &str, value: u64) -> Result<(), Error2> {
    if value > 9_007_199_254_740_991 {
        return Err(Error2::new(
            SESSION_QUERY_INVALID_FILTER,
            format!("{name} must be a non-negative safe integer"),
        ));
    }
    Ok(())
}

static ISO_TIMESTAMP: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}(?::[0-9]{2}(?:\.[0-9]+)?)?(?:Z|[+-][0-9]{2}:[0-9]{2})$",
    )
    .unwrap()
});

fn timestamp_range(
    name: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Option<SessionResultRange>, Error2> {
    if from.is_none() && to.is_none() {
        return Ok(None);
    }
    let from_timestamp = from
        .map(|value| parse_iso_timestamp(&format!("{name}_from"), value))
        .transpose()?;
    let to_timestamp = to
        .map(|value| parse_iso_timestamp(&format!("{name}_to"), value))
        .transpose()?;
    if let (Some(from), Some(to)) = (from_timestamp, to_timestamp)
        && from > to
    {
        return Err(invalid_range(name, "from must be less than or equal to to"));
    }
    Ok(Some(SessionResultRange {
        from: from_timestamp,
        to: to_timestamp,
    }))
}

fn parse_iso_timestamp(name: &str, value: &str) -> Result<i64, Error2> {
    if !ISO_TIMESTAMP.is_match(value) {
        return Err(invalid_range(
            name,
            "must be an ISO 8601 timestamp with Z or a numeric offset",
        ));
    }
    let source = serde_json::to_string(value)
        .map_err(|_| invalid_range(name, "must be a valid ISO 8601 timestamp"))?;
    let parsed = rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| {
                    ctx.eval::<String, _>(format!(
                        "(function(){{const timestamp=Date.parse({source});return Number.isSafeInteger(timestamp)?String(timestamp):'';}})()"
                    ))
                })
            })
        })
        .map_err(|_| invalid_range(name, "must be a valid ISO 8601 timestamp"))?;
    if parsed.is_empty() {
        return Err(invalid_range(name, "must be a valid ISO 8601 timestamp"));
    }
    parsed
        .parse::<i64>()
        .map_err(|_| invalid_range(name, "must be a valid ISO 8601 timestamp"))
}

fn invalid_range(name: &str, detail: &str) -> Error2 {
    Error2::new(
        SESSION_QUERY_INVALID_FILTER,
        format!("session {name} range {detail}"),
    )
}

fn invalid_query(detail: &str) -> Error2 {
    Error2::new(SESSION_QUERY_INVALID_FILTER, detail)
}

fn assert_non_empty_array<T>(name: &str, values: &[T]) -> Result<(), Error2> {
    if values.is_empty() {
        return Err(Error2::new(
            SESSION_QUERY_INVALID_FILTER,
            format!("{name} must contain at least one value when supplied"),
        ));
    }
    Ok(())
}
