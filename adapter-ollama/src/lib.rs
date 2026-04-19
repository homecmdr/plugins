use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use homecmdr_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use homecmdr_core::bus::EventBus;
use homecmdr_core::config::AdapterConfig;
use homecmdr_core::http::{external_http_client, send_with_retry};
use homecmdr_core::invoke::{InvokeRequest, InvokeResponse};
use homecmdr_core::model::AttributeValue;
use homecmdr_core::registry::DeviceRegistry;

const ADAPTER_NAME: &str = "ollama";
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaConfig {
    pub enabled: bool,
    pub model: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

pub struct OllamaFactory;

static OLLAMA_FACTORY: OllamaFactory = OllamaFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &OLLAMA_FACTORY,
    }
}

pub struct OllamaAdapter {
    client: Client,
    config: OllamaConfig,
}

impl OllamaAdapter {
    pub fn new(config: OllamaConfig) -> Result<Self> {
        Ok(Self {
            client: external_http_client()?,
            config,
        })
    }

    #[cfg(test)]
    fn with_base_url(config: OllamaConfig, base_url: impl Into<String>) -> Result<Self> {
        let mut config = config;
        config.base_url = base_url.into();
        Self::new(config)
    }

    async fn invoke_target(&self, request: InvokeRequest) -> Result<Option<InvokeResponse>> {
        let Some((adapter_name, target)) = request.target.split_once(':') else {
            return Ok(None);
        };
        if adapter_name != ADAPTER_NAME {
            return Ok(None);
        }

        let value = match target {
            "generate" => {
                let payload = parse_generate_payload(request.payload, &self.config.model)?;
                let response = self.generate(payload).await?;
                generate_response_to_attribute(response)
            }
            "vision" => {
                let payload = parse_vision_payload(request.payload, &self.config.model)?;
                let response = self.generate(payload).await?;
                vision_response_to_attribute(response)
            }
            "chat" => {
                let payload = parse_chat_payload(request.payload, &self.config.model)?;
                let response = self.chat(payload).await?;
                chat_response_to_attribute(response)
            }
            "embeddings" => {
                let payload = parse_embeddings_payload(request.payload, &self.config.model)?;
                let response = self.embeddings(payload).await?;
                embeddings_response_to_attribute(response)
            }
            "tags" => {
                let response = self.tags().await?;
                tags_response_to_attribute(response)
            }
            "ps" => {
                let response = self.running_models().await?;
                tags_response_to_attribute(response)
            }
            "show" => {
                let payload = parse_show_payload(request.payload, &self.config.model)?;
                let response = self.show(payload).await?;
                json_value_to_attribute(
                    serde_json::to_value(response).context("failed to serialize show response")?,
                )
            }
            "version" => {
                let response = self.version().await?;
                AttributeValue::Object(HashMap::from([(
                    "version".to_string(),
                    AttributeValue::Text(response.version),
                )]))
            }
            _ => return Ok(None),
        };

        Ok(Some(InvokeResponse { value }))
    }

    async fn generate(&self, payload: GeneratePayload) -> Result<OllamaGenerateResponse> {
        send_with_retry(
            self.client
                .post(format!("{}/api/generate", self.base_url()))
                .json(&GenerateRequest {
                    model: payload.model,
                    prompt: payload.prompt,
                    suffix: payload.suffix,
                    system: payload.system,
                    template: payload.template,
                    format: payload.format,
                    options: payload.options,
                    keep_alive: payload.keep_alive,
                    raw: payload.raw,
                    images: payload.images,
                    stream: false,
                }),
            "Ollama generate",
        )
        .await?
        .json::<OllamaGenerateResponse>()
        .await
        .context("failed to parse Ollama generate response")
    }

