//! TODO: Replace this module doc with a description of what your adapter controls.
//!
//! IPC adapter template for HomeCmdr.
//!
//! IPC adapters are native Rust binaries launched and supervised by the
//! HomeCmdr API process.  The host injects three environment variables:
//!
//!   - `HOMECMDR_API_URL`        — base URL of the running HomeCmdr API
//!   - `HOMECMDR_API_TOKEN`      — bearer token for authenticating API calls
//!   - `HOMECMDR_ADAPTER_CONFIG` — JSON-encoded adapter config block from
//!                                  [adapters.<name>] in the HomeCmdr config
//!
//! The adapter has two responsibilities:
//!
//!   1. **Ingest** — push device state to `POST /ingest/devices` whenever the
//!      external device or service reports a change (or on a polling timer).
//!
//!   2. **Commands** — listen on the `GET /events` WebSocket for
//!      `device.command_dispatched` events and translate them into calls to
//!      the external device or service.
//!
//! Build:
//!   cargo build --release
//!   # Output: target/release/my-plugin-adapter
//!
//! Deploy:
//!   Copy the binary into the path HomeCmdr expects, or reference it in
//!   my_plugin.plugin.toml under `binary`.

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

// ── Config ────────────────────────────────────────────────────────────────────
// Parsed once at startup from `HOMECMDR_ADAPTER_CONFIG` (JSON).
// Fields here should match the [[config.fields]] entries in my_plugin.plugin.toml.

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    // TODO: add config fields to match my_plugin.plugin.toml.
    base_url: String,
}

fn default_true() -> bool { true }

// ── HomeCmdr ingest API types ─────────────────────────────────────────────────
// These match the JSON shapes expected by `POST /ingest/devices`.

#[derive(Serialize)]
struct IngestRequest<'a> {
    /// Must match the plugin `name` in my_plugin.plugin.toml.
    adapter: &'a str,
    devices: Vec<IngestDevice>,
}

#[derive(Serialize)]
struct IngestDevice {
    /// Stable device identifier fragment (without the adapter name prefix).
    vendor_id: String,
    /// "light" | "switch" | "sensor" | "virtual"
    kind: String,
    /// JSON map of capability key → value.
    attributes: Value,
    /// Optional opaque vendor metadata stored under `metadata.vendor_specific`.
    metadata: Value,
}

// ── HomeCmdr WebSocket event types ───────────────────────────────────────────
// Only `device.command_dispatched` events need to be handled.

#[derive(Deserialize)]
struct WsEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    command: Option<DeviceCommand>,
}

#[derive(Deserialize)]
struct DeviceCommand {
    capability: String,
    action: String,
    #[serde(default)]
    value: Option<Value>,
}

// ── Shared state ──────────────────────────────────────────────────────────────
// Anything both tasks need to read or mutate.
// TODO: replace or extend with your adapter's actual shared state.

#[derive(Default)]
struct SharedState {
    // Example: track whether the last poll succeeded so commands can be
    // skipped when the device is unreachable.
    last_poll_ok: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Structured logging to stderr so it doesn't interfere with stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Read the three env vars injected by the HomeCmdr host.
    let api_url   = std::env::var("HOMECMDR_API_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3001".to_string());
    let api_token = std::env::var("HOMECMDR_API_TOKEN").unwrap_or_default();
    let config_json = std::env::var("HOMECMDR_ADAPTER_CONFIG")
        .unwrap_or_else(|_| "{}".to_string());

    let config: Config = serde_json::from_str(&config_json).unwrap_or_else(|e| {
        tracing::warn!("my_plugin: bad config ({e}); using defaults");
        // If your config has required fields with no defaults, bail out here.
        serde_json::from_str("{}").expect("empty config should parse")
    });

    if !config.enabled {
        tracing::info!("my_plugin adapter disabled by config — exiting");
        return;
    }

    let state = Arc::new(Mutex::new(SharedState::default()));

    // Run the poll loop and the WebSocket command listener concurrently.
    tokio::join!(
        run_poll_loop(config.base_url.clone(), api_url.clone(), api_token.clone(), Arc::clone(&state)),
        run_websocket(api_url, api_token, state),
    );
}

// ── Poll loop ─────────────────────────────────────────────────────────────────
// Periodically fetches state from the external device/service and pushes it
// to the HomeCmdr API via POST /ingest/devices.

/// Poll the device on a fixed interval and push updates to HomeCmdr.
///
/// TODO: replace the hard-coded 30 s interval with a config field if needed.
async fn run_poll_loop(
    base_url: String,
    api_url: String,
    api_token: String,
    state: Arc<Mutex<SharedState>>,
) {
    let http = reqwest::Client::new();
    let mut interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        interval.tick().await;

        match fetch_device_state(&http, &base_url).await {
            Ok(devices) => {
                {
                    let mut st = state.lock().await;
                    st.last_poll_ok = true;
                }
                post_ingest(&http, &api_url, &api_token, devices).await;
            }
            Err(e) => {
                tracing::warn!("my_plugin: poll failed: {e}");
                let mut st = state.lock().await;
                st.last_poll_ok = false;
            }
        }
    }
}

