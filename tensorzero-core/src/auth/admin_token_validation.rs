// tensorzero-core/src/auth/admin.rs
use crate::config_parser::Config;
use crate::error::{Error, ErrorDetails};
use axum::http::HeaderMap;

pub fn validate_admin_token(headers: &HeaderMap, config: &Config) -> Result<(), Error> {
    let admin_token = headers
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| {
            Error::new(ErrorDetails::ApiKeyMissing {
                provider_name: "Admin".to_string(),
            })
        })?;

    // Get admin token from config
    let expected_token = config.gateway.admin_token.as_ref().ok_or_else(|| {
        Error::new(ErrorDetails::Config {
            message: "Admin token not configured".to_string(),
        })
    })?;
    // check if token passed is the admin token or not.
    if admin_token != expected_token {
        return Err(Error::new(ErrorDetails::BadCredentialsPreInference {
            provider_name: "Admin".to_string(),
        }));
    }

    Ok(())
}
