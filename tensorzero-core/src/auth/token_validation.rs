use std::collections::HashMap;

// tensorzero-core/src/auth/token_validation.rs
use super::{AuthCache, AuthInfo};
use crate::clickhouse::ClickHouseConnectionInfo;
use crate::error::{Error, ErrorDetails};
use crate::gateway_util::AppStateData;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthCodeRecord {
    pub auth_code: String,
    pub tenant_id: String,
    pub username: String,
    pub created_at: String,
    pub is_active: u8,
    pub usage_count: u64,
    pub expires_at: Option<String>,
}

pub async fn validate_auth_code_with_cache(
    auth_code: &str,
    app_state: &AppStateData,
) -> Result<AuthInfo, Error> {
    let admin_token = app_state
        .config
        .gateway
        .admin_token
        .clone()
        .unwrap_or("".into());
    tracing::info!("Current Admin token is {admin_token}");
    // First, check the cache
    if let Some(cached_info) = app_state.auth_cache.get(auth_code).await {
        tracing::info!("Found auth key in cache");

        return Ok(cached_info);
    }

    // Cache miss - query ClickHouse
    let auth_info = validate_auth_code_from_db(auth_code, app_state).await?;

    // Cache the result
    app_state
        .auth_cache
        .insert(auth_code.to_string(), auth_info.clone())
        .await;

    Ok(auth_info)
}

async fn validate_auth_code_from_db(
    auth_code: &str,
    app_state: &AppStateData,
) -> Result<AuthInfo, Error> {
    tracing::info!("Auth Cache miss, checking the DB");
    let query = format!(
        r#"
        SELECT auth_code, tenant_id, username, is_active
        FROM AUTHCode
        WHERE auth_code = '{auth_code}' AND is_active = 1
    "#
    );

    let result = app_state
        .clickhouse_connection_info
        .run_query_synchronous_no_params(query.to_string())
        .await?;
    // if result is empty it implies the credential is invalid.
    if result.is_empty() {
        return Err(Error::new(ErrorDetails::InvalidAuthToken {
            provider_name: "TUPLEAP_AUTHCODE".to_string(),
        }));
    }
    tracing::info!("validate_auth_code_from_db: Query result {result}");
    Ok(AuthInfo {
        tenant_id: "".into(),
        username: "".into(),
        auth_code: "".into(),
        is_valid: true,
    })
}

pub async fn increment_usage_and_stats(
    auth_code: &str,
    app_state: &AppStateData,
) -> Result<(), Error> {
    // Increment usage counter
    let usage_query = format!(
        r#"
        ALTER TABLE AUTHCode
        UPDATE usage_count = usage_count + 1
        WHERE auth_code = '{auth_code}'
    "#
    );
    app_state
        .clickhouse_connection_info
        .run_query_synchronous_no_params(usage_query.to_string())
        .await?;
    Ok(())
}
