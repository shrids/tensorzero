use crate::clickhouse::migration_manager::migration_trait::Migration;
use crate::clickhouse::ClickHouseConnectionInfo;
use crate::error::Error;
use async_trait::async_trait;

use super::check_table_exists;

/// This migration is used to create the initial tables in the ClickHouse database.
///
/// It is used to create the following tables:
/// - BooleanMetricFeedback
/// - CommentFeedback
/// - DemonstrationFeedback
/// - FloatMetricFeedback
/// - ChatInference
/// - JsonInference
/// - ModelInference
pub struct Migrationauth0000<'a> {
    pub clickhouse: &'a ClickHouseConnectionInfo,
}

#[async_trait]
impl Migration for Migrationauth0000<'_> {
    async fn can_apply(&self) -> Result<(), Error> {
        Ok(())
    }

    /// Check if the tables exist
    async fn should_apply(&self) -> Result<bool, Error> {
        let tables = vec!["AUTHCode"];
        for table in tables {
            match check_table_exists(self.clickhouse, table, "auth0000").await {
                Ok(exists) => {
                    if !exists {
                        return Ok(true);
                    }
                }
                // If `can_apply` succeeds but this fails, it likely means the database does not exist
                Err(_) => return Ok(true),
            }
        }

        Ok(false)
    }

    async fn apply(&self, _clean_start: bool) -> Result<(), Error> {
        // Create the `BooleanMetricFeedback` table
        let query = r#"
            CREATE TABLE IF NOT EXISTS AUTHCode (
                auth_code String,
                tenant_id String,
                username String,
                created_at DateTime64(3),
                is_active UInt8 DEFAULT 1,
                usage_count UInt64 DEFAULT 0,
                created_by String,
                expires_at Nullable(DateTime64(3))
            ) ENGINE = MergeTree()
            ORDER BY (tenant_id, auth_code);
        "#;
        let _ = self
            .clickhouse
            .run_query_synchronous_no_params(query.to_string())
            .await?;

        // Create the `CommentFeedback` table
        let query = r#"
            ALTER TABLE AUTHCode ADD INDEX IdxAuthCode auth_code TYPE bloom_filter(0.01) GRANULARITY 1;
        "#;
        let _ = self
            .clickhouse
            .run_query_synchronous_no_params(query.to_string())
            .await?;

        Ok(())
    }

    fn rollback_instructions(&self) -> String {
        let database = self.clickhouse.database();

        format!(
            "/* **CAREFUL: THIS WILL DELETE ALL DATA** */\
            /* Drop the database */\
            DROP DATABASE IF EXISTS {database};\
            /* **CAREFUL: THIS WILL DELETE ALL DATA** */"
        )
    }

    /// Check if the migration has succeeded (i.e. it should not be applied again)
    async fn has_succeeded(&self) -> Result<bool, Error> {
        let should_apply = self.should_apply().await?;
        Ok(!should_apply)
    }
}
