//! Generic MQTT IPC adapter for HomeCmdr.
//!
//! Bridges any MQTT-capable device to HomeCmdr. Supports two message modes:
//!
//! - **JSON mode** (`state_topic`): the device publishes a full JSON state
//!   object on a single topic. Canonical attribute keys are extracted; any
//!   unknown keys fall through to `custom.mqtt.*`.
//!
//! - **Per-attribute mode** (`attribute_topics`): each attribute has its own
//!   MQTT topic carrying a scalar or simple JSON value. Useful for devices
//!   such as Tasmota sensors or custom ESP32 firmware.
//!
//! Both modes may be used simultaneously on the same device — the adapter
//! handles whichever messages arrive.
//!
//! IPC adapter env vars (injected by the HomeCmdr host):
//!   `HOMECMDR_API_URL`        — base URL of the running HomeCmdr API
//!   `HOMECMDR_API_TOKEN`      — bearer token for authenticating API calls
//!   `HOMECMDR_ADAPTER_CONFIG` — JSON-encoded `[adapters.mqtt]` config block
//!
//! Top-level config keys:
//!   `enabled`       — bool, default true
//!   `mqtt_host`     — broker hostname, default "127.0.0.1"
//!   `mqtt_port`     — broker port, default 1883
//!   `mqtt_username` — optional string
//!   `mqtt_password` — optional string
//!   `devices`       — array of device config tables (see `DeviceConfig`)
//!
//! Per-device config keys (`[[adapters.mqtt.devices]]`):
//!   `vendor_id`        — stable device ID fragment, e.g. "esp32_bedroom"
//!   `kind`             — "light" | "switch" | "sensor" | "virtual", default "sensor"
//!   `state_topic`      — (JSON mode) subscribe here for full JSON state objects
//!   `command_topic`    — (optional) publish commands here
//!   `command_format`   — "json" (default) | "plain" for raw action strings
//!   `attribute_topics` — (per-attribute mode) array of {topic, attribute} pairs
//!
//! Build:
//!   cargo build --release
//!   # Output: target/release/mqtt-adapter
//!
//! Deploy:
//!   Copy the binary into config/plugins/ alongside mqtt.plugin.toml.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use rumqttc::{AsyncClient, Event as MqttEvent, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

// ── Config ───────────────────────────────────────────────────────────────────
// Settings read at startup from the HOMECMDR_ADAPTER_CONFIG environment
// variable. Any field left out will use the default shown in the fn below.

/// Top-level adapter configuration, parsed from `HOMECMDR_ADAPTER_CONFIG`.
#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_mqtt_host")]
    mqtt_host: String,
    #[serde(default = "default_mqtt_port")]
    mqtt_port: u16,
    #[serde(default)]
    mqtt_username: Option<String>,
    #[serde(default)]
    mqtt_password: Option<String>,
    /// List of MQTT devices managed by this adapter.
    #[serde(default)]
    devices: Vec<DeviceConfig>,
}

/// Configuration for a single MQTT device.
#[derive(Debug, Deserialize, Clone)]
struct DeviceConfig {
    /// Stable identifier fragment for this device. HomeCmdr device ID will be
    /// `mqtt:<vendor_id>`.
    vendor_id: String,
    /// HomeCmdr device kind: "light" | "switch" | "sensor" | "virtual".
    #[serde(default = "default_sensor")]
    kind: String,
    /// JSON mode: topic that carries a full JSON state object.
    /// When a message arrives here, all object keys are mapped through the
    /// canonical attribute table and posted to HomeCmdr.
    #[serde(default)]
    state_topic: Option<String>,
    /// Topic to publish outbound commands to. If absent, commands for this
    /// device are silently dropped with a debug log.
    #[serde(default)]
    command_topic: Option<String>,
    /// Command payload format:
    ///   "json"  (default) — `{"capability":"value"}`, e.g. `{"state":"ON"}`
    ///   "plain"           — raw action string, e.g. `ON` (Tasmota-style)
    #[serde(default = "default_json")]
    command_format: String,
    /// Per-attribute mode: each entry maps one MQTT topic to one attribute.
    #[serde(default)]
    attribute_topics: Vec<AttributeTopicConfig>,
}

