use std::{
    cmp::Ordering, collections::HashMap, env, fmt::Display, future::Future, path::PathBuf,
    sync::Arc, time::Duration,
};

use futures::future::join_all;
use git::GitInfo;
use reqwest::header::HeaderMap;
use reqwest_eventsource::{Event, EventSource, RequestBuilderExt};
use serde_json::Value;
use std::fmt::Debug;
use tensorzero_core::auth::AuthCache;
pub use tensorzero_core::endpoints::optimization::LaunchOptimizationParams;
pub use tensorzero_core::endpoints::optimization::LaunchOptimizationWorkflowParams;
use tensorzero_core::endpoints::optimization::{launch_optimization, launch_optimization_workflow};
pub use tensorzero_core::optimization::{OptimizerJobHandle, OptimizerStatus};
use tensorzero_core::stored_inference::{render_stored_inference, reresolve_input_for_fine_tuning};
use tensorzero_core::{
    config_parser::Config,
    endpoints::{
        datasets::InsertDatapointParams,
        dynamic_evaluation_run::{
            validate_variant_pins, DynamicEvaluationRunEpisodeParams,
            DynamicEvaluationRunEpisodeResponse,
        },
        validate_tags,
    },
    error::{Error, ErrorDetails},
    gateway_util::{setup_clickhouse, setup_http_client, AppStateData},
};
use thiserror::Error;
use tokio::{sync::Mutex, time::error::Elapsed};
use tokio_stream::StreamExt;
use url::Url;
use uuid::Uuid;

mod client_inference_params;
mod client_input;
mod git;
pub use tensorzero_core::stored_inference::{
    RenderedStoredInference, StoredChatInference, StoredInference, StoredJsonInference,
};
pub mod input_handling;
pub use client_inference_params::{ClientInferenceParams, ClientSecretString};
pub use client_input::{ClientInput, ClientInputMessage, ClientInputMessageContent};

pub use tensorzero_core::cache::CacheParamsOptions;
pub use tensorzero_core::clickhouse::query_builder::{
    BooleanMetricNode, FloatComparisonOperator, FloatMetricNode, InferenceFilterTreeNode,
    InferenceOutputSource, ListInferencesParams,
};
pub use tensorzero_core::endpoints::datasets::{
    ChatInferenceDatapoint, Datapoint, JsonInferenceDatapoint,
};
pub use tensorzero_core::endpoints::dynamic_evaluation_run::{
    DynamicEvaluationRunParams, DynamicEvaluationRunResponse,
};
pub use tensorzero_core::endpoints::feedback::FeedbackResponse;
pub use tensorzero_core::endpoints::feedback::Params as FeedbackParams;
pub use tensorzero_core::endpoints::inference::{
    InferenceOutput, InferenceParams, InferenceResponse, InferenceResponseChunk, InferenceStream,
};
pub use tensorzero_core::endpoints::object_storage::ObjectResponse;
pub use tensorzero_core::inference::types::storage::{StorageKind, StoragePath};
pub use tensorzero_core::inference::types::File;
pub use tensorzero_core::inference::types::{
    ContentBlockChunk, Input, InputMessage, InputMessageContent, Role,
};
pub use tensorzero_core::tool::{DynamicToolParams, Tool};

enum ClientMode {
    HTTPGateway(HTTPGateway),
    EmbeddedGateway {
        gateway: EmbeddedGateway,
        timeout: Option<Duration>,
    },
}

impl Debug for ClientMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientMode::HTTPGateway(_) => write!(f, "HTTPGateway"),
            ClientMode::EmbeddedGateway {
                gateway: _,
                timeout,
            } => {
                write!(f, "EmbeddedGateway {{ timeout: {timeout:?} }}")
            }
        }
    }
}

struct HTTPGateway {
    base_url: Url,
    http_client: reqwest::Client,
    gateway_version: Mutex<Option<String>>, // Needs interior mutability so it can be set on every request
}

impl HTTPGateway {
    /// Sets the gateway version on the HTTPGateway struct.
    /// This should be called if the HTTPGateway is constructed within an async context.
    pub async fn discover_initialize_gateway_version(
        &self,
        client: &Client,
    ) -> Result<(), ClientBuilderError> {
        let status_url = self.base_url.join("status").map_err(|_| {
            ClientBuilderError::GatewayVersion("Failed to construct /status URL".to_string())
        })?;
        // If the client is initialized and the ping for version fails, we simply don't set it.
        let status_response = match self.http_client.get(status_url).send().await {
            Ok(status_response) => status_response,
            Err(_) => return Ok(()),
        };

        client
            .update_gateway_version_from_headers(status_response.headers())
            .await;

        Ok(())
    }
}

struct EmbeddedGateway {
    state: AppStateData,
}

/// Used to construct a `Client`
/// Call `ClientBuilder::new` to create a new `ClientBuilder`
/// in either `HTTPGateway` or `EmbeddedGateway` mode
pub struct ClientBuilder {
    mode: ClientBuilderMode,
    http_client: Option<reqwest::Client>,
    verbose_errors: bool,
}

/// An error type representing an error from within the TensorZero gateway
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum TensorZeroError {
    #[error("HTTP Error (status code {status_code}): {text:?}")]
    Http {
        status_code: u16,
        text: Option<String>,
        #[source]
        source: TensorZeroInternalError,
    },
    #[error("{source}")] // the `source` has already been formatted (below)
    Other {
        #[source]
        source: TensorZeroInternalError,
    },
    #[error("HTTP Error: request timed out")]
    RequestTimeout,
    #[error("Failed to get git info: {source}")]
    Git {
        #[source]
        source: git2::Error,
    },
}

