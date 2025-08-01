use std::borrow::Cow;
use std::sync::OnceLock;
use std::time::Duration;

use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::Instant;
use url::Url;

use crate::cache::ModelProviderRequest;
use crate::endpoints::inference::InferenceCredentials;
use crate::error::{DisplayOrDebugGateway, Error, ErrorDetails};
use crate::inference::types::batch::{BatchRequestRow, PollBatchInferenceResponse};
use crate::inference::types::{
    batch::StartBatchProviderInferenceResponse, Latency, ModelInferenceRequest,
    ModelInferenceRequestJsonMode, PeekableProviderInferenceResponseStream,
    ProviderInferenceResponse, ProviderInferenceResponseArgs,
};
use crate::inference::types::{
    ContentBlockChunk, ContentBlockOutput, FinishReason, ProviderInferenceResponseChunk,
    ProviderInferenceResponseStreamInner, TextChunk,
};
use crate::inference::InferenceProvider;
use crate::model::{build_creds_caching_default, Credential, CredentialLocation, ModelProvider};
use crate::providers::helpers::{
    inject_extra_request_data_and_send, inject_extra_request_data_and_send_eventsource,
};
use crate::providers::openai::check_api_base_suffix;
use crate::tool::ToolCallChunk;

use super::openai::{
    get_chat_url, handle_openai_error, prepare_openai_messages, prepare_openai_tools,
    OpenAIRequestMessage, OpenAIResponse, OpenAIResponseChoice, OpenAITool, OpenAIToolChoice,
    OpenAIUsage, StreamOptions,
};

fn default_api_key_location() -> CredentialLocation {
    CredentialLocation::Env("SGLANG_API_KEY".to_string())
}

const PROVIDER_NAME: &str = "SGLang";
const PROVIDER_TYPE: &str = "sglang";

#[derive(Debug)]
pub struct SGLangProvider {
    model_name: String,
    api_base: Url,
    credentials: SGLangCredentials,
}

static DEFAULT_CREDENTIALS: OnceLock<SGLangCredentials> = OnceLock::new();