/// Maps a single MQTT topic to a HomeCmdr attribute name.
#[derive(Debug, Deserialize, Clone)]
struct AttributeTopicConfig {
    /// MQTT topic to subscribe to.
    topic: String,
    /// HomeCmdr canonical attribute name, e.g. "temperature", "state",
    /// "humidity". The raw payload is normalised through the same attribute
    /// mapping used in JSON mode.
    attribute: String,
}

fn default_true() -> bool {
    true
}
fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_sensor() -> String {
    "sensor".to_string()
}
fn default_json() -> String {
    "json".to_string()
}

// ── HomeCmdr ingest API types ─────────────────────────────────────────────────
// The JSON shapes we POST to `POST /ingest/devices` when we have device state
// to report.

/// The top-level request body for `POST /ingest/devices`.
#[derive(Serialize)]
struct IngestRequest<'a> {
    adapter: &'a str,
    devices: Vec<IngestDevice>,
}

/// One device entry inside an ingest request.
#[derive(Serialize)]
struct IngestDevice {
    vendor_id: String,
    kind: String,
    attributes: Value,
    metadata: Value,
}

// ── HomeCmdr WebSocket event types ───────────────────────────────────────────
// Events that arrive over the HomeCmdr `/events` WebSocket. Only
// `device.command_dispatched` events are acted upon.

/// A single event received from the HomeCmdr WebSocket stream.
#[derive(Deserialize)]
struct WsEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    command: Option<DeviceCommand>,
}

/// The command payload attached to a `device.command_dispatched` event.
#[derive(Deserialize)]
struct DeviceCommand {
    capability: String,
    action: String,
    #[serde(default)]
    value: Option<Value>,
}

// ── Outbound MQTT publish ─────────────────────────────────────────────────────
// A message queued by the WebSocket task and consumed by the MQTT task.
// The channel decouples the two tasks so neither blocks the other.

struct MqttPublish {
    topic: String,
    payload: Vec<u8>,
}

// ── Topic lookup maps ─────────────────────────────────────────────────────────
// Built once from config at startup; never mutated after that.

struct TopicMaps {
    /// JSON mode: state_topic → vendor_id.
    json_topics: HashMap<String, String>,
    /// Per-attribute mode: topic → (vendor_id, attribute_name).
    attr_topics: HashMap<String, (String, String)>,
    /// vendor_id → DeviceConfig, for command dispatch and kind lookup.
    devices: HashMap<String, DeviceConfig>,
}

impl TopicMaps {
    fn build(devices: &[DeviceConfig]) -> Self {
        let mut json_topics = HashMap::new();
        let mut attr_topics = HashMap::new();
        let mut device_map = HashMap::new();

        for dev in devices {
            device_map.insert(dev.vendor_id.clone(), dev.clone());
            if let Some(ref t) = dev.state_topic {
                json_topics.insert(t.clone(), dev.vendor_id.clone());
            }
            for at in &dev.attribute_topics {
                attr_topics.insert(at.topic.clone(), (dev.vendor_id.clone(), at.attribute.clone()));
            }
        }

        Self { json_topics, attr_topics, devices: device_map }
    }