#[derive(Debug, Error)]
#[error("Internal TensorZero Error: {0}")]
pub struct TensorZeroInternalError(#[from] tensorzero_core::error::Error);

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ClientBuilderError {
    #[error(
        "Missing configuration: you must call `with_config_file` before calling `build` in EmbeddedGateway mode"
    )]
    MissingConfig,
    #[error(
        "Missing gateway URL: you must call `with_gateway_url` before calling `build` in HTTPGateway mode"
    )]
    MissingGatewayUrl,
    #[error("Called ClientBuilder.build_http() when not in HTTPGateway mode")]
    NotHTTPGateway,
    #[error("Failed to configure ClickHouse: {0}")]
    Clickhouse(TensorZeroError),
    #[error("Failed to parse config: {0}")]
    ConfigParsing(TensorZeroError),
    #[error("Failed to build HTTP client: {0}")]
    HTTPClientBuild(TensorZeroError),
    #[error("Failed to get gateway version: {0}")]
    GatewayVersion(String),
}

// Helper type to choose between using Debug or Display for a type
struct DisplayOrDebug<T: Debug + Display> {
    val: T,
    debug: bool,
}

impl<T: Debug + Display> Display for DisplayOrDebug<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.debug {
            write!(f, "{:?}", self.val)
        } else {
            write!(f, "{}", self.val)
        }
    }
}

/// Controls how a `Client` is run
pub enum ClientBuilderMode {
    /// In HTTPGateway mode, we make HTTP requests to a TensorZero gateway server.
    HTTPGateway { url: Url },
    /// In EmbeddedGateway mode, we run an embedded gateway using a config file.
    /// We do not launch an HTTP server - we only make outgoing HTTP requests to model providers and to ClickHouse.
    EmbeddedGateway {
        config_file: Option<PathBuf>,
        clickhouse_url: Option<String>,
        /// A timeout for all TensorZero gateway processing.
        /// If this timeout is hit, any in-progress LLM requests may be aborted.
        timeout: Option<std::time::Duration>,
        verify_credentials: bool,
    },
}

/// A `ClientBuilder` is used to construct a `Client`.
impl ClientBuilder {
    pub fn new(mode: ClientBuilderMode) -> Self {
        Self {
            mode,
            http_client: None,
            verbose_errors: false,
        }
    }

    /// Sets the `reqwest::Client` to be used when making any HTTP requests.
    /// In `EmbeddedGateway` mode, this is used for making requests to model endpoints,
    /// as well as ClickHouse.
    /// In `HTTPGateway` mode, this is used for making requests to the gateway.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Sets whether error messages should be more verbose (more `Debug` impls are used).
    /// This increases the chances of exposing sensitive information (e.g. model responses)
    /// in error messages.
    ///
    /// This is `false` by default.
    pub fn with_verbose_errors(mut self, verbose_errors: bool) -> Self {
        self.verbose_errors = verbose_errors;
        self
    }

    /// Constructs a `Client`, returning an error if the configuration is invalid.
    pub async fn build(self) -> Result<Client, ClientBuilderError> {
        match &self.mode {
            ClientBuilderMode::HTTPGateway { .. } => {
                let client = self.build_http()?;
                if let ClientMode::HTTPGateway(mode) = &client.mode {
                    mode.discover_initialize_gateway_version(&client).await?;
                }
                Ok(client)
            }
            ClientBuilderMode::EmbeddedGateway {
                config_file,
                clickhouse_url,
                timeout,
                verify_credentials,
            } => {
                let config = if let Some(config_file) = config_file {
                    Arc::new(
                        Config::load_from_path_optional_verify_credentials(
                            config_file,
                            *verify_credentials,
                        )
                        .await
                        .map_err(|e| {
                            ClientBuilderError::ConfigParsing(TensorZeroError::Other {
                                source: e.into(),
                            })
                        })?,
                    )
                } else {
                    tracing::info!("No config file provided, so only default functions will be available. Set `config_file` to specify your `tensorzero.toml`");
                    Arc::new(Config::default())
                };
                let auth_cache = AuthCache::new();

                let clickhouse_connection_info =
                    setup_clickhouse(&config, clickhouse_url.clone(), true)
                        .await
                        .map_err(|e| {
                            ClientBuilderError::Clickhouse(TensorZeroError::Other {
                                source: e.into(),
                            })
                        })?;
                let http_client = if let Some(http_client) = self.http_client {
                    http_client
                } else {
                    setup_http_client().map_err(|e| {
                        ClientBuilderError::HTTPClientBuild(TensorZeroError::Other {
                            source: e.into(),
                        })
                    })?
                };
                Ok(Client {
                    mode: ClientMode::EmbeddedGateway {
                        gateway: EmbeddedGateway {
                            state: AppStateData {
                                config,
                                http_client,
                                clickhouse_connection_info,
                                auth_cache,
                            },
                        },
                        timeout: *timeout,
                    },
                    verbose_errors: self.verbose_errors,
                    #[cfg(feature = "e2e_tests")]
                    last_body: Default::default(),
                })
            }
        }
    }

    /// Builds a `Client` in HTTPGateway mode, erroring if the mode is not HTTPGateway
    /// This allows avoiding calling the async `build` method
    pub fn build_http(self) -> Result<Client, ClientBuilderError> {
        let ClientBuilderMode::HTTPGateway { mut url } = self.mode else {
            return Err(ClientBuilderError::NotHTTPGateway);
        };
        // Enforce that the URL has a trailing slash, so that joining endpoints works correctly
        // This means that passing in a url that looks like 'http://example.com/some/prefix'
        // will result in inference requests being sent to 'http://example.com/some/prefix/inference'
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(Client {
            mode: ClientMode::HTTPGateway(HTTPGateway {
                base_url: url,
                http_client: self.http_client.unwrap_or_default(),
                gateway_version: Mutex::new(None),
            }),
            verbose_errors: self.verbose_errors,
            #[cfg(feature = "e2e_tests")]
            last_body: Default::default(),
        })
    }
}