impl SGLangProvider {
    pub fn new(
        model_name: String,
        api_base: Url,
        api_key_location: Option<CredentialLocation>,
    ) -> Result<Self, Error> {
        let credentials = build_creds_caching_default(
            api_key_location,
            default_api_key_location(),
            PROVIDER_TYPE,
            &DEFAULT_CREDENTIALS,
        )?;

        // Check if the api_base has the `/chat/completions` suffix and warn if it does
        check_api_base_suffix(&api_base);

        Ok(SGLangProvider {
            model_name,
            api_base,
            credentials,
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

#[derive(Clone, Debug)]
pub enum SGLangCredentials {
    Static(SecretString),
    Dynamic(String),
    None,
}

impl TryFrom<Credential> for SGLangCredentials {
    type Error = Error;

    fn try_from(credentials: Credential) -> Result<Self, Error> {
        match credentials {
            Credential::Static(key) => Ok(SGLangCredentials::Static(key)),
            Credential::Dynamic(key_name) => Ok(SGLangCredentials::Dynamic(key_name)),
            Credential::None => Ok(SGLangCredentials::None),
            Credential::Missing => Ok(SGLangCredentials::None),
            _ => Err(Error::new(ErrorDetails::Config {
                message: "Invalid api_key_location for SGLang provider".to_string(),
            })),
        }
    }
}

impl SGLangCredentials {
    pub fn get_api_key<'a>(
        &'a self,
        dynamic_api_keys: &'a InferenceCredentials,
    ) -> Result<Option<&'a SecretString>, Error> {
        match self {
            SGLangCredentials::Static(api_key) => Ok(Some(api_key)),
            SGLangCredentials::Dynamic(key_name) => {
                Some(dynamic_api_keys.get(key_name).ok_or_else(|| {
                    ErrorDetails::ApiKeyMissing {
                        provider_name: PROVIDER_NAME.to_string(),
                    }
                    .into()
                }))
                .transpose()
            }
            SGLangCredentials::None => Ok(None),
        }
    }
}

impl InferenceProvider for SGLangProvider {
    async fn infer<'a>(
        &'a self,
        ModelProviderRequest {
            request,
            provider_name: _,
            model_name,
        }: ModelProviderRequest<'a>,
        http_client: &'a reqwest::Client,
        dynamic_api_keys: &'a InferenceCredentials,
        model_provider: &'a ModelProvider,
    ) -> Result<ProviderInferenceResponse, Error> {
        let request_body = serde_json::to_value(SGLangRequest::new(&self.model_name, request)?)
            .map_err(|e| {
                Error::new(ErrorDetails::Serialization {
                    message: format!(
                        "Error serializing SGLang request: {}",
                        DisplayOrDebugGateway::new(e)
                    ),
                })
            })?;
        let request_url = get_chat_url(&self.api_base)?;
        let api_key = self.credentials.get_api_key(dynamic_api_keys)?;
        let start_time = Instant::now();
        let mut request_builder = http_client.post(request_url);
        if let Some(api_key) = api_key {
            request_builder = request_builder.bearer_auth(api_key.expose_secret());
        }
        let (res, raw_request) = inject_extra_request_data_and_send(
            PROVIDER_TYPE,
            &request.extra_body,
            &request.extra_headers,
            model_provider,
            model_name,
            request_body,
            request_builder,
        )
        .await?;
        if res.status().is_success() {
            let raw_response = res.text().await.map_err(|e| {
                Error::new(ErrorDetails::InferenceServer {
                    message: format!(
                        "Error parsing text response: {}",
                        DisplayOrDebugGateway::new(e)
                    ),
                    raw_request: Some(raw_request.clone()),
                    raw_response: None,
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?;

            let response = serde_json::from_str(&raw_response).map_err(|e| {
                Error::new(ErrorDetails::InferenceServer {
                    message: format!(
                        "Error parsing JSON response: {}",
                        DisplayOrDebugGateway::new(e)
                    ),
                    raw_request: Some(raw_request.clone()),
                    raw_response: Some(raw_response.clone()),
                    provider_type: PROVIDER_TYPE.to_string(),
                })
            })?;

            let latency = Latency::NonStreaming {
                response_time: start_time.elapsed(),
            };
            Ok(SGLangResponseWithMetadata {
                response,
                latency,
                raw_response,
                raw_request,
                generic_request: request,
            }
            .try_into()?)
        } else {
            Err(handle_openai_error(
                &raw_request,
                res.status(),
                &res.text().await.map_err(|e| {
                    Error::new(ErrorDetails::InferenceServer {
                        message: format!(
                            "Error parsing error response: {}",
                            DisplayOrDebugGateway::new(e)
                        ),
                        raw_request: Some(raw_request.clone()),
                        raw_response: None,
                        provider_type: PROVIDER_TYPE.to_string(),
                    })
                })?,
                PROVIDER_TYPE,
            ))
        }
    }

    async fn infer_stream<'a>(
        &'a self,
        ModelProviderRequest {
            request,
            provider_name: _,
            model_name,
        }: ModelProviderRequest<'a>,
        http_client: &'a reqwest::Client,
        dynamic_api_keys: &'a InferenceCredentials,
        model_provider: &'a ModelProvider,
    ) -> Result<(PeekableProviderInferenceResponseStream, String), Error> {
        let request_body = serde_json::to_value(SGLangRequest::new(&self.model_name, request)?)
            .map_err(|e| {
                Error::new(ErrorDetails::Serialization {
                    message: format!(
                        "Error serializing SGLang request: {}",
                        DisplayOrDebugGateway::new(e)
                    ),
                })
            })?;

        let request_url = get_chat_url(&self.api_base)?;
        let api_key = self.credentials.get_api_key(dynamic_api_keys)?;
        let start_time = Instant::now();
        let mut request_builder = http_client.post(request_url);
        if let Some(api_key) = api_key {
            request_builder = request_builder.bearer_auth(api_key.expose_secret());
        }
        let (event_source, raw_request) = inject_extra_request_data_and_send_eventsource(
            PROVIDER_TYPE,
            &request.extra_body,
            &request.extra_headers,
            model_provider,
            model_name,
            request_body,
            request_builder,
        )
        .await?;

        let stream = stream_sglang(event_source, start_time).peekable();
        Ok((stream, raw_request))
    }

    async fn start_batch_inference<'a>(
        &'a self,
        _requests: &'a [ModelInferenceRequest<'_>],
        _client: &'a reqwest::Client,
        _dynamic_api_keys: &'a InferenceCredentials,
    ) -> Result<StartBatchProviderInferenceResponse, Error> {
        Err(ErrorDetails::UnsupportedModelProviderForBatchInference {
            provider_type: PROVIDER_TYPE.to_string(),
        }
        .into())
    }

    async fn poll_batch_inference<'a>(
        &'a self,
        _batch_request: &'a BatchRequestRow<'a>,
        _http_client: &'a reqwest::Client,
        _dynamic_api_keys: &'a InferenceCredentials,
    ) -> Result<PollBatchInferenceResponse, Error> {
        Err(ErrorDetails::UnsupportedModelProviderForBatchInference {
            provider_type: PROVIDER_TYPE.to_string(),
        }
        .into())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SGLangFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    #[serde(other)]
    Unknown,
}

impl From<SGLangFinishReason> for FinishReason {
    fn from(reason: SGLangFinishReason) -> Self {
        match reason {
            SGLangFinishReason::Stop => FinishReason::Stop,
            SGLangFinishReason::Length => FinishReason::Length,
            SGLangFinishReason::ToolCalls => FinishReason::ToolCall,
            SGLangFinishReason::ContentFilter => FinishReason::ContentFilter,
            SGLangFinishReason::Unknown => FinishReason::Unknown,
        }
    }
}

// Streaming-specific structs
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SGLangFunctionCallChunk {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SGLangToolCallChunk {
    index: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    function: SGLangFunctionCallChunk,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SGLangDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<SGLangToolCallChunk>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SGLangChatChunkChoice {
    delta: SGLangDelta,
    #[serde(default)]
    finish_reason: Option<SGLangFinishReason>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct SGLangChatChunk {
    choices: Vec<SGLangChatChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<OpenAIUsage>,
}

/// Streams the SGLang response events and converts them into ProviderInferenceResponseChunks
/// This function handles parsing and processing of thinking blocks with proper state tracking
fn stream_sglang(
    mut event_source: EventSource,
    start_time: Instant,
) -> ProviderInferenceResponseStreamInner {
    let mut tool_call_ids = Vec::new();
    Box::pin(async_stream::stream! {
        while let Some(ev) = event_source.next().await {
            match ev {
                Err(e) => {
                    let message = e.to_string();
                    let mut raw_response = None;
                    if let reqwest_eventsource::Error::InvalidStatusCode(_, resp) = e {
                        raw_response = resp.text().await.ok();
                    }
                    yield Err(ErrorDetails::InferenceServer {
                        message,
                        raw_request: None,
                        raw_response,
                        provider_type: PROVIDER_TYPE.to_string(),
                    }.into());
                }
                Ok(event) => match event {
                    Event::Open => continue,
                    Event::Message(message) => {
                        if message.data == "[DONE]" {
                            break;
                        }
                        let data: Result<SGLangChatChunk, Error> =
                            serde_json::from_str(&message.data).map_err(|e| Error::new(ErrorDetails::InferenceServer {
                                message: format!("Error parsing chunk. Error: {e}"),
                                raw_request: None,
                                raw_response: Some(message.data.clone()),
                                provider_type: PROVIDER_TYPE.to_string(),
                            }));

                        let latency = start_time.elapsed();
                        let stream_message = data.and_then(|d| {
                            sglang_to_tensorzero_chunk(message.data, d, latency, &mut tool_call_ids)
                        });
                        yield stream_message;
                    }
                },
            }
        }

        event_source.close();
    })
}

/// Maps a SGLang chunk to a TensorZero chunk for streaming inferences
///
/// This function handles the conversion of SGLang chat chunks into TensorZero chunks.
/// It processes the content and tool calls from the SGLang response, updating the tool call IDs and names.
fn sglang_to_tensorzero_chunk(
    raw_message: String,
    mut chunk: SGLangChatChunk,
    latency: Duration,
    tool_call_ids: &mut Vec<String>,
) -> Result<ProviderInferenceResponseChunk, Error> {
    if chunk.choices.len() > 1 {
        return Err(ErrorDetails::InferenceServer {
            message: "Response has invalid number of choices: {}. Expected 1.".to_string(),
            raw_request: None,
            raw_response: Some(serde_json::to_string(&chunk).unwrap_or_default()),
            provider_type: PROVIDER_TYPE.to_string(),
        }
        .into());
    }
    let usage = chunk.usage.map(|u| u.into());
    let mut finish_reason = None;
    let mut content = vec![];
    if let Some(choice) = chunk.choices.pop() {
        if let Some(reason) = choice.finish_reason {
            finish_reason = Some(reason.into());
        }
        if let Some(text) = choice.delta.content {
            content.push(ContentBlockChunk::Text(TextChunk {
                text: text.to_string(),
                id: "0".to_string(),
            }));
        }
        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                let index = tool_call.index;
                let id = match tool_call.id {
                    Some(id) => {
                        tool_call_ids.push(id.clone());
                        id
                    }
                    None => {
                        // NOTE: SGLang does not always provide an index for the tool call.
                        // In this case, we assume it is zero as they hardcode a tool call id of zero.
                        tool_call_ids
                            .get(index.unwrap_or_default() as usize)
                            .ok_or_else(|| Error::new(ErrorDetails::InferenceServer {
                                message: "Tool call index out of bounds (meaning we haven't seen this many ids in the stream)".to_string(),
                                raw_request: None,
                                raw_response: None,
                                provider_type: PROVIDER_TYPE.to_string(),
                            }))?
                            .clone()
                    }
                };

                content.push(ContentBlockChunk::ToolCall(ToolCallChunk {
                    id,
                    raw_name: tool_call.function.name,
                    raw_arguments: tool_call.function.arguments.unwrap_or_default(),
                }));
            }
        }
    }

    Ok(ProviderInferenceResponseChunk::new(
        content,
        usage,
        raw_message,
        latency,
        finish_reason,
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
enum SGLangResponseFormat {
    #[default]
    Text,
    JsonSchema {
        json_schema: Value,
    },
}

impl SGLangResponseFormat {
    fn new(
        json_mode: &ModelInferenceRequestJsonMode,
        output_schema: Option<&Value>,
    ) -> Result<Option<Self>, Error> {
        match json_mode {
            // For now, we never explicitly send `SGLangResponseFormat::Text`
            ModelInferenceRequestJsonMode::Off => Ok(None),
            ModelInferenceRequestJsonMode::On | ModelInferenceRequestJsonMode::Strict => {
                if let Some(schema) = output_schema {
                    let json_schema = json!({"name": "response", "strict": true, "schema": schema});
                    return Ok(Some(SGLangResponseFormat::JsonSchema { json_schema }));
                }
                Err(ErrorDetails::InvalidRequest {
                    message: "The SGL models requires a schema to be provided in json mode"
                        .to_string(),
                }
                .into())
            }
        }
    }
}

/// This struct defines the supported parameters for the OpenAI API
/// See the [OpenAI API documentation](https://platform.openai.com/docs/api-reference/chat/create)
/// for more details.
/// We are not handling logprobs, top_logprobs, n,
/// presence_penalty, seed, service_tier, stop, user,
/// or the deprecated function_call and functions arguments.
#[derive(Debug, Serialize)]
struct SGLangRequest<'a> {
    messages: Vec<OpenAIRequestMessage<'a>>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<SGLangResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<OpenAIToolChoice<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Cow<'a, [String]>>,
}

impl<'a> SGLangRequest<'a> {
    pub fn new(
        model: &'a str,
        request: &'a ModelInferenceRequest<'_>,
    ) -> Result<SGLangRequest<'a>, Error> {
        let response_format = SGLangResponseFormat::new(&request.json_mode, request.output_schema)?;
        let stream_options = match request.stream {
            true => Some(StreamOptions {
                include_usage: true,
            }),
            false => None,
        };
        let messages = prepare_openai_messages(
            request.system.as_deref(),
            &request.messages,
            Some(&request.json_mode),
            PROVIDER_TYPE,
        )?;

        let (tools, tool_choice, parallel_tool_calls) = prepare_openai_tools(request);
        Ok(SGLangRequest {
            messages,
            model,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            seed: request.seed,
            top_p: request.top_p,
            presence_penalty: request.presence_penalty,
            frequency_penalty: request.frequency_penalty,
            stream: request.stream,
            stream_options,
            response_format,
            tools,
            tool_choice,
            parallel_tool_calls,
            stop: request.borrow_stop_sequences(),
        })
    }
}

struct SGLangResponseWithMetadata<'a> {
    response: OpenAIResponse,
    latency: Latency,
    raw_response: String,
    raw_request: String,
    generic_request: &'a ModelInferenceRequest<'a>,
}

impl<'a> TryFrom<SGLangResponseWithMetadata<'a>> for ProviderInferenceResponse {
    type Error = Error;
    fn try_from(value: SGLangResponseWithMetadata<'a>) -> Result<Self, Self::Error> {
        let SGLangResponseWithMetadata {
            mut response,
            latency,
            raw_response,
            raw_request,
            generic_request,
        } = value;
        if response.choices.len() != 1 {
            return Err(ErrorDetails::InferenceServer {
                message: format!(
                    "Response has invalid number of choices: {}. Expected 1.",
                    response.choices.len()
                ),
                raw_request: Some(raw_request.clone()),
                raw_response: Some(serde_json::to_string(&response).unwrap_or_default()),
                provider_type: PROVIDER_TYPE.to_string(),
            }
            .into());
        }
        let usage = response.usage.into();
        let OpenAIResponseChoice {
            message,
            finish_reason,
            ..
        } = response
            .choices
            .pop()
            .ok_or_else(|| Error::new(ErrorDetails::InferenceServer {
                message: "Response has no choices (this should never happen). Please file a bug report: https://github.com/tensorzero/tensorzero/issues/new".to_string(),
                provider_type: PROVIDER_TYPE.to_string(),
                raw_request: Some(raw_request.clone()),
                raw_response: Some(raw_response.clone()),
            }))?;
        let mut content: Vec<ContentBlockOutput> = Vec::new();
        if let Some(text) = message.content {
            content.push(text.into());
        }
        if let Some(tool_calls) = message.tool_calls {
            for tool_call in tool_calls {
                content.push(ContentBlockOutput::ToolCall(tool_call.into()));
            }
        }
        let system = generic_request.system.clone();
        let input_messages = generic_request.messages.clone();
        Ok(ProviderInferenceResponse::new(
            ProviderInferenceResponseArgs {
                output: content,
                system,
                input_messages,
                raw_request,
                raw_response: raw_response.clone(),
                usage,
                latency,
                finish_reason: Some(finish_reason.into()),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::{borrow::Cow, time::Duration};
    use tracing_test::traced_test;
    use uuid::Uuid;

    use crate::{
        inference::types::{
            FinishReason, FunctionType, ModelInferenceRequestJsonMode, RequestMessage, Role,
        },
        providers::{
            openai::{
                OpenAIFinishReason, OpenAIResponseChoice, OpenAIResponseMessage,
                OpenAIToolChoiceString, OpenAIUsage,
            },
            test_helpers::{MULTI_TOOL_CONFIG, QUERY_TOOL, WEATHER_TOOL, WEATHER_TOOL_CONFIG},
        },
        tool::{ToolCallConfig, ToolChoice},
    };

    use super::*;

    #[test]
    fn test_sglang_request_new() {
        let model_name = PROVIDER_TYPE.to_string();
        let basic_request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![
                RequestMessage {
                    role: Role::User,
                    content: vec!["Hello".to_string().into()],
                },
                RequestMessage {
                    role: Role::Assistant,
                    content: vec!["Hi there!".to_string().into()],
                },
            ],
            system: None,
            tool_config: None,
            temperature: Some(0.7),
            max_tokens: Some(100),
            seed: Some(69),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            stream: true,
            json_mode: ModelInferenceRequestJsonMode::Off,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        let sglang_request = SGLangRequest::new(&model_name, &basic_request).unwrap();

        assert_eq!(sglang_request.model, &model_name);
        assert_eq!(sglang_request.messages.len(), 2);
        assert_eq!(sglang_request.temperature, Some(0.7));
        assert_eq!(sglang_request.max_tokens, Some(100));
        assert_eq!(sglang_request.seed, Some(69));
        assert_eq!(sglang_request.top_p, Some(0.9));
        assert_eq!(sglang_request.presence_penalty, Some(0.1));
        assert_eq!(sglang_request.frequency_penalty, Some(0.2));
        assert!(sglang_request.stream);
        assert!(sglang_request.tools.is_none());
        assert_eq!(sglang_request.tool_choice, None);
        assert!(sglang_request.parallel_tool_calls.is_none());

        // Test that non-strict JSON mode requires an output schema
        let request_with_tools = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec!["What's the weather?".to_string().into()],
            }],
            system: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::On,
            tool_config: Some(Cow::Borrowed(&WEATHER_TOOL_CONFIG)),
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        SGLangRequest::new(&model_name, &request_with_tools).expect_err("requires a schema");

        // Test request with in strict JSON mode requires an no output schema
        let request_with_tools = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec!["What's the weather?".to_string().into()],
            }],
            system: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Strict,
            tool_config: None,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        SGLangRequest::new(&model_name, &request_with_tools).expect_err("requires a schema");

        // Test request with strict JSON mode with an output schema
        let output_schema = json!({});
        let request_with_tools = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec!["What's the weather?".to_string().into()],
            }],
            system: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Strict,
            tool_config: None,
            function_type: FunctionType::Chat,
            output_schema: Some(&output_schema),
            extra_body: Default::default(),
            ..Default::default()
        };

        let sglang_request = SGLangRequest::new(&model_name, &request_with_tools).unwrap();

        assert_eq!(sglang_request.model, &model_name);
        assert_eq!(sglang_request.messages.len(), 1);
        assert_eq!(sglang_request.temperature, None);
        assert_eq!(sglang_request.max_tokens, None);
        assert_eq!(sglang_request.seed, None);
        assert!(!sglang_request.stream);
        assert_eq!(sglang_request.top_p, None);
        assert_eq!(sglang_request.presence_penalty, None);
        assert_eq!(sglang_request.frequency_penalty, None);
    }