    fn all_subscribe_topics(&self) -> Vec<String> {
        let mut topics: Vec<String> = self.json_topics.keys().cloned().collect();
        topics.extend(self.attr_topics.keys().cloned());
        topics
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────
// Only per-attribute mode needs shared mutable state: we accumulate individual
// attribute values and post the full device snapshot on every update.

#[derive(Default)]
struct SharedState {
    /// Per-attribute cache: vendor_id → { attribute_name → value }.
    attr_cache: HashMap<String, serde_json::Map<String, Value>>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let api_url = std::env::var("HOMECMDR_API_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3001".to_string());
    let api_token = std::env::var("HOMECMDR_API_TOKEN").unwrap_or_default();
    let config_json = std::env::var("HOMECMDR_ADAPTER_CONFIG")
        .unwrap_or_else(|_| "{}".to_string());

    let config: Config = serde_json::from_str(&config_json).unwrap_or_else(|e| {
        tracing::warn!("mqtt: bad config ({e}); using defaults");
        serde_json::from_str("{}").unwrap()
    });

    if !config.enabled {
        tracing::info!("mqtt adapter disabled by config — exiting");
        return;
    }

    if config.devices.is_empty() {
        tracing::warn!("mqtt: no devices configured — adapter will subscribe to no topics");
    }

    let topic_maps = Arc::new(TopicMaps::build(&config.devices));
    let state = Arc::new(Mutex::new(SharedState::default()));
    let (cmd_tx, cmd_rx) = mpsc::channel::<MqttPublish>(128);

    tokio::join!(
        run_mqtt(
            config,
            Arc::clone(&topic_maps),
            api_url.clone(),
            api_token.clone(),
            Arc::clone(&state),
            cmd_rx,
        ),
        run_websocket(api_url, api_token, topic_maps, cmd_tx),
    );
}

// ── MQTT task ─────────────────────────────────────────────────────────────────
// Connects to the broker, subscribes to all configured topics, and dispatches
// each incoming message to the appropriate handler. Also drains the outbound
// command channel. rumqttc reconnects automatically on broker disconnect.

/// Run the MQTT event loop forever.
///
/// Handles incoming device state messages and outgoing command publishes.
/// Never returns unless the process exits.
async fn run_mqtt(
    config: Config,
    topic_maps: Arc<TopicMaps>,
    api_url: String,
    api_token: String,
    state: Arc<Mutex<SharedState>>,
    mut cmd_rx: mpsc::Receiver<MqttPublish>,
) {
    let Config { mqtt_host, mqtt_port, mqtt_username, mqtt_password, .. } = config;

    let (client, mut eventloop) =
        mqtt_connect("homecmdr-mqtt", &mqtt_host, mqtt_port, mqtt_username, mqtt_password);
    let http_client = reqwest::Client::new();

    loop {
        tokio::select! {
            event = eventloop.poll() => {
                match event {
                    Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                        tracing::info!(
                            "mqtt: connected to broker {}:{}",
                            mqtt_host, mqtt_port
                        );
                        let topics = topic_maps.all_subscribe_topics();
                        if topics.is_empty() {
                            tracing::debug!("mqtt: no topics to subscribe");
                        } else {
                            let cl = client.clone();
                            tokio::spawn(async move {
                                for topic in topics {
                                    if let Err(e) =
                                        cl.subscribe(&topic, QoS::AtLeastOnce).await
                                    {
                                        tracing::error!("mqtt: subscribe to {topic} failed: {e}");
                                    } else {
                                        tracing::debug!("mqtt: subscribed to {topic}");
                                    }
                                }
                            });
                        }
                    }

                    Ok(MqttEvent::Incoming(Packet::Publish(p))) => {
                        let topic = p.topic.clone();
                        let payload = match std::str::from_utf8(&p.payload) {
                            Ok(s) => s.to_string(),
                            Err(_) => continue,
                        };

                        if let Some(vendor_id) = topic_maps.json_topics.get(&topic) {
                            let vendor_id = vendor_id.clone();
                            let dev = topic_maps.devices.get(&vendor_id).cloned();
                            let hc = http_client.clone();
                            let au = api_url.clone();
                            let at = api_token.clone();
                            tokio::spawn(async move {
                                handle_json_message(
                                    &vendor_id, dev.as_ref(), &payload, &hc, &au, &at,
                                )
                                .await;
                            });
                        } else if let Some((vendor_id, attribute)) =
                            topic_maps.attr_topics.get(&topic)
                        {
                            let vendor_id = vendor_id.clone();
                            let attribute = attribute.clone();
                            let dev = topic_maps.devices.get(&vendor_id).cloned();
                            let st = Arc::clone(&state);
                            let hc = http_client.clone();
                            let au = api_url.clone();
                            let at = api_token.clone();
                            tokio::spawn(async move {
                                handle_attr_message(
                                    &vendor_id, &attribute, &payload,
                                    dev.as_ref(), &st, &hc, &au, &at,
                                )
                                .await;
                            });
                        }
                    }

                    Ok(_) => {}

                    Err(e) => {
                        tracing::warn!("mqtt: MQTT error: {e}; will retry");
                        // rumqttc reconnects automatically; throttle the poll loop.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }

            // Forward outbound MQTT publishes from the WebSocket task.
            msg = cmd_rx.recv() => {
                if let Some(m) = msg {
                    if let Err(e) =
                        client.publish(&m.topic, QoS::AtLeastOnce, false, m.payload).await
                    {
                        tracing::error!("mqtt: MQTT publish failed: {e}");
                    }
                }
            }
        }
    }
}

/// Build an MQTT client + event loop with standard HomeCmdr settings.
fn mqtt_connect(
    client_id: &str,
    host: &str,
    port: u16,
    username: Option<String>,
    password: Option<String>,
) -> (AsyncClient, rumqttc::EventLoop) {
    let mut opts = MqttOptions::new(client_id, host, port);
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_clean_session(true);
    opts.set_max_packet_size(1 * 1024 * 1024, 1 * 1024 * 1024);
    if let (Some(u), Some(p)) = (username, password) {
        opts.set_credentials(u, p);
    }
    AsyncClient::new(opts, 64)
}

// ── JSON-mode message handler ─────────────────────────────────────────────────

/// Handle a full JSON state payload received on a `state_topic`.
///
/// Skips non-object payloads (e.g. plain string availability messages).
async fn handle_json_message(
    vendor_id: &str,
    device: Option<&DeviceConfig>,
    payload: &str,
    http: &reqwest::Client,
    api_url: &str,
    api_token: &str,
) {
    let raw: serde_json::Map<String, Value> = match serde_json::from_str(payload) {
        Ok(Value::Object(m)) => m,
        _ => {
            tracing::debug!(
                "mqtt: non-JSON-object payload on {vendor_id} state_topic, skipping"
            );
            return;
        }
    };

    let kind = device.map(|d| d.kind.as_str()).unwrap_or("sensor").to_string();
    let attributes = map_attributes(&raw);

    post_ingest(
        http,
        api_url,
        api_token,
        vec![IngestDevice {
            vendor_id: vendor_id.to_string(),
            kind,
            attributes,
            metadata: serde_json::json!({}),
        }],
    )
    .await;
}

// ── Per-attribute message handler ─────────────────────────────────────────────

/// Handle a single attribute update from a per-attribute topic.
///
/// Normalises the incoming value through `map_attributes`, merges it into the
/// in-memory attribute cache for this device, and posts the full accumulated
/// state snapshot to the HomeCmdr API.
async fn handle_attr_message(
    vendor_id: &str,
    attribute: &str,
    raw_payload: &str,
    device: Option<&DeviceConfig>,
    state: &Arc<Mutex<SharedState>>,
    http: &reqwest::Client,
    api_url: &str,
    api_token: &str,
) {
    // Parse the raw MQTT payload to a JSON value (number, bool, string, etc.).
    let raw_value = parse_attr_value(raw_payload);

    // Run it through the canonical attribute mapping so that, for example,
    // attribute = "temperature" with payload "21.5" becomes
    // {"temperature": {"value": 21.5, "unit": "celsius"}}.
    let mut single = serde_json::Map::new();
    single.insert(attribute.to_string(), raw_value);
    let normalised = map_attributes(&single);

    let (kind, all_attrs) = {
        let mut st = state.lock().await;
        let cache = st.attr_cache.entry(vendor_id.to_string()).or_default();
        if let Value::Object(norm_map) = normalised {
            for (k, v) in norm_map {
                cache.insert(k, v);
            }
        }
        let kind = device.map(|d| d.kind.as_str()).unwrap_or("sensor").to_string();
        (kind, Value::Object(cache.clone()))
    };

    post_ingest(
        http,
        api_url,
        api_token,
        vec![IngestDevice {
            vendor_id: vendor_id.to_string(),
            kind,
            attributes: all_attrs,
            metadata: serde_json::json!({}),
        }],
    )
    .await;
}

/// Parse a raw MQTT payload as a JSON value, falling back to a plain string.
fn parse_attr_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

// ── Attribute mapping ─────────────────────────────────────────────────────────
// Converts a raw MQTT JSON object to HomeCmdr canonical attribute shapes.
// Mirrors the mapping in plugin-zigbee2mqtt so HomeCmdr sees the same
// attribute keys regardless of which MQTT adapter reported the device.

/// Map a raw MQTT JSON state object to HomeCmdr canonical attributes.
///
/// Known keys are normalised (e.g. `"state": "ON"` → `"state": "on"`;
/// `"temperature": 21.5` → `"temperature": {"value": 21.5, "unit": "celsius"}`).
/// Any key without a canonical mapping falls through to `custom.mqtt.<key>`
/// so no data is silently dropped.
fn map_attributes(state: &serde_json::Map<String, Value>) -> Value {
    let mut attrs = serde_json::Map::new();

    for (key, value) in state {
        match key.as_str() {
            // ── On/Off state ───────────────────────────────────────────────
            "state" => {
                match normalise_onoff(value) {
                    Some(s) => {
                        attrs.insert("state".to_string(), Value::String(s));
                    }
                    None => {
                        attrs.insert("custom.mqtt.state".to_string(), value.clone());
                    }
                }
            }

            // ── Lighting ───────────────────────────────────────────────────
            // Brightness is expected as a 0-100 percentage; 0-255 raw byte
            // values (ESPHome default) should be converted by the firmware or
            // handled with a per-attribute topic at the "brightness" key.
            "brightness" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert("brightness".to_string(), serde_json::json!(v.round() as i64));
                }
            }

            // ── Sensors ────────────────────────────────────────────────────
            "temperature" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "temperature".to_string(),
                        serde_json::json!({ "value": v, "unit": "celsius" }),
                    );
                }
            }
            "humidity" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "humidity".to_string(),
                        serde_json::json!({ "value": v, "unit": "percent" }),
                    );
                }
            }
            "pressure" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "pressure".to_string(),
                        serde_json::json!({ "value": v, "unit": "hPa" }),
                    );
                }
            }
            "co2" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "co2".to_string(),
                        serde_json::json!({ "value": v, "unit": "ppm" }),
                    );
                }
            }
            "battery" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert("battery".to_string(), serde_json::json!(v.round() as i64));
                }
            }
            "illuminance" | "illuminance_lux" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "illuminance".to_string(),
                        serde_json::json!(v.round() as i64),
                    );
                }
            }
            "occupancy" => {
                let occupied = match value {
                    Value::Bool(b) => *b,
                    Value::String(s) => {
                        matches!(s.to_ascii_lowercase().as_str(), "occupied" | "true" | "1")
                    }
                    _ => false,
                };
                attrs.insert(
                    "occupancy".to_string(),
                    Value::String(
                        if occupied { "occupied" } else { "unoccupied" }.to_string(),
                    ),
                );
            }
            "contact" => {
                // Convention: true / "closed" = contact present = door/window shut.
                let closed = match value {
                    Value::Bool(b) => *b,
                    Value::String(s) => {
                        matches!(s.to_ascii_lowercase().as_str(), "closed" | "true" | "1")
                    }
                    _ => false,
                };
                attrs.insert(
                    "contact".to_string(),
                    Value::String(if closed { "closed" } else { "open" }.to_string()),
                );
            }
            "smoke" => {
                if let Some(b) = value.as_bool() {
                    attrs.insert("smoke".to_string(), Value::Bool(b));
                }
            }
            "water_leak" => {
                if let Some(b) = value.as_bool() {
                    attrs.insert("water_leak".to_string(), Value::Bool(b));
                }
            }

            // ── Energy ─────────────────────────────────────────────────────
            "power_consumption" | "wattage" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "power_consumption".to_string(),
                        serde_json::json!({ "value": v, "unit": "W" }),
                    );
                }
            }
            "voltage" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "voltage".to_string(),
                        serde_json::json!({ "value": v, "unit": "V" }),
                    );
                }
            }
            "current" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "current".to_string(),
                        serde_json::json!({ "value": v, "unit": "A" }),
                    );
                }
            }
            "energy" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "energy_total".to_string(),
                        serde_json::json!({ "value": v, "unit": "kWh", "period": "lifetime" }),
                    );
                }
            }

            // ── Fallthrough ────────────────────────────────────────────────
            other => {
                attrs.insert(format!("custom.mqtt.{other}"), value.clone());
            }
        }
    }

    Value::Object(attrs)
}

