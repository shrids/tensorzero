use crate::auth::token_validation::{increment_usage_and_stats, validate_auth_code_with_cache};
// tensorzero-core/src/auth/mod.rs
use crate::error::{Error, ErrorDetails};
use crate::gateway_util::AppStateData;

use std::collections::HashMap;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;
use std::time::Duration;

// tensorzero-core/src/auth/token_validation.rs
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod admin_token_validation;
pub mod token_validation;

const TUPLEAP_AUTHCODE_HEADER: &str = "TUPLEAP_AUTHCODE";

#[derive(Clone, Debug)]
pub struct AuthInfo {
    pub tenant_id: String,
    pub username: String,
    pub auth_code: String,
    pub is_valid: bool,
}

#[derive(Clone, Debug)]
pub struct AuthCache {
    cache: Arc<Cache<String, AuthInfo>>,
}

impl AuthCache {
    pub fn new() -> Self {
        tracing::info!("Creating a new instance of AuthCache");

        let cache = Cache::builder()
            .max_capacity(10_000) // Configurable based on needs
            .time_to_live(Duration::from_secs(60 * 60)) // 60 minutes TTL
            .build();

        Self {
            cache: Arc::new(cache),
        }
    }

    pub async fn get(&self, key: &str) -> Option<AuthInfo> {
        self.cache.get(key).await
    }

    pub async fn insert(&self, key: String, value: AuthInfo) {
        self.cache.insert(key, value).await;
    }
}

pub async fn authenticate_request(
    State(app_state): State<AppStateData>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, Error> {
    tracing::info!("Entering authenticate request");
    // Extract the auth code from headers
    let auth_code = headers
        .get(TUPLEAP_AUTHCODE_HEADER)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| {
            Error::new(ErrorDetails::ApiKeyMissing {
                provider_name: "TUPLEAP_AUTHCODE".to_string(),
            })
        })?;

    // Validate the auth code using cache-first approach
    let auth_info = validate_auth_code_with_cache(auth_code, &app_state)
        .await
        .map_err(|_| {
            Error::new(ErrorDetails::InvalidAuthToken {
                provider_name: "TUPLEAP_AUTHCODE".to_string(),
            })
        })?;

    if !auth_info.is_valid {
        return Err(Error::new(ErrorDetails::InvalidAuthToken {
            provider_name: "TUPLEAP_AUTHCODE".to_string(),
        }));
    }

    // Add authentication info to request extensions
    request.extensions_mut().insert(auth_info.clone());

    // Increment usage counter and API stats
    increment_usage_and_stats(auth_code, &app_state).await?;

    Ok(next.run(request).await)
}
