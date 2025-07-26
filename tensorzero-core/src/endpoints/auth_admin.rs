// tensorzero-core/src/endpoints/auth_admin.rs
use crate::auth::admin_token_validation::validate_admin_token;
use crate::error::{Error, ErrorDetails};
use crate::gateway_util::{AppState, AppStateData, StructuredJson};
use axum::{extract::State, http::HeaderMap, response::Json};
use chrono::{DateTime, Days, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct GenerateAuthCodeRequest {
    pub tenant_id: String,
    pub username: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
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

    // Check if the user exists.
    let exists_query = format!(
        "SELECT count() FROM AUTHCode WHERE tenant_id = '{0}' AND username = '{1}'",
        request.tenant_id, request.username
    );
    let count_result = clickhouse_connection_info
        .run_query_synchronous_no_params(exists_query)
        .await?;
    let count: u64 =
        count_result
            .trim()
            .parse()
            .map_err(|error_details: std::num::ParseIntError| {
                Error::new(ErrorDetails::ClickHouseDeserialization {
                    message: error_details.to_string(),
                })
            })?;

    // If count is > 0 then we have an error.
    if count > 0 {
        tracing::info!(
            "The user {0} already exists in tenant {1}",
            request.username,
            request.tenant_id
        );
        return Err(Error::new(ErrorDetails::UserAlreadyExists {
            user_name: request.username,
        }));
    }
    // Generate unique auth code
    let auth_code = generate_unique_auth_code(&request.tenant_id, &request.username);
    let created_at = Utc::now();
    let expires_at = Utc::now().checked_add_days(Days::new(30));

    let created_string = created_at.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    let expires = expires_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
        .unwrap_or_default();

    // Insert into database
    let insert_query = format!(
        r#"
        INSERT INTO AUTHCode
        (auth_code, tenant_id, username, created_at, expires_at, created_by, is_active, usage_count)
        VALUES ('{auth_code}', '{0}', '{1}', '{2}', '{3}', 'admin', 1, 0)
        "#,
        request.tenant_id, request.username, created_string, expires
    );

    let _ = clickhouse_connection_info
        .run_query_synchronous_no_params(insert_query.to_string())
        .await?;
    tracing::info!("Creating auth token for {0}.", request.username);

    Ok(Json(GenerateAuthCodeResponse {
        auth_code,
        tenant_id: request.tenant_id,
        username: request.username,
        created_at,
        expires_at: request.expires_at,
    }))
}

// helper method to generate unique auth code.
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
pub struct GetAuthCodesRequest {
    pub tenant_id: String,
    pub username: String,
}

pub async fn get_auth_codes_handler(
    State(AppStateData {
        config,
        clickhouse_connection_info,
        ..
    }): AppState,
    headers: HeaderMap,
    StructuredJson(request): StructuredJson<GetAuthCodesRequest>,
) -> Result<String, Error> {
    validate_admin_token(&headers, &config)?;
    let tenant_id = request.tenant_id.clone();
    let user_name = request.username.clone();

    tracing::info!("{0}", request.tenant_id);

    let query = format!(
        r#"
        SELECT auth_code, tenant_id, username, usage_count, created_at, expires_at
        FROM AUTHCode
        WHERE tenant_id = '{tenant_id}' AND username = '{user_name}'
        ORDER BY created_at DESC
        LIMIT 1
        FORMAT JSON
    "#
    );

    let result = clickhouse_connection_info
        .run_query_synchronous_no_params(query)
        .await
        .map_err(|e| {
            Error::new(ErrorDetails::ClickHouseQuery {
                message: format!("Failed to list auth codes: {}", e),
            })
        })?;

    Ok(result)
}