/// Normalise an on/off value to the canonical string "on" or "off".
///
/// Returns `None` for values that cannot be interpreted as on/off
/// (the caller falls through to `custom.mqtt.state`).
fn normalise_onoff(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(
            match s.to_ascii_lowercase().as_str() {
                "on" | "true" | "1" => "on",
                "off" | "false" | "0" => "off",
                other => other,
            }
            .to_string(),
        ),
        Value::Bool(b) => Some(if *b { "on" } else { "off" }.to_string()),
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some("on".to_string()),
            Some(0) => Some("off".to_string()),
            _ => None,
        },
        _ => None,
    }
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
    let req = IngestRequest { adapter: "mqtt", devices };
    let url = format!("{}/ingest/devices", api_url.trim_end_matches('/'));

    match http.post(&url).bearer_auth(api_token).json(&req).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::debug!("mqtt: ingest OK ({count} devices)");
        }
        Ok(r) => {
            tracing::warn!("mqtt: ingest returned {}", r.status());
        }
        Err(e) => {
            tracing::warn!("mqtt: ingest HTTP error: {e}");
        }
    }
}

// ── WebSocket task ────────────────────────────────────────────────────────────
// Connects to the HomeCmdr API's `/events` WebSocket and listens for
// `device.command_dispatched` events. Reconnects with exponential back-off.