/// Fetch the current state of the device/service and return it as ingest
/// records ready to POST to HomeCmdr.
///
/// TODO: replace the example HTTP call and attribute mapping with the real
///       logic for your device/service.
async fn fetch_device_state(
    http: &reqwest::Client,
    base_url: &str,
) -> anyhow::Result<Vec<IngestDevice>> {
    // TODO: call the real API endpoint for your device.
    let url = format!("{}/status", base_url.trim_end_matches('/'));
    let resp: serde_json::Value = http.get(&url).send().await?.json().await?;

    // TODO: map the response fields to HomeCmdr canonical attribute keys.
    let on   = resp["on"].as_bool().unwrap_or(false);

    let attributes = serde_json::json!({
        "state": if on { "on" } else { "off" },
        // TODO: add more canonical attributes here, e.g.:
        // "brightness": resp["brightness"].as_i64().unwrap_or(0),
        // "custom.my_plugin.signal_strength": resp["rssi"],
    });

    Ok(vec![IngestDevice {
        // TODO: use a stable identifier for this device.
        vendor_id:  "device_0".to_string(),
        // TODO: "light" | "switch" | "sensor" | "virtual"
        kind:       "switch".to_string(),
        attributes,
        metadata:   serde_json::json!({}),
    }])
}

// ── HTTP ingest helper ────────────────────────────────────────────────────────

/// POST a batch of device updates to `POST /ingest/devices`.
async fn post_ingest(
    http: &reqwest::Client,
    api_url: &str,
    api_token: &str,
    devices: Vec<IngestDevice>,
) {
    let count = devices.len();
    // TODO: update "my_plugin" to match your plugin name.
    let req = IngestRequest { adapter: "my_plugin", devices };
    let url = format!("{}/ingest/devices", api_url.trim_end_matches('/'));

    match http.post(&url).bearer_auth(api_token).json(&req).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::debug!("my_plugin: ingest OK ({count} devices)");
        }
        Ok(r) => {
            tracing::warn!("my_plugin: ingest returned {}", r.status());
        }
        Err(e) => {
            tracing::warn!("my_plugin: ingest HTTP error: {e}");
        }
    }
}

// ── WebSocket command listener ────────────────────────────────────────────────
// Connects to `/events` and handles `device.command_dispatched` events.
// Reconnects automatically with exponential back-off.

/// Maintain the WebSocket connection and dispatch incoming commands.
///
/// Never returns unless the process exits.
async fn run_websocket(
    api_url: String,
    api_token: String,
    state: Arc<Mutex<SharedState>>,
) {
    let ws_url = {
        let base = api_url.trim_end_matches('/');
        let base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        format!("{base}/events")
    };

    let mut backoff = Duration::from_secs(1);
    loop {
        match ws_session(&ws_url, &api_token, &state).await {
            Ok(()) => {
                tracing::info!("my_plugin: WebSocket session ended cleanly");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!("my_plugin: WebSocket error: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Handle one WebSocket connection lifetime.
async fn ws_session(
    ws_url: &str,
    api_token: &str,
    _state: &Arc<Mutex<SharedState>>,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};

    let mut req = ws_url.into_client_request()?;
    req.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_token}"))?,
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    tracing::info!("my_plugin: WebSocket connected");

    while let Some(msg) = ws.next().await {
        use tokio_tungstenite::tungstenite::Message;
        let text = match msg? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

        let event: WsEvent = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if event.event_type == "device.command_dispatched" {
            handle_command(event).await;
        }
    }

    Ok(())
}

// ── Command handler ───────────────────────────────────────────────────────────

/// Translate a `device.command_dispatched` event into a call to the external
/// device or service.
///
/// TODO: replace the example match arms with the real command logic.
async fn handle_command(event: WsEvent) {
    // Only handle devices that belong to this adapter.
    let device_id = match event.id.as_deref() {
        Some(id) if id.starts_with("my_plugin:") => id.to_string(),
        _ => return,
    };

    let cmd = match event.command {
        Some(c) => c,
        None => return,
    };

    tracing::debug!(
        "my_plugin: command {}/{} for '{device_id}'",
        cmd.capability, cmd.action
    );

    // TODO: map HomeCmdr capability/action pairs to calls on your device.
    match (cmd.capability.as_str(), cmd.action.as_str()) {
        ("state", "on") => {
            // TODO: send the real "on" command to your device.
            tracing::info!("my_plugin: turning on {device_id}");
        }
        ("state", "off") => {
            // TODO: send the real "off" command to your device.
            tracing::info!("my_plugin: turning off {device_id}");
        }
        ("state", "toggle") => {
            // TODO: implement toggle.
            tracing::info!("my_plugin: toggling {device_id}");
        }
        // TODO: add more capability/action arms here.
        _ => {
            tracing::debug!("my_plugin: unhandled command {}/{}", cmd.capability, cmd.action);
        }
    }
}