/// A TensorZero client. This is constructed using `ClientBuilder`
#[derive(Debug)]
pub struct Client {
    mode: ClientMode,
    verbose_errors: bool,
    #[cfg(feature = "e2e_tests")]
    pub last_body: Mutex<Option<String>>,
}

impl Client {
    /// Queries the health of the ClickHouse database
    /// This does nothing in `ClientMode::HTTPGateway`
    pub async fn clickhouse_health(&self) -> Result<(), TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(_) => Ok(()),
            ClientMode::EmbeddedGateway {
                gateway,
                timeout: _,
            } => gateway
                .state
                .clickhouse_connection_info
                .health()
                .await
                .map_err(|e| TensorZeroError::Other { source: e.into() }),
        }
    }

    #[cfg(feature = "pyo3")]
    pub fn get_config(&self) -> Option<Arc<Config<'_>>> {
        match &self.mode {
            ClientMode::EmbeddedGateway { gateway, .. } => Some(gateway.state.config.clone()),
            _ => None,
        }
    }

    /// Assigns feedback for a TensorZero inference.
    /// See https://www.tensorzero.com/docs/gateway/api-reference#post-feedback
    pub async fn feedback(
        &self,
        params: FeedbackParams,
    ) -> Result<FeedbackResponse, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client
                    .base_url
                    .join("feedback")
                    .map_err(|e| TensorZeroError::Other {
                        source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                            message: format!(
                                "Failed to join base URL with /feedback endpoint: {e}"
                            ),
                        })
                        .into(),
                    })?;
                let builder = client.http_client.post(url).json(&params);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::feedback::feedback(gateway.state.clone(), params)
                        .await
                        .map_err(err_to_http)
                })
                .await?
                .0)
            }
        }
    }

    // Runs a TensorZero inference.
    // See https://www.tensorzero.com/docs/gateway/api-reference#post-inference
    pub async fn inference(
        &self,
        mut params: ClientInferenceParams,
    ) -> Result<InferenceOutput, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let gateway_version = { client.gateway_version.lock().await.clone() };
                // We only perform this adjustment in HTTP gateway mode, since the embedded gateway
                // version always matches our client version.
                try_adjust_tool_call_arguments(gateway_version.as_deref(), &mut params.input)?;
                let url =
                    client
                        .base_url
                        .join("inference")
                        .map_err(|e| TensorZeroError::Other {
                            source: tensorzero_core::error::Error::new(
                                ErrorDetails::InvalidBaseUrl {
                                    message: format!(
                                        "Failed to join base URL with /inference endpoint: {e}"
                                    ),
                                },
                            )
                            .into(),
                        })?;
                let body = serde_json::to_string(&params).map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                        message: format!(
                            "Failed to serialize inference params: {}",
                            DisplayOrDebug {
                                val: e,
                                debug: self.verbose_errors,
                            }
                        ),
                    })
                    .into(),
                })?;
                #[cfg(feature = "e2e_tests")]
                {
                    *self.last_body.lock().await = Some(body.clone());
                }
                let builder = client
                    .http_client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body);
                if params.stream.unwrap_or(false) {
                    let event_source =
                        builder.eventsource().map_err(|e| TensorZeroError::Other {
                            source: tensorzero_core::error::Error::new(ErrorDetails::JsonRequest {
                                message: format!("Error constructing event stream: {e:?}"),
                            })
                            .into(),
                        })?;
                    Ok(InferenceOutput::Streaming(
                        self.http_inference_stream(event_source).await?,
                    ))
                } else {
                    Ok(InferenceOutput::NonStreaming(
                        self.parse_http_response(builder.send().await).await?,
                    ))
                }
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::inference::inference(
                        gateway.state.config.clone(),
                        &gateway.state.http_client,
                        gateway.state.clickhouse_connection_info.clone(),
                        params.try_into().map_err(err_to_http)?,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn get_object(
        &self,
        storage_path: StoragePath,
    ) -> Result<ObjectResponse, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client
                    .base_url
                    .join("internal/object_storage")
                    .map_err(|e| TensorZeroError::Other {
                        source: tensorzero_core::error::Error::new(
                            ErrorDetails::InvalidBaseUrl {
                                message: format!(
                                    "Failed to join base URL with /internal/object_storage endpoint: {e}"
                                ),
                            },
                        )
                        .into(),
                    })?;
                let storage_path_json =
                    serde_json::to_string(&storage_path).map_err(|e| TensorZeroError::Other {
                        source: tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                            message: format!("Failed to serialize storage path: {e}"),
                        })
                        .into(),
                    })?;
                let builder = client
                    .http_client
                    .get(url)
                    .query(&[("storage_path", storage_path_json)]);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::object_storage::get_object(
                        &gateway.state.config,
                        storage_path,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn dynamic_evaluation_run(
        &self,
        mut params: DynamicEvaluationRunParams,
    ) -> Result<DynamicEvaluationRunResponse, TensorZeroError> {
        // We validate the tags here since we're going to add git information to the tags afterwards and set internal to true
        validate_tags(&params.tags, false)
            .map_err(|e| TensorZeroError::Other { source: e.into() })?;
        // Apply the git information to the tags so it gets stored for our dynamic evaluation run
        if let Ok(git_info) = GitInfo::new() {
            params.tags.extend(git_info.into_tags());
        }
        // Set internal to true so we don't validate the tags again
        params.internal = true;
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client.base_url.join("dynamic_evaluation_run").map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                        message: format!("Failed to join base URL with /dynamic_evaluation_run endpoint: {e}"),
                    })
                    .into(),
                })?;
                let builder = client.http_client.post(url).json(&params);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::dynamic_evaluation_run::dynamic_evaluation_run(
                        gateway.state.clone(),
                        params,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn dynamic_evaluation_run_episode(
        &self,
        run_id: Uuid,
        params: DynamicEvaluationRunEpisodeParams,
    ) -> Result<DynamicEvaluationRunEpisodeResponse, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client.base_url.join(&format!("dynamic_evaluation_run/{run_id}/episode")).map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                        message: format!("Failed to join base URL with /dynamic_evaluation_run/{run_id}/episode endpoint: {e}"),
                    })
                    .into(),
                })?;
                let builder = client.http_client.post(url).json(&params);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::dynamic_evaluation_run::dynamic_evaluation_run_episode(
                        gateway.state.clone(),
                        run_id,
                        params,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn bulk_insert_datapoints(
        &self,
        dataset_name: String,
        params: InsertDatapointParams,
    ) -> Result<Vec<Uuid>, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client.base_url.join(&format!("datasets/{dataset_name}/datapoints/bulk")).map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                        message: format!("Failed to join base URL with /datasets/{dataset_name}/datapoints/bulk endpoint: {e}"),
                    })
                    .into(),
                })?;
                let builder = client.http_client.post(url).json(&params);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::datasets::insert_datapoint(
                        dataset_name,
                        params,
                        &gateway.state.config,
                        &gateway.state.http_client,
                        &gateway.state.clickhouse_connection_info,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn delete_datapoint(
        &self,
        dataset_name: String,
        datapoint_id: Uuid,
    ) -> Result<(), TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client.base_url.join(&format!("datasets/{dataset_name}/datapoints/{datapoint_id}")).map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                        message: format!("Failed to join base URL with /datasets/{dataset_name}/datapoints/{datapoint_id} endpoint: {e}"),
                    })
                    .into(),
                })?;
                let builder = client.http_client.delete(url);
                let resp = builder.send().await.map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::JsonRequest {
                        message: format!("Error deleting datapoint: {e:?}"),
                    })
                    .into(),
                })?;
                if resp.status().is_success() {
                    Ok(())
                } else {
                    Err(TensorZeroError::Other {
                        source: tensorzero_core::error::Error::new(ErrorDetails::JsonRequest {
                            message: format!(
                                "Error deleting datapoint: {}",
                                resp.text().await.unwrap_or_default()
                            ),
                        })
                        .into(),
                    })
                }
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::datasets::delete_datapoint(
                        dataset_name,
                        datapoint_id,
                        &gateway.state.clickhouse_connection_info,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn list_datapoints(
        &self,
        dataset_name: String,
        function_name: Option<String>,
        limit: Option<u32>,
        offset: Option<u32>,
    ) -> Result<Vec<Datapoint>, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client.base_url.join(&format!("datasets/{dataset_name}/datapoints")).map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                        message: format!("Failed to join base URL with /datasets/{dataset_name}/datapoints endpoint: {e}"),
                    })
                    .into(),
                })?;
                let mut query_params = Vec::new();
                query_params.push(("limit", limit.unwrap_or(100).to_string()));
                query_params.push(("offset", offset.unwrap_or(0).to_string()));
                if let Some(function_name) = function_name {
                    query_params.push(("function_name", function_name));
                }
                let builder = client.http_client.get(url).query(&query_params);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::datasets::list_datapoints(
                        dataset_name,
                        &gateway.state.clickhouse_connection_info,
                        function_name,
                        limit,
                        offset,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    pub async fn get_datapoint(
        &self,
        dataset_name: String,
        datapoint_id: Uuid,
    ) -> Result<Datapoint, TensorZeroError> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                let url = client.base_url.join(&format!("datasets/{dataset_name}/datapoints/{datapoint_id}")).map_err(|e| TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::InvalidBaseUrl {
                        message: format!("Failed to join base URL with /datasets/{dataset_name}/datapoints/{datapoint_id} endpoint: {e}"),
                    })
                    .into(),
                })?;
                let builder = client.http_client.get(url);
                self.parse_http_response(builder.send().await).await
            }
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::datasets::get_datapoint(
                        dataset_name,
                        datapoint_id,
                        &gateway.state.clickhouse_connection_info,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
        }
    }

    /// Query the Clickhouse database for inferences.
    ///
    /// This function is only available in EmbeddedGateway mode.
    ///
    /// # Arguments
    ///
    /// * `function_name` - The name of the function to query.
    /// * `variant_name` - The name of the variant to query. Optional
    /// * `filters` - A filter tree to apply to the query. Optional
    /// * `output_source` - The source of the output to query. "inference" or "demonstration"
    /// * `limit` - The maximum number of inferences to return. Optional
    /// * `offset` - The offset to start from. Optional
    /// * `format` - The format to return the inferences in. For now, only "JSONEachRow" is supported.
    pub async fn experimental_list_inferences(
        &self,
        params: ListInferencesParams<'_>,
    ) -> Result<Vec<StoredInference>, TensorZeroError> {
        // TODO: consider adding a flag that returns the generated sql query
        let ClientMode::EmbeddedGateway { gateway, .. } = &self.mode else {
            return Err(TensorZeroError::Other {
                source: tensorzero_core::error::Error::new(ErrorDetails::InvalidClientMode {
                    mode: "Http".to_string(),
                    message: "This function is only available in EmbeddedGateway mode".to_string(),
                })
                .into(),
            });
        };
        let inferences = gateway
            .state
            .clickhouse_connection_info
            .list_inferences(&gateway.state.config, &params)
            .await
            .map_err(err_to_http)?;
        Ok(inferences)
    }

    /// There are two things that need to happen in this function:
    /// 1. We need to resolve all network resources (e.g. images) in the inference examples.
    /// 2. We need to prepare all messages into "simple" messages that have been templated for a particular variant.
    ///    To do this, we need to know what variant to use for each function that might appear in the data.
    ///
    /// IMPORTANT: For now, this function drops datapoints which are bad, e.g. ones where templating fails, the function
    ///            has no variant specified, or where the process of downloading resources fails.
    ///            In future we will make this behavior configurable by the caller.
    pub async fn experimental_render_inferences(
        &self,
        mut stored_inferences: Vec<StoredInference>,
        variants: HashMap<String, String>, // Map from function name to variant name
    ) -> Result<Vec<RenderedStoredInference>, TensorZeroError> {
        let ClientMode::EmbeddedGateway { gateway, .. } = &self.mode else {
            return Err(TensorZeroError::Other {
                source: tensorzero_core::error::Error::new(ErrorDetails::InvalidClientMode {
                    mode: "Http".to_string(),
                    message: "This function is only available in EmbeddedGateway mode".to_string(),
                })
                .into(),
            });
        };
        validate_variant_pins(&variants, &gateway.state.config)
            .map_err(|e| TensorZeroError::Other { source: e.into() })?;
        let resolution_futures = stored_inferences.iter_mut().map(|inference_example| {
            // Create a future for each call to reresolve_input_for_fine_tuning.
            // This function modifies inference_example.input_mut() in place.
            // `self` (the client) is passed by immutable reference.
            reresolve_input_for_fine_tuning(inference_example.input_mut(), &gateway.state.config)
        });

        // Await all futures concurrently.
        // For now, we drop the errors here.
        // They are logged on construction in the task.
        // TODO: make it configurable whether to drop or error on failures.
        let results = join_all(resolution_futures).await;

        // Ensure that the number of results matches the number of inference examples.
        // This should be guaranteed to be true based on the code above, but we assert it anyway.
        assert_eq!(
            stored_inferences.len(),
            results.len(),
            "Mismatch between number of inference examples and resolution results. This indicates a bug."
        );

        let final_rendered_examples: Vec<RenderedStoredInference> = stored_inferences
            .into_iter() // Consumes Vec<StoredInference>; elements are already mutated
            .zip(results.into_iter()) // Creates an iterator of (StoredInference, Result<(), Error>)
            .filter_map(|(example, resolution_result)| {
                // Filter out examples where reresolve_input_for_fine_tuning failed.
                // If resolution_result is Ok, map Some(()) to Some(example).
                // If resolution_result is Err, .ok() yields None, so filter_map drops it.
                resolution_result.ok().map(|_| example)
            })
            .filter_map(|resolved_example| {
                // resolved_example is a StoredInference that was successfully processed by reresolve.
                // Now, attempt to render it.
                // render_stored_inference returns Result<RenderedStoredInference, Error>.
                // .ok() converts this to Option<RenderedStoredInference>.
                // filter_map will keep Some(RenderedStoredInference) and discard None (if rendering failed).
                render_stored_inference(resolved_example, &gateway.state.config, &variants).ok()
            })
            .collect();

        Ok(final_rendered_examples)
    }

    /// Launch an optimization job.
    pub async fn experimental_launch_optimization(
        &self,
        params: tensorzero_core::endpoints::optimization::LaunchOptimizationParams,
    ) -> Result<OptimizerJobHandle, TensorZeroError> {
        match &self.mode {
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                // TODO: do we want this?
                Ok(with_embedded_timeout(*timeout, async {
                    launch_optimization(&gateway.state.http_client, params)
                        .await
                        .map_err(err_to_http)
                })
                .await?)
            }
            ClientMode::HTTPGateway(_) => Err(TensorZeroError::Other {
                source: tensorzero_core::error::Error::new(ErrorDetails::InvalidClientMode {
                    mode: "Http".to_string(),
                    message: "This function is only available in EmbeddedGateway mode".to_string(),
                })
                .into(),
            }),
        }
    }

    /// Start an optimization job.
    /// NOTE: This is the composition of `list_inferences`, `render_inferences`, and `launch_optimization`.
    pub async fn experimental_launch_optimization_workflow(
        &self,
        params: LaunchOptimizationWorkflowParams,
    ) -> Result<OptimizerJobHandle, TensorZeroError> {
        let ClientMode::EmbeddedGateway { gateway, timeout } = &self.mode else {
            return Err(TensorZeroError::Other {
                source: tensorzero_core::error::Error::new(ErrorDetails::InvalidClientMode {
                    mode: "Http".to_string(),
                    message: "This function is only available in EmbeddedGateway mode".to_string(),
                })
                .into(),
            });
        };
        with_embedded_timeout(*timeout, async {
            launch_optimization_workflow(
                &gateway.state.http_client,
                gateway.state.config.clone(),
                &gateway.state.clickhouse_connection_info,
                params,
            )
            .await
            .map_err(err_to_http)
        })
        .await
    }

    /// Poll an optimization job for status.
    pub async fn experimental_poll_optimization(
        &self,
        job_handle: OptimizerJobHandle,
    ) -> Result<OptimizerStatus, TensorZeroError> {
        match &self.mode {
            ClientMode::EmbeddedGateway { gateway, timeout } => {
                Ok(with_embedded_timeout(*timeout, async {
                    tensorzero_core::endpoints::optimization::poll_optimization(
                        &gateway.state.http_client,
                        &job_handle,
                    )
                    .await
                    .map_err(err_to_http)
                })
                .await?)
            }
            ClientMode::HTTPGateway(_) => Err(TensorZeroError::Other {
                source: tensorzero_core::error::Error::new(ErrorDetails::InvalidClientMode {
                    mode: "Http".to_string(),
                    message: "This function is only available in EmbeddedGateway mode".to_string(),
                })
                .into(),
            }),
        }
    }

    async fn parse_http_response<T: serde::de::DeserializeOwned>(
        &self,
        resp: Result<reqwest::Response, reqwest::Error>,
    ) -> Result<T, TensorZeroError> {
        let resp = resp.map_err(|e| {
            if e.is_timeout() {
                TensorZeroError::RequestTimeout
            } else {
                TensorZeroError::Other {
                    source: tensorzero_core::error::Error::new(ErrorDetails::JsonRequest {
                        message: format!(
                            "Error from server: {}",
                            DisplayOrDebug {
                                val: e,
                                debug: self.verbose_errors,
                            }
                        ),
                    })
                    .into(),
                }
            }
        })?;

        self.update_gateway_version_from_headers(resp.headers())
            .await;

        if let Err(e) = resp.error_for_status_ref() {
            let status_code = resp.status().as_u16();
            let text = resp.text().await.ok();
            return Err(TensorZeroError::Http {
                status_code,
                text,
                source: tensorzero_core::error::Error::new(ErrorDetails::JsonRequest {
                    message: format!(
                        "Request failed: {}",
                        DisplayOrDebug {
                            val: e,
                            debug: self.verbose_errors,
                        }
                    ),
                })
                .into(),
            });
        }

        resp.json().await.map_err(|e| TensorZeroError::Other {
            source: tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                message: format!(
                    "Error deserializing response: {}",
                    DisplayOrDebug {
                        val: e,
                        debug: self.verbose_errors,
                    }
                ),
            })
            .into(),
        })
    }

    async fn http_inference_stream(
        &self,
        event_source: EventSource,
    ) -> Result<InferenceStream, TensorZeroError> {
        let mut event_source = event_source.peekable();
        let first = event_source.peek().await;
        if let Some(Err(_)) = first {
            // Discard the stream if it has an error
            let res = event_source.next().await;
            #[expect(clippy::panic)]
            let Some(Err(e)) = res
            else {
                panic!("Peeked error but got non-err {res:?}");
            };
            let err_str = format!("Error in streaming response: {e:?}");
            let inner_err = tensorzero_core::error::Error::new(ErrorDetails::StreamError {
                source: Box::new(tensorzero_core::error::Error::new(
                    ErrorDetails::Serialization { message: err_str },
                )),
            });
            if let reqwest_eventsource::Error::InvalidStatusCode(code, resp) = e {
                return Err(TensorZeroError::Http {
                    status_code: code.as_u16(),
                    text: resp.text().await.ok(),
                    source: inner_err.into(),
                });
            }
            return Err(TensorZeroError::Other {
                source: tensorzero_core::error::Error::new(ErrorDetails::StreamError {
                    source: Box::new(inner_err),
                })
                .into(),
            });
        }
        let verbose_errors = self.verbose_errors;
        Ok(Box::pin(async_stream::stream! {
            while let Some(ev) = event_source.next().await {
                match ev {
                    Err(e) => {
                        if matches!(e, reqwest_eventsource::Error::StreamEnded) {
                            break;
                        }
                        yield Err(tensorzero_core::error::Error::new(ErrorDetails::StreamError {
                            source: Box::new(tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                                message: format!("Error in streaming response: {}", DisplayOrDebug {
                                    val: e,
                                    debug: verbose_errors,
                                })
                            }))
                        }))
                    }
                    Ok(e) => match e {
                        Event::Open => continue,
                        Event::Message(message) => {
                            if message.data == "[DONE]" {
                                break;
                            }
                            let json: serde_json::Value = serde_json::from_str(&message.data).map_err(|e| {
                                tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                                    message: format!("Error deserializing inference response chunk: {}", DisplayOrDebug {
                                        val: e,
                                        debug: verbose_errors,
                                    }),
                                })
                            })?;
                            if let Some(err) = json.get("error") {
                                yield Err(tensorzero_core::error::Error::new(ErrorDetails::StreamError {
                                    source: Box::new(tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                                        message: format!("Stream produced an error: {}", DisplayOrDebug {
                                            val: err,
                                            debug: verbose_errors,
                                        }),
                                    }))
                                }));
                            } else {
                                let data: InferenceResponseChunk =
                                serde_json::from_value(json).map_err(|e| {
                                    tensorzero_core::error::Error::new(ErrorDetails::Serialization {
                                        message: format!("Error deserializing json value as InferenceResponseChunk: {}", DisplayOrDebug {
                                            val: e,
                                            debug: verbose_errors,
                                        }),
                                    })
                                })?;
                                yield Ok(data);
                            }

                        }
                    }
                }
            }
        }))
    }

    #[cfg(any(feature = "e2e_tests", feature = "pyo3"))]
    pub fn get_app_state_data(&self) -> Option<&AppStateData> {
        match &self.mode {
            ClientMode::EmbeddedGateway { gateway, .. } => Some(&gateway.state),
            _ => None,
        }
    }

    #[cfg(feature = "e2e_tests")]
    pub async fn e2e_update_gateway_version(&self, version: String) {
        self.update_gateway_version(version).await;
    }

    async fn update_gateway_version_from_headers(&self, headers: &HeaderMap) {
        let mut version = headers
            .get("x-tensorzero-gateway-version")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        if cfg!(feature = "e2e_tests") {
            if let Ok(version_override) = env::var("TENSORZERO_E2E_GATEWAY_VERSION_OVERRIDE") {
                version = Some(version_override);
            }
        };
        if let Some(version) = version {
            self.update_gateway_version(version).await;
        }
    }

    async fn update_gateway_version(&self, version: String) {
        match &self.mode {
            ClientMode::HTTPGateway(client) => {
                // Acquire the lock on the gateway version
                let mut gateway_version = client.gateway_version.lock().await;
                *gateway_version = Some(version)
            }
            // Should never be called
            ClientMode::EmbeddedGateway { .. } => {}
        }
    }

    #[cfg(feature = "e2e_tests")]
    pub async fn get_gateway_version(&self) -> Option<String> {
        match &self.mode {
            ClientMode::HTTPGateway(client) => client.gateway_version.lock().await.clone(),
            ClientMode::EmbeddedGateway { .. } => None,
        }
    }
}