/// Maintain the WebSocket connection to the HomeCmdr API.
///
/// Reconnects automatically after any failure, backing off from 1 s up to
/// 60 s between attempts. Never returns unless the process exits.
async fn run_websocket(
    api_url: String,
    api_token: String,
    topic_maps: Arc<TopicMaps>,
    cmd_tx: mpsc::Sender<MqttPublish>,
) {
    let ws_url = {
        let base = api_url.trim_end_matches('/');
        let base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        format!("{base}/events")
    };

    let mut backoff = Duration::from_secs(1);
    loop {
        match ws_session(&ws_url, &api_token, &topic_maps, &cmd_tx).await {
            Ok(()) => {
                tracing::info!("mqtt: WebSocket session ended");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!("mqtt: WebSocket error: {e}");
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
    topic_maps: &Arc<TopicMaps>,
    cmd_tx: &mpsc::Sender<MqttPublish>,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};

    let mut req = ws_url.into_client_request()?;
    req.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_token}"))?,
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    tracing::info!("mqtt: WebSocket connected to {ws_url}");

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
            dispatch_command(event, topic_maps, cmd_tx).await;
        }
    }

    Ok(())
}

// ── Command dispatch ──────────────────────────────────────────────────────────

/// Convert a `device.command_dispatched` event into an MQTT publish.
///
/// Ignores commands for devices not managed by this adapter or devices without
/// a `command_topic`.
async fn dispatch_command(
    event: WsEvent,
    topic_maps: &Arc<TopicMaps>,
    cmd_tx: &mpsc::Sender<MqttPublish>,
) {
    let device_id = match event.id.as_deref() {
        Some(id) if id.starts_with("mqtt:") => id.to_string(),
        _ => return,
    };
    let cmd = match event.command {
        Some(c) => c,
        None => return,
    };

    let vendor_id = device_id.strip_prefix("mqtt:").unwrap_or(&device_id);

    let device = match topic_maps.devices.get(vendor_id) {
        Some(d) => d,
        None => {
            tracing::debug!("mqtt: command for unknown device '{vendor_id}', ignoring");
            return;
        }
    };

    let command_topic = match &device.command_topic {
        Some(t) => t.clone(),
        None => {
            tracing::debug!(
                "mqtt: device '{vendor_id}' has no command_topic, dropping command"
            );
            return;
        }
    };

    let payload_str = match build_command_payload(&cmd, &device.command_format) {
        Some(p) => p,
        None => {
            tracing::debug!(
                "mqtt: unhandled command {}/{} for '{vendor_id}'",
                cmd.capability,
                cmd.action
            );
            return;
        }
    };

    tracing::debug!("mqtt: publish {command_topic} ← {payload_str}");
    let _ = cmd_tx
        .send(MqttPublish { topic: command_topic, payload: payload_str.into_bytes() })
        .await;
}

