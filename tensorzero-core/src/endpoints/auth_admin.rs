// tensorzero-core/src/endpoints/auth_admin.rs
use crate::auth::admin::validate_admin_token;
use crate::error::{Error, ErrorDetails};
use crate::gateway_util::{AppState, AppStateData, StructuredJson};
use axum::{extract::State, http::HeaderMap, response::Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct GenerateAuthCodeRequest {
    pub tenant_id: String,
    pub username: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct GenerateAuthCodeResponse {
    pub auth_code: String,
    pub tenant_id: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

pub async fn generate_auth_code_handler(
    State(AppStateData {
        config,
        clickhouse_connection_info,
        ..
    }): AppState,
    headers: HeaderMap,
    StructuredJson(request): StructuredJson<GenerateAuthCodeRequest>,
) -> Result<Json<GenerateAuthCodeResponse>, Error> {
    // Validate admin token
    validate_admin_token(&headers, &config)?;

    // Generate unique auth code
    let auth_code = generate_unique_auth_code(&request.tenant_id, &request.username);
    let created_at = Utc::now();

    // Insert into database
    let insert_query = "
        INSERT INTO tupleap_auth_codes
        (auth_code, tenant_id, username, created_at, expires_at, created_by, is_active, usage_count)
        VALUES (?, ?, ?, ?, ?, 'admin', 1, 0)
    ";

    clickhouse_connection_info
        .execute(
            insert_query,
            &[
                &auth_code,
                &request.tenant_id,
                &request.username,
                &created_at.to_rfc3339(),
                &request
                    .expires_at
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default(),
            ],
        )
        .await
        .map_err(|e| {
            Error::new(ErrorDetails::ClickHouseQuery {
                message: format!("Failed to insert auth code: {}", e),
            })
        })?;

    Ok(Json(GenerateAuthCodeResponse {
        auth_code,
        tenant_id: request.tenant_id,
        username: request.username,
        created_at,
        expires_at: request.expires_at,
    }))
}

fn generate_unique_auth_code(tenant_id: &str, username: &str) -> String {
    // Create a deterministic but unique auth code
    let timestamp = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let random_suffix = Uuid::new_v4().to_string().replace("-", "")[..8].to_string();

    // Format: tupleap_{tenant}_{user}_{timestamp}_{random}
    format!(
        "tupleap_{}_{}_{}_{}",
        tenant_id.replace(" ", "_"),
        username.replace(" ", "_"),
        timestamp,
        random_suffix
    )
}

// Additional endpoint to list auth codes for a tenant
#[derive(Debug, Deserialize)]
pub struct ListAuthCodesRequest {
    pub tenant_id: Option<String>,
    pub username: Option<String>,
    pub limit: Option<u32>,
}

pub async fn list_auth_codes_handler(
    State(AppStateData {
        config,
        clickhouse_connection_info,
        ..
    }): AppState,
    headers: HeaderMap,
    StructuredJson(request): StructuredJson<ListAuthCodesRequest>,
) -> Result<Json<Vec<GenerateAuthCodeResponse>>, Error> {
    validate_admin_token(&headers, &config)?;

    let mut query = "SELECT auth_code, tenant_id, username, created_at, expires_at FROM tupleap_auth_codes WHERE is_active = 1".to_string();
    let mut params = Vec::new();

    if let Some(tenant_id) = &request.tenant_id {
        query.push_str(" AND tenant_id = ?");
        params.push(tenant_id.as_str());
    }

    if let Some(username) = &request.username {
        query.push_str(" AND username = ?");
        params.push(username.as_str());
    }

    query.push_str(" ORDER BY created_at DESC");

    if let Some(limit) = request.limit {
        query.push_str(&format!(" LIMIT {}", limit));
    }

    let results: Vec<GenerateAuthCodeResponse> = clickhouse_connection_info
        .query(&query, &params)
        .await
        .map_err(|e| {
            Error::new(ErrorDetails::ClickHouseQuery {
                message: format!("Failed to list auth codes: {}", e),
            })
        })?;

    Ok(Json(results))
}