async fn with_embedded_timeout<R, F: Future<Output = Result<R, TensorZeroError>>>(
    timeout: Option<Duration>,
    fut: F,
) -> Result<R, TensorZeroError> {
    if let Some(timeout) = timeout {
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_: Elapsed| TensorZeroError::RequestTimeout)?
    } else {
        fut.await
    }
}

/// Compares two TensorZero version strings, returning `None`
/// if the versions cannot be meaningfully compared
/// (e.g. at least one is not in the <year>.<month>.<number> format).
fn compare_versions(first: &str, second: &str) -> Result<Ordering, TensorZeroError> {
    let extract_numbers = |s: &str| {
        s.split('.')
            .map(|x| x.parse::<u32>())
            .collect::<Result<Vec<_>, _>>()
    };
    let first_components = extract_numbers(first).map_err(|e| TensorZeroError::Other {
        source: Error::new(ErrorDetails::InternalError {
            message: format!("Failed to parse first version string `{first}`: {e}"),
        })
        .into(),
    })?;
    let second_components = extract_numbers(second).map_err(|e| TensorZeroError::Other {
        source: Error::new(ErrorDetails::InternalError {
            message: format!("Failed to parse second version string `{second}`: {e}"),
        })
        .into(),
    })?;
    if first_components.len() != second_components.len() {
        return Err(TensorZeroError::Other {
            source: Error::new(ErrorDetails::InternalError {
                message: format!(
                    "Version strings `{first}` and `{second}` have different number of components"
                ),
            })
            .into(),
        });
    }
    // Compare in lexicographical order
    Ok(first_components.cmp(&second_components))
}