/// Build an MQTT command payload string from a HomeCmdr device command.
///
/// For `command_format = "json"`:  publishes a JSON object, e.g. `{"state":"ON"}`.
/// For `command_format = "plain"`: publishes the raw action string, e.g. `ON`.
///
/// Returns `None` for commands with no value and no explicit handler (the
/// caller logs a debug message and skips the publish).
fn build_command_payload(cmd: &DeviceCommand, format: &str) -> Option<String> {
    let plain = format == "plain";
    match (cmd.capability.as_str(), cmd.action.as_str()) {
        ("state", "on") => Some(if plain {
            "ON".to_string()
        } else {
            r#"{"state":"ON"}"#.to_string()
        }),
        ("state", "off") => Some(if plain {
            "OFF".to_string()
        } else {
            r#"{"state":"OFF"}"#.to_string()
        }),
        ("state", "toggle") => Some(if plain {
            "TOGGLE".to_string()
        } else {
            r#"{"state":"TOGGLE"}"#.to_string()
        }),
        ("brightness", "set") => {
            let pct = cmd.value.as_ref()?.as_f64()?;
            let raw = pct.round() as i64;
            if plain {
                Some(format!("{raw}"))
            } else {
                Some(format!(r#"{{"brightness":{raw}}}"#))
            }
        }
        (cap, _act) => {
            // Generic fallback: publish the value under the capability name.
            let v = cmd.value.as_ref()?;
            if plain {
                Some(match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
            } else {
                let mut obj = serde_json::Map::new();
                obj.insert(cap.to_string(), v.clone());
                Some(Value::Object(obj).to_string())
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(state: &serde_json::Map<String, Value>) -> Value {
        map_attributes(state)
    }

    // ── On/Off state ──────────────────────────────────────────────────────────

    #[test]
    fn state_on_uppercase() {
        let mut s = serde_json::Map::new();
        s.insert("state".to_string(), serde_json::json!("ON"));
        assert_eq!(attrs(&s)["state"], serde_json::json!("on"));
    }

    #[test]
    fn state_off_uppercase() {
        let mut s = serde_json::Map::new();
        s.insert("state".to_string(), serde_json::json!("OFF"));
        assert_eq!(attrs(&s)["state"], serde_json::json!("off"));
    }

    #[test]
    fn state_bool_true() {
        let mut s = serde_json::Map::new();
        s.insert("state".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["state"], serde_json::json!("on"));
    }

    #[test]
    fn state_bool_false() {
        let mut s = serde_json::Map::new();
        s.insert("state".to_string(), serde_json::json!(false));
        assert_eq!(attrs(&s)["state"], serde_json::json!("off"));
    }

    // ── Lighting ──────────────────────────────────────────────────────────────

    #[test]
    fn brightness_passthrough() {
        let mut s = serde_json::Map::new();
        s.insert("brightness".to_string(), serde_json::json!(75));
        assert_eq!(attrs(&s)["brightness"], serde_json::json!(75_i64));
    }

    // ── Sensors ───────────────────────────────────────────────────────────────

    #[test]
    fn temperature_sensor() {
        let mut s = serde_json::Map::new();
        s.insert("temperature".to_string(), serde_json::json!(22.5_f64));
        assert_eq!(
            attrs(&s)["temperature"],
            serde_json::json!({ "value": 22.5, "unit": "celsius" })
        );
    }

    #[test]
    fn humidity_sensor() {
        let mut s = serde_json::Map::new();
        s.insert("humidity".to_string(), serde_json::json!(60.0_f64));
        assert_eq!(
            attrs(&s)["humidity"],
            serde_json::json!({ "value": 60.0, "unit": "percent" })
        );
    }

    #[test]
    fn pressure_sensor() {
        let mut s = serde_json::Map::new();
        s.insert("pressure".to_string(), serde_json::json!(1013.0_f64));
        assert_eq!(
            attrs(&s)["pressure"],
            serde_json::json!({ "value": 1013.0, "unit": "hPa" })
        );
    }

    #[test]
    fn co2_sensor() {
        let mut s = serde_json::Map::new();
        s.insert("co2".to_string(), serde_json::json!(850.0_f64));
        assert_eq!(
            attrs(&s)["co2"],
            serde_json::json!({ "value": 850.0, "unit": "ppm" })
        );
    }

    #[test]
    fn battery_percentage() {
        let mut s = serde_json::Map::new();
        s.insert("battery".to_string(), serde_json::json!(87.0_f64));
        assert_eq!(attrs(&s)["battery"], serde_json::json!(87_i64));
    }

    #[test]
    fn illuminance_lux_key() {
        let mut s = serde_json::Map::new();
        s.insert("illuminance_lux".to_string(), serde_json::json!(300.0_f64));
        assert_eq!(attrs(&s)["illuminance"], serde_json::json!(300_i64));
        // original key must not appear
        assert!(attrs(&s).get("illuminance_lux").is_none());
    }

    #[test]
    fn occupancy_bool_true() {
        let mut s = serde_json::Map::new();
        s.insert("occupancy".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["occupancy"], serde_json::json!("occupied"));
    }

    #[test]
    fn occupancy_bool_false() {
        let mut s = serde_json::Map::new();
        s.insert("occupancy".to_string(), serde_json::json!(false));
        assert_eq!(attrs(&s)["occupancy"], serde_json::json!("unoccupied"));
    }

    #[test]
    fn contact_closed() {
        let mut s = serde_json::Map::new();
        s.insert("contact".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["contact"], serde_json::json!("closed"));
    }

    #[test]
    fn contact_open() {
        let mut s = serde_json::Map::new();
        s.insert("contact".to_string(), serde_json::json!(false));
        assert_eq!(attrs(&s)["contact"], serde_json::json!("open"));
    }

    // ── Energy ────────────────────────────────────────────────────────────────

    #[test]
    fn power_consumption() {
        let mut s = serde_json::Map::new();
        s.insert("power_consumption".to_string(), serde_json::json!(120.5_f64));
        assert_eq!(
            attrs(&s)["power_consumption"],
            serde_json::json!({ "value": 120.5, "unit": "W" })
        );
    }

    #[test]
    fn voltage_measurement() {
        let mut s = serde_json::Map::new();
        s.insert("voltage".to_string(), serde_json::json!(230.0_f64));
        assert_eq!(
            attrs(&s)["voltage"],
            serde_json::json!({ "value": 230.0, "unit": "V" })
        );
    }

    #[test]
    fn energy_total() {
        let mut s = serde_json::Map::new();
        s.insert("energy".to_string(), serde_json::json!(12.3_f64));
        assert_eq!(
            attrs(&s)["energy_total"],
            serde_json::json!({ "value": 12.3, "unit": "kWh", "period": "lifetime" })
        );
    }

    // ── Fallthrough ───────────────────────────────────────────────────────────

    #[test]
    fn unknown_key_falls_to_custom() {
        let mut s = serde_json::Map::new();
        s.insert("rssi".to_string(), serde_json::json!(-70));
        assert_eq!(attrs(&s)["custom.mqtt.rssi"], serde_json::json!(-70));
    }

    // ── parse_attr_value ──────────────────────────────────────────────────────

    #[test]
    fn parse_attr_value_number() {
        assert_eq!(parse_attr_value("21.5"), serde_json::json!(21.5_f64));
    }

    #[test]
    fn parse_attr_value_bool() {
        assert_eq!(parse_attr_value("true"), serde_json::json!(true));
    }

    #[test]
    fn parse_attr_value_plain_string() {
        assert_eq!(parse_attr_value("ON"), Value::String("ON".to_string()));
    }

    // ── Command payload builder ───────────────────────────────────────────────

    #[test]
    fn command_state_on_json() {
        let cmd = DeviceCommand {
            capability: "state".to_string(),
            action: "on".to_string(),
            value: None,
        };
        assert_eq!(
            build_command_payload(&cmd, "json"),
            Some(r#"{"state":"ON"}"#.to_string())
        );
    }

    #[test]
    fn command_state_off_plain() {
        let cmd = DeviceCommand {
            capability: "state".to_string(),
            action: "off".to_string(),
            value: None,
        };
        assert_eq!(build_command_payload(&cmd, "plain"), Some("OFF".to_string()));
    }

    #[test]
    fn command_state_toggle_plain() {
        let cmd = DeviceCommand {
            capability: "state".to_string(),
            action: "toggle".to_string(),
            value: None,
        };
        assert_eq!(build_command_payload(&cmd, "plain"), Some("TOGGLE".to_string()));
    }

    #[test]
    fn command_brightness_set_json() {
        let cmd = DeviceCommand {
            capability: "brightness".to_string(),
            action: "set".to_string(),
            value: Some(serde_json::json!(75.0_f64)),
        };
        assert_eq!(
            build_command_payload(&cmd, "json"),
            Some(r#"{"brightness":75}"#.to_string())
        );
    }

    #[test]
    fn command_brightness_set_plain() {
        let cmd = DeviceCommand {
            capability: "brightness".to_string(),
            action: "set".to_string(),
            value: Some(serde_json::json!(50.0_f64)),
        };
        assert_eq!(build_command_payload(&cmd, "plain"), Some("50".to_string()));
    }

    #[test]
    fn command_no_value_no_handler_returns_none() {
        let cmd = DeviceCommand {
            capability: "custom".to_string(),
            action: "trigger".to_string(),
            value: None,
        };
        assert_eq!(build_command_payload(&cmd, "json"), None);
    }
}