    #[test]
    fn test_sglang_response_with_metadata_try_into() {
        let valid_response = OpenAIResponse {
            choices: vec![OpenAIResponseChoice {
                index: 0,
                message: OpenAIResponseMessage {
                    content: Some("Hello, world!".to_string()),
                    tool_calls: None,
                },
                finish_reason: OpenAIFinishReason::Stop,
            }],
            usage: OpenAIUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            },
        };
        let generic_request = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec!["test_user".to_string().into()],
            }],
            system: None,
            temperature: Some(0.5),
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: Some(100),
            stream: false,
            seed: Some(69),
            json_mode: ModelInferenceRequestJsonMode::Off,
            tool_config: None,
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        let sglang_response_with_metadata = SGLangResponseWithMetadata {
            response: valid_response,
            raw_response: "test_response".to_string(),
            latency: Latency::NonStreaming {
                response_time: Duration::from_secs(0),
            },
            raw_request: serde_json::to_string(
                &SGLangRequest::new("test-model", &generic_request).unwrap(),
            )
            .unwrap(),
            generic_request: &generic_request,
        };
        let inference_response: ProviderInferenceResponse =
            sglang_response_with_metadata.try_into().unwrap();

        assert_eq!(inference_response.output.len(), 1);
        assert_eq!(
            inference_response.output[0],
            "Hello, world!".to_string().into()
        );
        assert_eq!(inference_response.raw_response, "test_response");
        assert_eq!(inference_response.usage.input_tokens, 10);
        assert_eq!(inference_response.usage.output_tokens, 20);
        assert_eq!(inference_response.finish_reason, Some(FinishReason::Stop));
        assert_eq!(
            inference_response.latency,
            Latency::NonStreaming {
                response_time: Duration::from_secs(0)
            }
        );
    }

    #[test]
    #[traced_test]
    fn test_sglang_provider_new_api_base_check() {
        let model_name = "test-model".to_string();
        let api_key_location = Some(CredentialLocation::None);

        // Valid cases (should not warn)
        let _ = SGLangProvider::new(
            model_name.clone(),
            Url::parse("http://localhost:1234/v1/").unwrap(),
            api_key_location.clone(),
        )
        .unwrap();

        let _ = SGLangProvider::new(
            model_name.clone(),
            Url::parse("http://localhost:1234/v1").unwrap(),
            api_key_location.clone(),
        )
        .unwrap();

        // Invalid cases (should warn)
        let invalid_url_1 = Url::parse("http://localhost:1234/chat/completions").unwrap();
        let _ = SGLangProvider::new(
            model_name.clone(),
            invalid_url_1.clone(),
            api_key_location.clone(),
        )
        .unwrap();
        assert!(logs_contain("automatically appends `/chat/completions`"));
        assert!(logs_contain(invalid_url_1.as_ref()));

        let invalid_url_2 = Url::parse("http://localhost:1234/v1/chat/completions/").unwrap();
        let _ = SGLangProvider::new(
            model_name.clone(),
            invalid_url_2.clone(),
            api_key_location.clone(),
        )
        .unwrap();
        assert!(logs_contain("automatically appends `/chat/completions`"));
        assert!(logs_contain(invalid_url_2.as_ref()));
    }

    #[test]
    fn test_sglang_tools() {
        let model_name = PROVIDER_TYPE.to_string();
        let request_with_tools = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec!["What's the weather?".to_string().into()],
            }],
            system: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Off,
            tool_config: Some(Cow::Borrowed(&MULTI_TOOL_CONFIG)),
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };

        let sglang_request = SGLangRequest::new(&model_name, &request_with_tools).unwrap();

        let tools = sglang_request.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].function.name, WEATHER_TOOL.name());
        assert_eq!(tools[0].function.parameters, WEATHER_TOOL.parameters());
        assert_eq!(tools[1].function.name, QUERY_TOOL.name());
        assert_eq!(tools[1].function.parameters, QUERY_TOOL.parameters());
        let tool_choice = sglang_request.tool_choice.unwrap();
        assert_eq!(
            tool_choice,
            OpenAIToolChoice::String(OpenAIToolChoiceString::Required)
        );
        let parallel_tool_calls = sglang_request.parallel_tool_calls.unwrap();
        assert!(parallel_tool_calls);
        let tool_config = ToolCallConfig {
            tools_available: vec![],
            tool_choice: ToolChoice::Required,
            parallel_tool_calls: Some(true),
        };

        // Test no tools but a tool choice and make sure tool choice output is None
        let request_without_tools = ModelInferenceRequest {
            inference_id: Uuid::now_v7(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec!["What's the weather?".to_string().into()],
            }],
            system: None,
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stream: false,
            json_mode: ModelInferenceRequestJsonMode::Off,
            tool_config: Some(Cow::Borrowed(&tool_config)),
            function_type: FunctionType::Chat,
            output_schema: None,
            extra_body: Default::default(),
            ..Default::default()
        };
        let sglang_request = SGLangRequest::new(&model_name, &request_without_tools).unwrap();
        assert!(sglang_request.tools.is_none());
        assert!(sglang_request.tool_choice.is_none());
        assert!(sglang_request.parallel_tool_calls.is_none());
    }
}
