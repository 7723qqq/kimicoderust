use super::migration::{WireMigration, WireMigrationRecord};

pub struct MigrateV12ToV13;

pub static MIGRATE_V1_2_TO_V1_3: MigrateV12ToV13 = MigrateV12ToV13;

impl WireMigration for MigrateV12ToV13 {
    fn source_version(&self) -> &'static str {
        "1.2"
    }

    fn target_version(&self) -> &'static str {
        "1.3"
    }

    fn migrate_record(&self, record: WireMigrationRecord) -> WireMigrationRecord {
        record
    }
}
