// tensorzero-core/src/auth/token_validation.rs
use super::AuthInfo;
use crate::clickhouse::ClickHouseConnectionInfo;
use crate::error::{Error, ErrorDetails};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthCodeRecord {
    pub auth_code: String,
    pub tenant_id: String,
    pub username: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub is_active: bool,
    pub usage_count: u64,
}

// pub async fn validate_auth_code(
//     auth_code: &str,
//     app_state: &crate::gateway_util::AppStateData,
// ) -> Result<AuthInfo, Error> {
//     // Query ClickHouse for auth code validation
//     let query = "
//         SELECT auth_code, tenant_id, username, usage_count, is_active
//         FROM tupleap_auth_codes
//         WHERE auth_code = ? AND is_active = 1
//     ";

//     let result: Option<AuthCodeRecord> = app_state
//         .clickhouse_connection_info.
//         .query_single(query, &[auth_code])
//         .await
//         .map_err(|e| {
//             Error::new(ErrorDetails::ClickHouseQuery {
//                 message: format!("Failed to validate auth code: {}", e),
//             })
//         })?;

//     match result {
//         Some(record) => Ok(AuthInfo {
//             tenant_id: record.tenant_id,
//             username: record.username,
//             auth_code: record.auth_code,
//             usage_count: record.usage_count,
//         }),
//         None => Err(Error::new(ErrorDetails::BadCredentialsPreInference {
//             provider_name: "TUPLEAP_AUTHCODE".to_string(),
//         })),
//     }
// }

// pub async fn increment_usage_counter(
//     auth_code: &str,
//     app_state: &crate::gateway_util::AppStateData,
// ) -> Result<(), Error> {
//     let query = "
//         ALTER TABLE tupleap_auth_codes
//         UPDATE usage_count = usage_count + 1
//         WHERE auth_code = ?
//     ";

//     app_state
//         .clickhouse_connection_info
//         .execute(query, &[auth_code])
//         .await
//         .map_err(|e| {
//             Error::new(ErrorDetails::ClickHouseQuery {
//                 message: format!("Failed to increment usage counter: {}", e),
//             })
//         })?;

//     Ok(())
// }