    async fn chat(&self, payload: ChatPayload) -> Result<OllamaChatResponse> {
        send_with_retry(
            self.client
                .post(format!("{}/api/chat", self.base_url()))
                .json(&ChatRequest {
                    model: payload.model,
                    messages: payload.messages,
                    format: payload.format,
                    options: payload.options,
                    keep_alive: payload.keep_alive,
                    tools: payload.tools,
                    stream: false,
                }),
            "Ollama chat",
        )
        .await?
        .json::<OllamaChatResponse>()
        .await
        .context("failed to parse Ollama chat response")
    }

    async fn embeddings(&self, payload: EmbeddingsPayload) -> Result<OllamaEmbeddingsResponse> {
        send_with_retry(
            self.client
                .post(format!("{}/api/embed", self.base_url()))
                .json(&EmbedRequest {
                    model: payload.model,
                    input: payload.input,
                    keep_alive: payload.keep_alive,
                    options: payload.options,
                    truncate: payload.truncate,
                }),
            "Ollama embeddings",
        )
        .await?
        .json::<OllamaEmbeddingsResponse>()
        .await
        .context("failed to parse Ollama embeddings response")
    }

    async fn tags(&self) -> Result<OllamaModelsResponse> {
        send_with_retry(
            self.client.get(format!("{}/api/tags", self.base_url())),
            "Ollama tags",
        )
        .await?
        .json::<OllamaModelsResponse>()
        .await
        .context("failed to parse Ollama tags response")
    }

    async fn running_models(&self) -> Result<OllamaModelsResponse> {
        send_with_retry(
            self.client.get(format!("{}/api/ps", self.base_url())),
            "Ollama running models",
        )
        .await?
        .json::<OllamaModelsResponse>()
        .await
        .context("failed to parse Ollama running models response")
    }

    async fn show(&self, payload: ShowPayload) -> Result<OllamaShowResponse> {
        send_with_retry(
            self.client
                .post(format!("{}/api/show", self.base_url()))
                .json(&ShowRequest {
                    model: payload.model,
                    verbose: payload.verbose,
                }),
            "Ollama show",
        )
        .await?
        .json::<OllamaShowResponse>()
        .await
        .context("failed to parse Ollama show response")
    }

    async fn version(&self) -> Result<OllamaVersionResponse> {
        send_with_retry(
            self.client.get(format!("{}/api/version", self.base_url())),
            "Ollama version",
        )
        .await?
        .json::<OllamaVersionResponse>()
        .await
        .context("failed to parse Ollama version response")
    }

    fn base_url(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }
}

impl AdapterFactory for OllamaFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: OllamaConfig =
            serde_json::from_value(config).context("failed to parse ollama adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(OllamaAdapter::new(config)?)))
    }
}

#[async_trait]
impl Adapter for OllamaAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, _registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(homecmdr_core::event::Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        std::future::pending::<()>().await;
        Ok(())
    }

    async fn invoke(
        &self,
        request: InvokeRequest,
        _registry: DeviceRegistry,
    ) -> Result<Option<InvokeResponse>> {
        self.invoke_target(request).await
    }
}