fn supports_tool_call_arguments_object(gateway_version: &str) -> Result<bool, TensorZeroError> {
    // This is the first release that includes commit https://github.com/tensorzero/tensorzero/commit/0bb8832b88e767287eed6d1c7e4502ea5d2397fa
    const MIN_VERSION: &str = "2025.03.3";
    Ok(compare_versions(gateway_version, MIN_VERSION)?.is_ge())
}

fn try_adjust_tool_call_arguments(
    gateway_version: Option<&str>,
    input: &mut ClientInput,
) -> Result<(), TensorZeroError> {
    // If we know the gateway version, and it's recent enough, we skip adjusting tool call arguments.
    // We perform the adjustment if the version is known to be too old, or if we didn't get a
    // version header at all (old enough gateways don't send a version header).

    // TODO (#1410): Deprecate this behavior

    if let Some(gateway_version) = gateway_version {
        if supports_tool_call_arguments_object(gateway_version)? {
            return Ok(());
        }
    }
    for msg in &mut input.messages {
        for content in &mut msg.content {
            if let ClientInputMessageContent::ToolCall(tool_call) = content {
                if let Some(args @ Value::Object(_)) = &mut tool_call.arguments {
                    // Stringify 'arguments' to support older gateways
                    tool_call.arguments = Some(Value::String(args.to_string()));
                }
            }
        }
    }
    Ok(())
}

