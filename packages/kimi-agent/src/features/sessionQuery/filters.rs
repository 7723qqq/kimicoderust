use kimi_core::primitives::errors::Error2;

use super::errors::SESSION_QUERY_INVALID_FILTER;
use super::types::{
    SessionAvailability, SessionRecord, SessionResultFilter, SessionResultRange,
};

pub fn filter_session_results(
    records: &[SessionRecord],
    filters: &[SessionResultFilter],
) -> Result<Vec<SessionRecord>, Error2> {
    for filter in filters {
        validate_filter(filter)?;
    }
    Ok(records
        .iter()
        .filter(|record| filters.iter().all(|filter| matches(record, filter)))
        .cloned()
        .collect())
}

pub fn materialize_session_result_filters(
    filters: &[SessionResultFilter],
) -> Result<Vec<SessionResultFilter>, Error2> {
    for filter in filters {
        validate_filter(filter)?;
    }
    Ok(filters.to_vec())
}

fn validate_filter(filter: &SessionResultFilter) -> Result<(), Error2> {
    match filter {
        SessionResultFilter::CreatedAt { range } => validate_range("created-at", range),
        SessionResultFilter::Id { .. }
        | SessionResultFilter::Cwd { .. }
        | SessionResultFilter::Parent { .. }
        | SessionResultFilter::Availability { .. } => Ok(()),
    }
}

fn matches(record: &SessionRecord, filter: &SessionResultFilter) -> bool {
    match filter {
        SessionResultFilter::Id { values } => values.contains(&record.id),
        SessionResultFilter::Cwd { values } => values.contains(&record.cwd),
        SessionResultFilter::CreatedAt { range } => matches_range(record.created_at, range),
        SessionResultFilter::Parent { values } => values.contains(&record.parent_session_id),
        SessionResultFilter::Availability { values } => values.iter().any(|value| match value {
            SessionAvailability::Live => record.live,
            SessionAvailability::Persisted => record.persisted,
        }),
    }
}

pub(crate) fn validate_range(name: &str, range: &SessionResultRange) -> Result<(), Error2> {
    if let (Some(from), Some(to)) = (range.from, range.to)
        && from > to
    {
        return Err(invalid_range(name, "from must be less than or equal to to"));
    }
    Ok(())
}

pub(crate) fn matches_range(value: i64, range: &SessionResultRange) -> bool {
    range.from.is_none_or(|from| value >= from) && range.to.is_none_or(|to| value <= to)
}

pub(crate) fn invalid_range(name: &str, detail: &str) -> Error2 {
    invalid_filter(&format!("{name} filter {detail}"))
}

pub(crate) fn invalid_filter(detail: &str) -> Error2 {
    Error2::new(
        SESSION_QUERY_INVALID_FILTER,
        format!("session {detail}"),
    )
}
