// tensorzero-core/src/auth/mod.rs
// use crate::error::{Error, ErrorDetails};
// use crate::gateway_util::AppStateData;
// use axum::{
//     extract::{Request, State},
//     http::{HeaderMap, StatusCode},
//     middleware::Next,
//     response::Response,
// };
// use std::collections::HashMap;

// pub mod auth_service;
pub mod token_validation;

const TUPLEAP_AUTHCODE_HEADER: &str = "TUPLEAP_AUTHCODE";

// pub async fn authenticate_request(
//     State(app_state): State<AppStateData>,
//     headers: HeaderMap,
//     mut request: Request,
//     next: Next,
// ) -> Result<Response, Error> {
//     // Extract the auth code from headers
//     let auth_code = headers
//         .get(TUPLEAP_AUTHCODE_HEADER)
//         .and_then(|h| h.to_str().ok())
//         .ok_or_else(|| {
//             Error::new(ErrorDetails::ApiKeyMissing {
//                 provider_name: "TUPLEAP_AUTHCODE".to_string(),
//             })
//         })?;

//     // Validate the auth code and extract tenant/user info
//     let auth_info = validate_auth_code(auth_code, &app_state).await?;

//     // Add authentication info to request extensions
//     request.extensions_mut().insert(auth_info);

//     // Increment usage metrics
//     increment_usage_counter(auth_code, &app_state).await?;

//     Ok(next.run(request).await)
// }

#[derive(Clone, Debug)]
pub struct AuthInfo {
    pub tenant_id: String,
    pub username: String,
    pub auth_code: String,
    pub usage_count: u64,
}