// This is intentionally not a `From` impl, since we only want to make fake HTTP
// errors for certain embedded gateway errors. For example, a config parsing error
// should be `TensorZeroError::Other`, not `TensorZeroError::Http`.
#[doc(hidden)]
pub fn err_to_http(e: tensorzero_core::error::Error) -> TensorZeroError {
    TensorZeroError::Http {
        status_code: e.status_code().as_u16(),
        text: Some(serde_json::json!({"error": e.to_string()}).to_string()),
        source: e.into(),
    }
}

#[cfg(feature = "pyo3")]
pub use tensorzero_core::observability;

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

    #[tokio::test]
    #[ignore] // TODO - set an environment variable, or create a new config with dummy credentials
    async fn test_missing_clickhouse() {
        // This config file requires ClickHouse, so it should fail if no ClickHouse URL is provided
        let err = ClientBuilder::new(ClientBuilderMode::EmbeddedGateway {
            config_file: Some(PathBuf::from(
                "../../examples/haiku-hidden-preferences/config/tensorzero.toml",
            )),
            clickhouse_url: None,
            timeout: None,
            verify_credentials: true,
        })
        .build()
        .await
        .expect_err("ClientBuilder should have failed");
        let err_msg = err.to_string();
        assert!(
            err.to_string().contains("Missing ClickHouse URL"),
            "Bad error message: {err_msg}"
        );
    }

    #[tokio::test]
    #[traced_test]
    async fn test_log_no_clickhouse() {
        // Default observability and no ClickHouse URL
        ClientBuilder::new(ClientBuilderMode::EmbeddedGateway {
            config_file: Some(PathBuf::from(
                "../../examples/haiku-hidden-preferences/config/tensorzero.toml",
            )),
            clickhouse_url: None,
            timeout: None,
            verify_credentials: true,
        })
        .build()
        .await
        .expect("Failed to build client");
        assert!(!logs_contain(
            "Missing environment variable TENSORZERO_CLICKHOUSE_URL"
        ));
        assert!(logs_contain("Disabling observability: `gateway.observability.enabled` is not explicitly specified in config and `clickhouse_url` was not provided."));
    }

    #[tokio::test]
    #[traced_test]
    async fn test_log_no_config() {
        ClientBuilder::new(ClientBuilderMode::EmbeddedGateway {
            config_file: None,
            clickhouse_url: None,
            timeout: None,
            verify_credentials: true,
        })
        .build()
        .await
        .expect("Failed to build client");
        assert!(!logs_contain(
            "Missing environment variable TENSORZERO_CLICKHOUSE_URL"
        ));
        assert!(logs_contain("No config file provided, so only default functions will be available. Set `config_file` to specify your `tensorzero.toml`"));
        assert!(logs_contain("Disabling observability: `gateway.observability.enabled` is not explicitly specified in config and `clickhouse_url` was not provided."));
    }

    use std::cmp::Ordering;

    use super::compare_versions;
    use super::try_adjust_tool_call_arguments;
    use serde_json::Value;
    use tensorzero_core::tool::ToolCallInput;

    #[test]
    fn test_compare_versions() {
        assert_eq!(
            compare_versions("2025.01.1", "2025.01.1").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_versions("2025.01.1", "2025.01.01").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_versions("2025.01.1", "2025.01.10").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("2026.01.1", "2025.07.8").unwrap(),
            Ordering::Greater
        );

        let missing_component = compare_versions("2025.01", "2025.01.1").unwrap_err();
        assert!(
            missing_component.to_string().contains("component"),
            "Unexpected error: {missing_component}"
        );

        let invalid_version = compare_versions("2025.01.1", "2025.01.a").unwrap_err();
        assert!(
            invalid_version.to_string().contains("invalid digit"),
            "Unexpected error: {invalid_version}"
        );
    }

    #[test]
    fn test_adjust_tool_call_args() {
        let input = ClientInput {
            system: None,
            messages: vec![ClientInputMessage {
                role: Role::User,
                content: vec![
                    ClientInputMessageContent::ToolCall(ToolCallInput {
                        name: Some("test_name".to_string()),
                        arguments: Some(serde_json::json!({
                            "key": "value"
                        })),
                        raw_arguments: Some("My raw args".to_string()),
                        id: "My id".to_string(),
                        raw_name: Some("test_raw_name".to_string()),
                    }),
                    ClientInputMessageContent::ToolCall(ToolCallInput {
                        name: Some("other_test_name".to_string()),
                        arguments: Some(Value::String("Initial string args".to_string())),
                        raw_arguments: Some("My raw args".to_string()),
                        id: "My id".to_string(),
                        raw_name: Some("test_raw_name".to_string()),
                    }),
                    ClientInputMessageContent::ToolCall(ToolCallInput {
                        name: Some("other_test_name".to_string()),
                        arguments: Some(Value::Array(vec![
                            Value::String("First entry".to_string()),
                            Value::Bool(true),
                        ])),
                        raw_arguments: Some("My raw args".to_string()),
                        id: "My id".to_string(),
                        raw_name: Some("test_raw_name".to_string()),
                    }),
                ],
            }],
        };

        let expected_adjusted = ClientInput {
            system: None,
            messages: vec![ClientInputMessage {
                role: Role::User,
                content: vec![
                    ClientInputMessageContent::ToolCall(ToolCallInput {
                        name: Some("test_name".to_string()),
                        arguments: Some(Value::String(r#"{"key":"value"}"#.to_string())),
                        raw_arguments: Some("My raw args".to_string()),
                        id: "My id".to_string(),
                        raw_name: Some("test_raw_name".to_string()),
                    }),
                    ClientInputMessageContent::ToolCall(ToolCallInput {
                        name: Some("other_test_name".to_string()),
                        arguments: Some(Value::String("Initial string args".to_string())),
                        raw_arguments: Some("My raw args".to_string()),
                        id: "My id".to_string(),
                        raw_name: Some("test_raw_name".to_string()),
                    }),
                    ClientInputMessageContent::ToolCall(ToolCallInput {
                        name: Some("other_test_name".to_string()),
                        arguments: Some(Value::Array(vec![
                            Value::String("First entry".to_string()),
                            Value::Bool(true),
                        ])),
                        raw_arguments: Some("My raw args".to_string()),
                        id: "My id".to_string(),
                        raw_name: Some("test_raw_name".to_string()),
                    }),
                ],
            }],
        };

        // Versions greater or equal to `2025.03.3` should not cause the input to be adjusted
        let mut non_adjusted = input.clone();
        try_adjust_tool_call_arguments(Some("2025.03.3"), &mut non_adjusted).unwrap();
        assert_eq!(input, non_adjusted);
        try_adjust_tool_call_arguments(Some("2026.01.01"), &mut non_adjusted).unwrap();
        assert_eq!(input, non_adjusted);

        // When we don't provide a gateway version, the input should be adjusted
        let mut adjusted_no_version = input.clone();
        try_adjust_tool_call_arguments(None, &mut adjusted_no_version).unwrap();
        assert_eq!(expected_adjusted, adjusted_no_version);

        // When we provide a version lower than `2025.03.3`, the input should be adjusted
        let mut adjusted_with_version = input.clone();
        try_adjust_tool_call_arguments(Some("2025.03.2"), &mut adjusted_with_version).unwrap();
        assert_eq!(expected_adjusted, adjusted_with_version);
    }
}
