use once_cell::sync::Lazy;
use regex::Regex;

static ISO_8601_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}(?::?\d{2})?)$")
        .unwrap()
});

#[derive(Clone, Debug)]
pub struct IsoDateTimeError(pub String);

impl std::fmt::Display for IsoDateTimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IsoDateTimeError {}

pub struct IsoDateTimeSchema;

impl IsoDateTimeSchema {
    pub fn parse(&self, value: &str) -> Result<String, IsoDateTimeError> {
        if !ISO_8601_REGEX.is_match(value) {
            return Err(IsoDateTimeError(
                "must be an ISO 8601 datetime string".to_string(),
            ));
        }
        let mut normalized = value.to_string();
        if !normalized.ends_with('Z') {
            let timezone_index = normalized
                .char_indices()
                .rev()
                .find(|(_, character)| matches!(character, '+' | '-'))
                .map(|(index, _)| index)
                .unwrap();
            let offset = &normalized[timezone_index + 1..];
            if offset.len() == 2 {
                normalized.push_str(":00");
            } else if offset.len() == 4 && !offset.contains(':') {
                normalized.insert(normalized.len() - 2, ':');
            }
        }
        let parsed = chrono::DateTime::parse_from_rfc3339(&normalized)
            .map_err(|_| IsoDateTimeError("invalid ISO 8601 datetime".to_string()))?;
        Ok(parsed
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string())
    }
}

pub static ISO_DATE_TIME_SCHEMA: IsoDateTimeSchema = IsoDateTimeSchema;

pub fn iso_date_time(value: &str) -> Result<String, IsoDateTimeError> {
    ISO_DATE_TIME_SCHEMA.parse(value)
}
