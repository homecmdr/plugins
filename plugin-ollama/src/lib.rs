//! Ollama WASM plugin for HomeCmdr.
//!
//! Polls a local Ollama instance for available models and service health.
//! Exposes the Ollama service as one virtual device and each available model
//! as an additional virtual device with metadata attributes.
//!
//! Capabilities:
//!   - service health (up/down via poll success/failure)
//!   - per-model metadata (name, size, family, quantization)
//!   - generate command: POST /api/generate; result is logged at info level
//!     (the WIT interface does not yet provide a way to return the generated
//!     text to the caller — that requires a richer command-result type)
//!
//! Build:
//!   cargo build --release
//!   cp target/wasm32-wasip2/release/plugin_ollama_wasm.wasm \
//!      <workspace>/config/plugins/ollama.wasm

wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::Deserialize;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Config — parsed once from config-json on init
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    /// HTTP base URL of the Ollama server (default: http://127.0.0.1:11434).
    #[serde(default = "default_base_url")]
    base_url: String,
}

fn default_true() -> bool {
    true
}
fn default_base_url() -> String {
    "http://127.0.0.1:11434".to_string()
}

// ---------------------------------------------------------------------------
// Runtime state — held for the lifetime of the plugin
// ---------------------------------------------------------------------------

struct OllamaState {
    base_url: String,
    /// Maps the sanitised vendor_id fragment back to the original Ollama model
    /// name.  Populated on each poll() so commands can resolve the real name.
    /// e.g. "llama2_latest" → "llama2:latest"
    model_map: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Ollama API response types
// ---------------------------------------------------------------------------

/// Response from `GET /api/tags`.
#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    /// Full model name including tag, e.g. "llama2:latest".
    name: String,
    /// Size on disk in bytes.
    #[serde(default)]
    size: u64,
    #[serde(default)]
    details: ModelDetails,
}

#[derive(Debug, Deserialize, Default)]
struct ModelDetails {
    #[serde(default)]
    format: String,
    #[serde(default)]
    family: String,
    #[serde(default)]
    parameter_size: String,
    #[serde(default)]
    quantization_level: String,
}

/// Partial shape of a `POST /api/generate` response (stream: false).
#[derive(Debug, Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    response: String,
}

// ---------------------------------------------------------------------------
// Plugin state
// ---------------------------------------------------------------------------

thread_local! {
    static STATE: std::cell::RefCell<Option<OllamaState>> = std::cell::RefCell::new(None);
}

// ---------------------------------------------------------------------------
// Guest implementation
// ---------------------------------------------------------------------------

struct OllamaPlugin;

impl Guest for OllamaPlugin {
    fn name() -> String {
        "ollama".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse ollama config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "ollama plugin is disabled by config");
            return Ok(());
        }

        host_log::log(
            "info",
            &format!("ollama initialised ({})", config.base_url),
        );
        STATE.with(|s| {
            *s.borrow_mut() = Some(OllamaState {
                base_url: config.base_url,
                model_map: HashMap::new(),
            });
        });
        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        let base_url = STATE
            .with(|s| s.borrow().as_ref().map(|st| st.base_url.clone()))
            .ok_or("ollama not initialised or disabled")?;

        let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
        let body = host_http::get(&url)
            .map_err(|e| format!("ollama HTTP GET failed: {e}"))?;

        let resp: TagsResponse =
            serde_json::from_str(&body).map_err(|e| format!("ollama parse error: {e}"))?;

        let model_count = resp.models.len() as i64;
        let mut updates = Vec::new();

        // Build the sanitised → original mapping and persist it in STATE.
        let mut new_model_map: HashMap<String, String> = HashMap::new();

        // One virtual device representing the Ollama service itself.
        updates.push(DeviceUpdate {
            vendor_id: "service".to_string(),
            kind: "virtual".to_string(),
            attributes_json: serde_json::json!({
                "power": true,
                "custom.ollama.model_count": model_count,
            })
            .to_string(),
        });

        // One virtual device per available model.
        for model in &resp.models {
            let safe_name = sanitise_model_name(&model.name);
            new_model_map.insert(safe_name.clone(), model.name.clone());

            updates.push(DeviceUpdate {
                vendor_id: format!("model:{}", safe_name),
                kind: "virtual".to_string(),
                attributes_json: serde_json::json!({
                    "custom.ollama.model_name":         model.name,
                    "custom.ollama.size_bytes":         model.size as i64,
                    "custom.ollama.format":             model.details.format,
                    "custom.ollama.family":             model.details.family,
                    "custom.ollama.parameter_size":     model.details.parameter_size,
                    "custom.ollama.quantization_level": model.details.quantization_level,
                })
                .to_string(),
            });
        }

        // Persist updated model map (we only borrow_mut after the HTTP work is done).
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.model_map = new_model_map;
            }
        });

        Ok(updates)
    }

    fn command(device_id: String, command_json: String) -> Result<CommandResult, String> {
        if !device_id.starts_with("ollama:") {
            return Ok(CommandResult {
                handled: false,
                error: None,
            });
        }

        let (base_url, model_map) = STATE
            .with(|s| {
                s.borrow().as_ref().map(|st| (st.base_url.clone(), st.model_map.clone()))
            })
            .ok_or("ollama not initialised or disabled")?;

        // device_id format: "ollama:model:<safe_name>"  or  "ollama:service"
        // Only model devices accept generate commands.
        let parts: Vec<&str> = device_id.splitn(3, ':').collect();
        if parts.get(1).copied() != Some("model") {
            return Ok(CommandResult { handled: false, error: None });
        }

        let safe_name = parts.get(2).copied().unwrap_or("");
        // Resolve original model name; fall back to the safe name if poll()
        // has not run yet (edge case: command before first poll).
        let original_name = model_map
            .get(safe_name)
            .cloned()
            .unwrap_or_else(|| safe_name.replace('_', ":"));

        let cmd: DeviceCommand = serde_json::from_str(&command_json)
            .map_err(|e| format!("failed to parse command: {e}"))?;

        match (cmd.capability.as_str(), cmd.action.as_str()) {
            ("generate", "run") => {
                let prompt = cmd
                    .value
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .ok_or("generate/run requires a string prompt in `value`")?;

                let request = serde_json::json!({
                    "model":  original_name,
                    "prompt": prompt,
                    "stream": false,
                })
                .to_string();

                let generate_url =
                    format!("{}/api/generate", base_url.trim_end_matches('/'));
                let response_body =
                    host_http::post(&generate_url, "application/json", &request)
                        .map_err(|e| {
                            return format!("ollama generate POST failed: {e}");
                        })?;

                // Parse and log the response — the current WIT does not provide
                // a channel to return generated text back to the automation.
                let generated = serde_json::from_str::<GenerateResponse>(&response_body)
                    .map(|r| r.response)
                    .unwrap_or_else(|_| response_body.clone());

                host_log::log(
                    "info",
                    &format!("ollama [{}] response: {}", original_name, generated),
                );

                Ok(CommandResult { handled: true, error: None })
            }
            _ => Ok(CommandResult { handled: false, error: None }),
        }
    }
}

export!(OllamaPlugin);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sanitise an Ollama model name for use as a vendor_id fragment.
/// "llama2:latest" → "llama2_latest"
fn sanitise_model_name(name: &str) -> String {
    name.replace(':', "_").replace('/', "_").replace(' ', "_")
}

#[derive(Deserialize)]
struct DeviceCommand {
    capability: String,
    action: String,
    value: Option<serde_json::Value>,
}
