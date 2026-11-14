use serde_json::Map;

use crate::wire::errors::{WIRE_MIGRATION_MISSING, WIRE_VERSION_TOO_LOW, WireError};
use crate::wire::record::WireRecord;

pub use super::v1_1::MIGRATE_V1_0_TO_V1_1;
pub use super::v1_2::MIGRATE_V1_1_TO_V1_2;
pub use super::v1_3::MIGRATE_V1_2_TO_V1_3;
pub use super::v1_4::MIGRATE_V1_3_TO_V1_4;
pub use super::v1_5::MIGRATE_V1_4_TO_V1_5;

pub const WIRE_PROTOCOL_VERSION: &str = "1.5";
pub const WIRE_MIN_READER_VERSION: &str = "1.5";

pub type WireMigrationRecord = WireRecord;

pub trait WireMigration: Send + Sync {
    fn source_version(&self) -> &'static str;
    fn target_version(&self) -> &'static str;
    fn migrate_record(&self, record: WireMigrationRecord) -> WireMigrationRecord;
}

pub static MIGRATIONS: &[&dyn WireMigration] = &[
    &MIGRATE_V1_0_TO_V1_1,
    &MIGRATE_V1_1_TO_V1_2,
    &MIGRATE_V1_2_TO_V1_3,
    &MIGRATE_V1_3_TO_V1_4,
    &MIGRATE_V1_4_TO_V1_5,
];

pub fn is_newer_wire_version(read_version: &str) -> bool {
    compare_wire_versions(read_version, WIRE_PROTOCOL_VERSION) > 0.0
}

pub fn assert_wire_reader_version(min_reader_version: Option<&str>) -> Result<(), WireError> {
    if let Some(min_reader_version) = min_reader_version
        && compare_wire_versions(WIRE_PROTOCOL_VERSION, min_reader_version) < 0.0
    {
        let mut details = Map::new();
        details.insert(
            "minReaderVersion".to_string(),
            min_reader_version.into(),
        );
        details.insert(
            "readerVersion".to_string(),
            WIRE_PROTOCOL_VERSION.into(),
        );
        return Err(WireError::with_options(
            WIRE_VERSION_TOO_LOW,
            "This session requires a newer version of Kimi Code. Upgrade Kimi Code to the latest version to read this session.",
            kimi_core::primitives::errors::Error2Options {
                details: Some(details),
                ..Default::default()
            },
        ));
    }
    Ok(())
}

pub fn resolve_wire_migrations(
    read_version: &str,
) -> Result<Vec<&'static dyn WireMigration>, WireError> {
    if compare_wire_versions(read_version, WIRE_PROTOCOL_VERSION) >= 0.0 {
        return Ok(Vec::new());
    }

    let mut migrations = Vec::new();
    let mut version = read_version.to_string();
    while compare_wire_versions(&version, WIRE_PROTOCOL_VERSION) < 0.0 {
        let Some(migration) = find_migration(&version) else {
            let mut details = Map::new();
            details.insert("version".to_string(), version.clone().into());
            return Err(WireError::with_options(
                WIRE_MIGRATION_MISSING,
                format!("Missing wire migration for version {version}"),
                kimi_core::primitives::errors::Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            ));
        };
        migrations.push(migration);
        version = migration.target_version().to_string();
    }
    Ok(migrations)
}

pub fn migrate_wire_record(
    record: WireMigrationRecord,
    migrations: &[&dyn WireMigration],
) -> WireMigrationRecord {
    migrations.iter().fold(record, |current, migration| {
        migration.migrate_record(current)
    })
}

pub fn migrate_wire_records(
    records: &[WireMigrationRecord],
    read_version: Option<&str>,
) -> Result<Vec<WireMigrationRecord>, WireError> {
    let migrations = match read_version {
        Some(read_version) => resolve_wire_migrations(read_version)?,
        None => MIGRATIONS.to_vec(),
    };
    Ok(apply_wire_migrations(records, &migrations))
}

pub fn apply_wire_migrations(
    records: &[WireMigrationRecord],
    migrations: &[&dyn WireMigration],
) -> Vec<WireMigrationRecord> {
    records
        .iter()
        .cloned()
        .map(|record| migrate_wire_record(record, migrations))
        .collect()
}

fn find_migration(source_version: &str) -> Option<&'static dyn WireMigration> {
    MIGRATIONS
        .iter()
        .copied()
        .find(|migration| migration.source_version() == source_version)
}

fn compare_wire_versions(a: &str, b: &str) -> f64 {
    let parts_a: Vec<&str> = a.split('.').collect();
    let parts_b: Vec<&str> = b.split('.').collect();
    let max_length = parts_a.len().max(parts_b.len());
    for index in 0..max_length {
        let diff = number_part(parts_a.get(index).copied())
            - number_part(parts_b.get(index).copied());
        if diff != 0.0 {
            return diff;
        }
    }
    0.0
}

fn number_part(part: Option<&str>) -> f64 {
    let part = part.unwrap_or("0").trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'
                | '\u{000b}'
                | '\u{000c}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
                | '\n'
                | '\r'
        )
    });
    if part.is_empty() {
        return 0.0;
    }
    match part {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0b", 2), ("0B", 2), ("0o", 8), ("0O", 8)] {
        if let Some(digits) = part.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut value = 0.0;
            for digit in digits.chars() {
                let Some(digit) = digit.to_digit(radix) else {
                    return f64::NAN;
                };
                value = value * radix as f64 + digit as f64;
            }
            return value;
        }
    }
    part.parse().unwrap_or(f64::NAN)
}