#[derive(Debug, Clone)]
struct GeneratePayload {
    model: String,
    prompt: String,
    suffix: Option<String>,
    system: Option<String>,
    template: Option<String>,
    format: Option<JsonValue>,
    options: Option<JsonValue>,
    keep_alive: Option<JsonValue>,
    raw: Option<bool>,
    images: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct ChatPayload {
    model: String,
    messages: Vec<ChatMessageRequest>,
    format: Option<JsonValue>,
    options: Option<JsonValue>,
    keep_alive: Option<JsonValue>,
    tools: Option<JsonValue>,
}

#[derive(Debug, Clone)]
struct EmbeddingsPayload {
    model: String,
    input: JsonValue,
    keep_alive: Option<JsonValue>,
    options: Option<JsonValue>,
    truncate: Option<bool>,
}

#[derive(Debug, Clone)]
struct ShowPayload {
    model: String,
    verbose: Option<bool>,
}

#[derive(Debug, Serialize)]
struct GenerateRequest {
    model: String,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessageRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<JsonValue>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct EmbedRequest {
    model: String,
    input: JsonValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_alive: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncate: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ShowRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    verbose: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct ChatMessageRequest {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    total_duration: Option<i64>,
    #[serde(default)]
    load_duration: Option<i64>,
    #[serde(default)]
    prompt_eval_count: Option<i64>,
    #[serde(default)]
    prompt_eval_duration: Option<i64>,
    #[serde(default)]
    eval_count: Option<i64>,
    #[serde(default)]
    eval_duration: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    total_duration: Option<i64>,
    #[serde(default)]
    load_duration: Option<i64>,
    #[serde(default)]
    prompt_eval_count: Option<i64>,
    #[serde(default)]
    prompt_eval_duration: Option<i64>,
    #[serde(default)]
    eval_count: Option<i64>,
    #[serde(default)]
    eval_duration: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OllamaMessage {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingsResponse {
    embeddings: Vec<Vec<f64>>,
    #[serde(default)]
    total_duration: Option<i64>,
    #[serde(default)]
    load_duration: Option<i64>,
    #[serde(default)]
    prompt_eval_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelsResponse {
    models: Vec<JsonValue>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OllamaShowResponse {
    #[serde(flatten)]
    body: HashMap<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
struct OllamaVersionResponse {
    version: String,
}

fn validate_config(config: &OllamaConfig) -> Result<()> {
    if config.model.trim().is_empty() {
        bail!("adapters.ollama.model must not be empty");
    }

    if config.base_url.trim().is_empty() {
        bail!("adapters.ollama.base_url must not be empty");
    }

    Ok(())
}

fn parse_generate_payload(payload: AttributeValue, default_model: &str) -> Result<GeneratePayload> {
    let fields = expect_object(payload, "ollama generate payload must be an object")?;

    Ok(GeneratePayload {
        model: model_from_fields(&fields, default_model)?,
        prompt: require_text(&fields, "prompt")?,
        suffix: optional_text(&fields, "suffix")?,
        system: optional_text(&fields, "system")?,
        template: optional_text(&fields, "template")?,
        format: optional_json(&fields, "format")?,
        options: optional_json(&fields, "options")?,
        keep_alive: optional_json(&fields, "keep_alive")?,
        raw: optional_bool(&fields, "raw")?,
        images: optional_base64_images(&fields, "images")?,
    })
}

fn parse_vision_payload(payload: AttributeValue, default_model: &str) -> Result<GeneratePayload> {
    let fields = expect_object(payload, "ollama vision payload must be an object")?;

    Ok(GeneratePayload {
        model: model_from_fields(&fields, default_model)?,
        prompt: require_text(&fields, "prompt")?,
        suffix: None,
        system: optional_text(&fields, "system")?,
        template: optional_text(&fields, "template")?,
        format: optional_json(&fields, "format")?,
        options: optional_json(&fields, "options")?,
        keep_alive: optional_json(&fields, "keep_alive")?,
        raw: optional_bool(&fields, "raw")?,
        images: Some(required_base64_images(&fields, "image_base64")?),
    })
}

fn parse_chat_payload(payload: AttributeValue, default_model: &str) -> Result<ChatPayload> {
    let fields = expect_object(payload, "ollama chat payload must be an object")?;
    let messages = match fields.get("messages") {
        Some(AttributeValue::Array(messages)) => messages
            .iter()
            .cloned()
            .map(parse_chat_message)
            .collect::<Result<Vec<_>>>()?,
        Some(_) => bail!("ollama chat payload field 'messages' must be a list"),
        None => bail!("ollama chat payload field 'messages' is required"),
    };

    Ok(ChatPayload {
        model: model_from_fields(&fields, default_model)?,
        messages,
        format: optional_json(&fields, "format")?,
        options: optional_json(&fields, "options")?,
        keep_alive: optional_json(&fields, "keep_alive")?,
        tools: optional_json(&fields, "tools")?,
    })
}

fn parse_embeddings_payload(
    payload: AttributeValue,
    default_model: &str,
) -> Result<EmbeddingsPayload> {
    let fields = expect_object(payload, "ollama embeddings payload must be an object")?;
    let input = fields
        .get("input")
        .cloned()
        .context("ollama embeddings payload field 'input' is required")
        .and_then(attribute_to_json_value)?;

    Ok(EmbeddingsPayload {
        model: model_from_fields(&fields, default_model)?,
        input,
        keep_alive: optional_json(&fields, "keep_alive")?,
        options: optional_json(&fields, "options")?,
        truncate: optional_bool(&fields, "truncate")?,
    })
}

fn parse_show_payload(payload: AttributeValue, default_model: &str) -> Result<ShowPayload> {
    let fields = expect_object(payload, "ollama show payload must be an object")?;

    Ok(ShowPayload {
        model: model_from_fields(&fields, default_model)?,
        verbose: optional_bool(&fields, "verbose")?,
    })
}

fn parse_chat_message(value: AttributeValue) -> Result<ChatMessageRequest> {
    let fields = expect_object(value, "ollama chat messages must be objects")?;

    Ok(ChatMessageRequest {
        role: require_text(&fields, "role")?,
        content: optional_text(&fields, "content")?,
        images: optional_base64_images(&fields, "images")?,
        tool_calls: optional_json(&fields, "tool_calls")?,
        tool_name: optional_text(&fields, "tool_name")?,
    })
}

fn expect_object(value: AttributeValue, message: &str) -> Result<HashMap<String, AttributeValue>> {
    let AttributeValue::Object(fields) = value else {
        bail!("{message}");
    };

    Ok(fields)
}

fn model_from_fields(
    fields: &HashMap<String, AttributeValue>,
    default_model: &str,
) -> Result<String> {
    Ok(optional_text(fields, "model")?.unwrap_or_else(|| default_model.to_string()))
}

fn require_text(fields: &HashMap<String, AttributeValue>, key: &str) -> Result<String> {
    match fields.get(key) {
        Some(AttributeValue::Text(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(AttributeValue::Text(_)) => bail!("ollama payload field '{key}' must not be empty"),
        Some(_) => bail!("ollama payload field '{key}' must be a string"),
        None => bail!("ollama payload field '{key}' is required"),
    }
}

fn optional_text(fields: &HashMap<String, AttributeValue>, key: &str) -> Result<Option<String>> {
    match fields.get(key) {
        Some(AttributeValue::Text(value)) if value.trim().is_empty() => Ok(None),
        Some(AttributeValue::Text(value)) => Ok(Some(value.clone())),
        Some(_) => bail!("ollama payload field '{key}' must be a string"),
        None => Ok(None),
    }
}

fn optional_bool(fields: &HashMap<String, AttributeValue>, key: &str) -> Result<Option<bool>> {
    match fields.get(key) {
        Some(AttributeValue::Bool(value)) => Ok(Some(*value)),
        Some(_) => bail!("ollama payload field '{key}' must be a boolean"),
        None => Ok(None),
    }
}

fn optional_json(fields: &HashMap<String, AttributeValue>, key: &str) -> Result<Option<JsonValue>> {
    match fields.get(key) {
        Some(value) => Ok(Some(attribute_to_json_value(value.clone())?)),
        None => Ok(None),
    }
}

fn optional_base64_images(
    fields: &HashMap<String, AttributeValue>,
    key: &str,
) -> Result<Option<Vec<String>>> {
    match fields.get(key) {
        Some(value) => Ok(Some(base64_images_from_value(value, key)?)),
        None => Ok(None),
    }
}

fn required_base64_images(
    fields: &HashMap<String, AttributeValue>,
    key: &str,
) -> Result<Vec<String>> {
    let value = fields
        .get(key)
        .with_context(|| format!("ollama payload field '{key}' is required"))?;
    base64_images_from_value(value, key)
}

fn base64_images_from_value(value: &AttributeValue, key: &str) -> Result<Vec<String>> {
    let values = match value {
        AttributeValue::Text(value) => vec![value.clone()],
        AttributeValue::Array(values) => values
            .iter()
            .map(|value| match value {
                AttributeValue::Text(text) => Ok(text.clone()),
                _ => bail!("ollama payload field '{key}' image list items must be strings"),
            })
            .collect::<Result<Vec<_>>>()?,
        _ => bail!("ollama payload field '{key}' must be a string or list of strings"),
    };

    if values.is_empty() {
        bail!("ollama payload field '{key}' must not be empty");
    }

    values
        .iter()
        .map(|value| normalize_image_base64(value))
        .collect::<Result<Vec<_>>>()
}

fn normalize_image_base64(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let encoded = trimmed
        .split_once(',')
        .map(|(_, encoded)| encoded)
        .unwrap_or(trimmed);

    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("ollama images must be valid base64")?;

    Ok(encoded.to_string())
}

fn generate_response_to_attribute(response: OllamaGenerateResponse) -> AttributeValue {
    let trimmed = response.response.trim().to_string();

    AttributeValue::Object(HashMap::from([
        (
            "response".to_string(),
            AttributeValue::Text(trimmed.clone()),
        ),
        (
            "boolean".to_string(),
            match trimmed.to_ascii_lowercase().as_str() {
                "true" => AttributeValue::Bool(true),
                "false" => AttributeValue::Bool(false),
                _ => AttributeValue::Null,
            },
        ),
        ("done".to_string(), AttributeValue::Bool(response.done)),
        optional_i64_field("total_duration", response.total_duration),
        optional_i64_field("load_duration", response.load_duration),
        optional_i64_field("prompt_eval_count", response.prompt_eval_count),
        optional_i64_field("prompt_eval_duration", response.prompt_eval_duration),
        optional_i64_field("eval_count", response.eval_count),
        optional_i64_field("eval_duration", response.eval_duration),
        optional_text_field("done_reason", response.done_reason),
    ]))
}

fn vision_response_to_attribute(response: OllamaGenerateResponse) -> AttributeValue {
    generate_response_to_attribute(response)
}

fn chat_response_to_attribute(response: OllamaChatResponse) -> AttributeValue {
    let OllamaChatResponse {
        message,
        done,
        done_reason,
        total_duration,
        load_duration,
        prompt_eval_count,
        prompt_eval_duration,
        eval_count,
        eval_duration,
    } = response;

    let mut fields = HashMap::from([
        (
            "message".to_string(),
            AttributeValue::Object(HashMap::from([
                ("role".to_string(), AttributeValue::Text(message.role)),
                (
                    "content".to_string(),
                    message
                        .content
                        .map(AttributeValue::Text)
                        .unwrap_or(AttributeValue::Null),
                ),
                (
                    "tool_calls".to_string(),
                    match message.tool_calls {
                        Some(value) => json_value_to_attribute(value),
                        None => AttributeValue::Null,
                    },
                ),
            ])),
        ),
        ("done".to_string(), AttributeValue::Bool(done)),
    ]);

    insert_optional_i64(&mut fields, "total_duration", total_duration);
    insert_optional_i64(&mut fields, "load_duration", load_duration);
    insert_optional_i64(&mut fields, "prompt_eval_count", prompt_eval_count);
    insert_optional_i64(&mut fields, "prompt_eval_duration", prompt_eval_duration);
    insert_optional_i64(&mut fields, "eval_count", eval_count);
    insert_optional_i64(&mut fields, "eval_duration", eval_duration);
    insert_optional_text(&mut fields, "done_reason", done_reason);

    AttributeValue::Object(fields)
}

fn embeddings_response_to_attribute(response: OllamaEmbeddingsResponse) -> AttributeValue {
    let mut fields = HashMap::from([(
        "embeddings".to_string(),
        AttributeValue::Array(
            response
                .embeddings
                .into_iter()
                .map(|embedding| {
                    AttributeValue::Array(
                        embedding
                            .into_iter()
                            .map(AttributeValue::Float)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
        ),
    )]);

    insert_optional_i64(&mut fields, "total_duration", response.total_duration);
    insert_optional_i64(&mut fields, "load_duration", response.load_duration);
    insert_optional_i64(&mut fields, "prompt_eval_count", response.prompt_eval_count);

    AttributeValue::Object(fields)
}

fn tags_response_to_attribute(response: OllamaModelsResponse) -> AttributeValue {
    AttributeValue::Object(HashMap::from([(
        "models".to_string(),
        AttributeValue::Array(
            response
                .models
                .into_iter()
                .map(json_value_to_attribute)
                .collect::<Vec<_>>(),
        ),
    )]))
}

fn optional_i64_field(key: &str, value: Option<i64>) -> (String, AttributeValue) {
    (
        key.to_string(),
        value
            .map(AttributeValue::Integer)
            .unwrap_or(AttributeValue::Null),
    )
}

fn optional_text_field(key: &str, value: Option<String>) -> (String, AttributeValue) {
    (
        key.to_string(),
        value
            .map(AttributeValue::Text)
            .unwrap_or(AttributeValue::Null),
    )
}

fn insert_optional_i64(
    fields: &mut HashMap<String, AttributeValue>,
    key: &str,
    value: Option<i64>,
) {
    fields.insert(
        key.to_string(),
        value
            .map(AttributeValue::Integer)
            .unwrap_or(AttributeValue::Null),
    );
}

fn insert_optional_text(
    fields: &mut HashMap<String, AttributeValue>,
    key: &str,
    value: Option<String>,
) {
    fields.insert(
        key.to_string(),
        value
            .map(AttributeValue::Text)
            .unwrap_or(AttributeValue::Null),
    );
}

fn attribute_to_json_value(value: AttributeValue) -> Result<JsonValue> {
    Ok(match value {
        AttributeValue::Null => JsonValue::Null,
        AttributeValue::Bool(value) => JsonValue::Bool(value),
        AttributeValue::Integer(value) => JsonValue::Number(value.into()),
        AttributeValue::Float(value) => serde_json::Number::from_f64(value)
            .map(JsonValue::Number)
            .context("floating point value must be finite")?,
        AttributeValue::Text(value) => JsonValue::String(value),
        AttributeValue::Array(values) => JsonValue::Array(
            values
                .into_iter()
                .map(attribute_to_json_value)
                .collect::<Result<Vec<_>>>()?,
        ),
        AttributeValue::Object(fields) => JsonValue::Object(
            fields
                .into_iter()
                .map(|(key, value)| Ok((key, attribute_to_json_value(value)?)))
                .collect::<Result<serde_json::Map<_, _>>>()?,
        ),
    })
}

fn json_value_to_attribute(value: JsonValue) -> AttributeValue {
    match value {
        JsonValue::Null => AttributeValue::Null,
        JsonValue::Bool(value) => AttributeValue::Bool(value),
        JsonValue::Number(value) => {
            if let Some(value) = value.as_i64() {
                AttributeValue::Integer(value)
            } else {
                AttributeValue::Float(value.as_f64().unwrap_or_default())
            }
        }
        JsonValue::String(value) => AttributeValue::Text(value),
        JsonValue::Array(values) => {
            AttributeValue::Array(values.into_iter().map(json_value_to_attribute).collect())
        }
        JsonValue::Object(fields) => AttributeValue::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, json_value_to_attribute(value)))
                .collect(),
        ),
    }
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use homecmdr_core::invoke::InvokeRequest;
    use homecmdr_core::model::AttributeValue;
    use homecmdr_core::registry::DeviceRegistry;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn invoke_vision_target_returns_boolean_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .and(body_json(serde_json::json!({
                "model": "llava",
                "prompt": "Reply only true or false.",
                "stream": false,
                "images": ["aGVsbG8="]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "response": "true",
                "done": true
            })))
            .mount(&server)
            .await;

        let adapter = test_adapter(&server);
        let response = invoke(
            &adapter,
            "ollama:vision",
            AttributeValue::Object(HashMap::from([
                (
                    "prompt".to_string(),
                    AttributeValue::Text("Reply only true or false.".to_string()),
                ),
                (
                    "image_base64".to_string(),
                    AttributeValue::Text("aGVsbG8=".to_string()),
                ),
            ])),
        )
        .await;

        assert_eq!(
            response,
            AttributeValue::Object(HashMap::from([
                (
                    "response".to_string(),
                    AttributeValue::Text("true".to_string()),
                ),
                ("boolean".to_string(), AttributeValue::Bool(true)),
                ("done".to_string(), AttributeValue::Bool(true)),
                ("total_duration".to_string(), AttributeValue::Null),
                ("load_duration".to_string(), AttributeValue::Null),
                ("prompt_eval_count".to_string(), AttributeValue::Null),
                ("prompt_eval_duration".to_string(), AttributeValue::Null),
                ("eval_count".to_string(), AttributeValue::Null),
                ("eval_duration".to_string(), AttributeValue::Null),
                ("done_reason".to_string(), AttributeValue::Null),
            ]))
        );
    }

    #[tokio::test]
    async fn invoke_chat_target_supports_message_lists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(serde_json::json!({
                "model": "llava",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": false
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"role": "assistant", "content": "hi there"},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let adapter = test_adapter(&server);
        let response = invoke(
            &adapter,
            "ollama:chat",
            AttributeValue::Object(HashMap::from([(
                "messages".to_string(),
                AttributeValue::Array(vec![AttributeValue::Object(HashMap::from([
                    ("role".to_string(), AttributeValue::Text("user".to_string())),
                    (
                        "content".to_string(),
                        AttributeValue::Text("hello".to_string()),
                    ),
                ]))]),
            )])),
        )
        .await;

        let AttributeValue::Object(fields) = response else {
            panic!("chat response should be an object");
        };
        assert_eq!(fields.get("done"), Some(&AttributeValue::Bool(true)));
    }

    #[tokio::test]
    async fn invoke_embeddings_target_returns_nested_arrays() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/embed"))
            .and(body_json(serde_json::json!({
                "model": "llava",
                "input": ["alpha", "beta"]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embeddings": [[0.1, 0.2], [0.3, 0.4]]
            })))
            .mount(&server)
            .await;

        let adapter = test_adapter(&server);
        let response = invoke(
            &adapter,
            "ollama:embeddings",
            AttributeValue::Object(HashMap::from([(
                "input".to_string(),
                AttributeValue::Array(vec![
                    AttributeValue::Text("alpha".to_string()),
                    AttributeValue::Text("beta".to_string()),
                ]),
            )])),
        )
        .await;

        let AttributeValue::Object(fields) = response else {
            panic!("embeddings response should be an object");
        };
        assert!(matches!(
            fields.get("embeddings"),
            Some(AttributeValue::Array(_))
        ));
    }

    #[tokio::test]
    async fn invoke_tags_target_lists_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "llava", "size": 123}]
            })))
            .mount(&server)
            .await;

        let adapter = test_adapter(&server);
        let response = invoke(&adapter, "ollama:tags", AttributeValue::Null).await;

        let AttributeValue::Object(fields) = response else {
            panic!("tags response should be an object");
        };
        assert!(matches!(
            fields.get("models"),
            Some(AttributeValue::Array(_))
        ));
    }

    fn test_adapter(server: &MockServer) -> OllamaAdapter {
        OllamaAdapter::with_base_url(
            OllamaConfig {
                enabled: true,
                model: "llava".to_string(),
                base_url: default_base_url(),
            },
            server.uri(),
        )
        .expect("adapter builds")
    }

    async fn invoke(
        adapter: &OllamaAdapter,
        target: &str,
        payload: AttributeValue,
    ) -> AttributeValue {
        adapter
            .invoke(
                InvokeRequest {
                    target: target.to_string(),
                    payload,
                },
                DeviceRegistry::new(homecmdr_core::bus::EventBus::new(4)),
            )
            .await
            .expect("invoke succeeds")
            .expect("target supported")
            .value
    }
}
