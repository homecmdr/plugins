//! Zigbee2MQTT IPC adapter for HomeCmdr.
//!
//! Reads the following environment variables set by the IPC host:
//!
//! - `HOMECMDR_API_URL`        — base URL of the running HomeCmdr API
//! - `HOMECMDR_API_TOKEN`      — bearer token for authentication
//! - `HOMECMDR_ADAPTER_CONFIG` — JSON-encoded adapter config block
//!
//! Config keys (in `HOMECMDR_ADAPTER_CONFIG`):
//! - `enabled`        — bool, default true
//! - `mqtt_host`      — MQTT broker hostname, default "127.0.0.1"
//! - `mqtt_port`      — MQTT broker port, default 1883
//! - `mqtt_username`  — optional MQTT username
//! - `mqtt_password`  — optional MQTT password
//! - `base_topic`     — Zigbee2MQTT base topic, default "zigbee2mqtt"
//!
//! The adapter subscribes to `{base_topic}/#` over MQTT.  Incoming device
//! state messages are pushed to the HomeCmdr API via `POST /ingest/devices`.
//! Outbound commands arrive from the `/events` WebSocket as
//! `device.command_dispatched` events, which are translated to MQTT
//! publishes on `{base_topic}/{friendly_name}/set`.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use rumqttc::{AsyncClient, Event as MqttEvent, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

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
    #[serde(default = "default_base_topic")]
    base_topic: String,
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
fn default_base_topic() -> String {
    "zigbee2mqtt".to_string()
}

// ---------------------------------------------------------------------------
// Zigbee2MQTT bridge/devices message types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BridgeDevice {
    friendly_name: String,
    #[serde(rename = "type", default)]
    device_type: String,
    #[serde(default)]
    definition: Option<BridgeDefinition>,
    #[serde(default)]
    state: HashMap<String, Value>,
}

#[derive(Deserialize, Default, Clone)]
struct BridgeDefinition {
    #[serde(default)]
    model: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    exposes: Vec<Expose>,
}

#[derive(Deserialize, Clone)]
struct Expose {
    #[serde(rename = "type", default)]
    expose_type: String,
}

// ---------------------------------------------------------------------------
// HomeCmdr ingest API types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IngestRequest<'a> {
    adapter: &'a str,
    devices: Vec<IngestDevice>,
}

#[derive(Serialize)]
struct IngestDevice {
    vendor_id: String,
    kind: String,
    attributes: Value,
}

// ---------------------------------------------------------------------------
// HomeCmdr WebSocket event types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Outbound MQTT publish (sent from WebSocket task → MQTT task)
// ---------------------------------------------------------------------------

struct MqttPublish {
    topic: String,
    payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SharedState {
    /// vendor_id → original friendly_name (for reversing sanitisation in commands)
    vendor_id_map: HashMap<String, String>,
    /// friendly_name → device kind string (populated from bridge/devices)
    device_kinds: HashMap<String, String>,
    /// friendly_name → bridge definition (for attribute metadata)
    device_definitions: HashMap<String, BridgeDefinition>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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
        tracing::warn!("zigbee2mqtt: bad config ({e}); using defaults");
        serde_json::from_str("{}").unwrap()
    });

    if !config.enabled {
        tracing::info!("zigbee2mqtt adapter disabled by config — exiting");
        return;
    }

    let state = Arc::new(Mutex::new(SharedState::default()));
    let (cmd_tx, cmd_rx) = mpsc::channel::<MqttPublish>(128);

    tokio::join!(
        run_mqtt(
            config,
            api_url.clone(),
            api_token.clone(),
            Arc::clone(&state),
            cmd_rx
        ),
        run_websocket(api_url, api_token, state, cmd_tx),
    );
}

// ---------------------------------------------------------------------------
// MQTT task
// ---------------------------------------------------------------------------

async fn run_mqtt(
    config: Config,
    api_url: String,
    api_token: String,
    state: Arc<Mutex<SharedState>>,
    mut cmd_rx: mpsc::Receiver<MqttPublish>,
) {
    let Config {
        mqtt_host,
        mqtt_port,
        mqtt_username,
        mqtt_password,
        base_topic,
        ..
    } = config;
    let base_topic = base_topic.trim_end_matches('/').to_string();

    let mut mqttopts = MqttOptions::new("homecmdr-zigbee2mqtt", &mqtt_host, mqtt_port);
    mqttopts.set_keep_alive(Duration::from_secs(30));
    mqttopts.set_clean_session(true);
    // bridge/devices payloads can exceed the rumqttc default (10 KB).
    // 10 MB is generous but safe for large Zigbee networks.
    mqttopts.set_max_packet_size(10 * 1024 * 1024, 10 * 1024 * 1024);
    if let (Some(user), Some(pass)) = (mqtt_username, mqtt_password) {
        mqttopts.set_credentials(user, pass);
    }

    let (client, mut eventloop) = AsyncClient::new(mqttopts, 64);
    let http_client = reqwest::Client::new();

    loop {
        tokio::select! {
            event = eventloop.poll() => {
                match event {
                    Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                        tracing::info!(
                            "zigbee2mqtt: connected to MQTT broker {}:{}",
                            mqtt_host, mqtt_port
                        );
                        // Subscribe to the full topic tree; bridge/devices is a sub-topic.
                        let bt = base_topic.clone();
                        let cl = client.clone();
                        tokio::spawn(async move {
                            if let Err(e) = cl.subscribe(format!("{bt}/#"), QoS::AtLeastOnce).await {
                                tracing::error!("zigbee2mqtt: subscribe failed: {e}");
                            } else {
                                tracing::info!("zigbee2mqtt: subscribed to {bt}/#");
                            }
                        });
                    }

                    Ok(MqttEvent::Incoming(Packet::Publish(p))) => {
                        let topic = p.topic.clone();
                        let payload = match std::str::from_utf8(&p.payload) {
                            Ok(s) => s.to_string(),
                            Err(_) => continue,
                        };

                        let bridge_topic = format!("{}/bridge/devices", base_topic);
                        let prefix = format!("{}/", base_topic);

                        if topic == bridge_topic {
                            let st = Arc::clone(&state);
                            let hc = http_client.clone();
                            let au = api_url.clone();
                            let at = api_token.clone();
                            tokio::spawn(async move {
                                handle_bridge_devices(&payload, &st, &hc, &au, &at).await;
                            });
                        } else if let Some(suffix) = topic.strip_prefix(&prefix) {
                            // Only handle top-level device state topics.
                            // Exclude any sub-topics (bridge/*, <device>/availability, etc.).
                            if !suffix.is_empty() && !suffix.contains('/') {
                                let name = suffix.to_string();
                                let st = Arc::clone(&state);
                                let hc = http_client.clone();
                                let au = api_url.clone();
                                let at = api_token.clone();
                                tokio::spawn(async move {
                                    handle_device_state(&name, &payload, &st, &hc, &au, &at)
                                        .await;
                                });
                            }
                        }
                    }

                    Ok(_) => {}

                    Err(e) => {
                        tracing::warn!(
                            "zigbee2mqtt: MQTT error: {e}; will retry"
                        );
                        // rumqttc reconnects automatically; throttle the poll loop.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }

            // Forward outbound MQTT publishes from the WebSocket task.
            msg = cmd_rx.recv() => {
                if let Some(m) = msg {
                    if let Err(e) = client
                        .publish(&m.topic, QoS::AtLeastOnce, false, m.payload)
                        .await
                    {
                        tracing::error!("zigbee2mqtt: MQTT publish failed: {e}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// bridge/devices handler — full device list refresh
// ---------------------------------------------------------------------------

async fn handle_bridge_devices(
    payload: &str,
    state: &Arc<Mutex<SharedState>>,
    http: &reqwest::Client,
    api_url: &str,
    api_token: &str,
) {
    let devices: Vec<BridgeDevice> = match serde_json::from_str(payload) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("zigbee2mqtt: failed to parse bridge/devices: {e}");
            return;
        }
    };

    let mut ingest: Vec<IngestDevice> = Vec::new();

    {
        let mut st = state.lock().await;
        for d in &devices {
            if d.device_type.eq_ignore_ascii_case("Coordinator") {
                continue;
            }
            let vendor_id = sanitise_vendor_id(&d.friendly_name);
            let kind = infer_kind(&d.definition);

            st.vendor_id_map.insert(vendor_id.clone(), d.friendly_name.clone());
            st.device_kinds.insert(d.friendly_name.clone(), kind.clone());
            if let Some(def) = &d.definition {
                st.device_definitions.insert(d.friendly_name.clone(), def.clone());
            }

            let attributes = build_attributes(&d.state, &d.definition);
            ingest.push(IngestDevice { vendor_id, kind, attributes });
        }
    }

    if !ingest.is_empty() {
        post_ingest(http, api_url, api_token, ingest).await;
    }
}

// ---------------------------------------------------------------------------
// Per-device state message handler
// ---------------------------------------------------------------------------

async fn handle_device_state(
    friendly_name: &str,
    payload: &str,
    state: &Arc<Mutex<SharedState>>,
    http: &reqwest::Client,
    api_url: &str,
    api_token: &str,
) {
    let state_map: HashMap<String, Value> = match serde_json::from_str(payload) {
        Ok(m) => m,
        Err(_) => return, // Not a JSON object — availability strings, etc.
    };

    let (vendor_id, kind, definition) = {
        let mut st = state.lock().await;
        let vendor_id = sanitise_vendor_id(friendly_name);
        // Update reverse map so commands can find this device.
        st.vendor_id_map.insert(vendor_id.clone(), friendly_name.to_string());
        let kind = st
            .device_kinds
            .get(friendly_name)
            .cloned()
            .unwrap_or_else(|| "sensor".to_string());
        let definition = st.device_definitions.get(friendly_name).cloned();
        (vendor_id, kind, definition)
    };

    let attributes = build_attributes(&state_map, &definition);
    let devices = vec![IngestDevice { vendor_id, kind, attributes }];
    post_ingest(http, api_url, api_token, devices).await;
}

// ---------------------------------------------------------------------------
// HTTP ingest helper
// ---------------------------------------------------------------------------

async fn post_ingest(
    http: &reqwest::Client,
    api_url: &str,
    api_token: &str,
    devices: Vec<IngestDevice>,
) {
    let count = devices.len();
    let req = IngestRequest { adapter: "zigbee2mqtt", devices };
    let url = format!("{}/ingest/devices", api_url.trim_end_matches('/'));

    match http.post(&url).bearer_auth(api_token).json(&req).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::debug!("zigbee2mqtt: ingest OK ({count} devices)");
        }
        Ok(r) => {
            tracing::warn!("zigbee2mqtt: ingest returned {}", r.status());
        }
        Err(e) => {
            tracing::warn!("zigbee2mqtt: ingest HTTP error: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket task — receives device.command_dispatched events
// ---------------------------------------------------------------------------

async fn run_websocket(
    api_url: String,
    api_token: String,
    state: Arc<Mutex<SharedState>>,
    cmd_tx: mpsc::Sender<MqttPublish>,
) {
    let ws_url = {
        let base = api_url.trim_end_matches('/');
        let base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        format!("{base}/events")
    };

    let mut backoff = Duration::from_secs(1);
    loop {
        match ws_session(&ws_url, &api_token, &state, &cmd_tx).await {
            Ok(()) => {
                tracing::info!("zigbee2mqtt: WebSocket session ended");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!("zigbee2mqtt: WebSocket error: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn ws_session(
    ws_url: &str,
    api_token: &str,
    state: &Arc<Mutex<SharedState>>,
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
    tracing::info!("zigbee2mqtt: WebSocket connected to {ws_url}");

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
            dispatch_command(event, state, cmd_tx).await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Command dispatch: WebSocket event → MQTT publish
// ---------------------------------------------------------------------------

async fn dispatch_command(
    event: WsEvent,
    state: &Arc<Mutex<SharedState>>,
    cmd_tx: &mpsc::Sender<MqttPublish>,
) {
    let device_id = match event.id.as_deref() {
        Some(id) if id.starts_with("zigbee2mqtt:") => id.to_string(),
        _ => return,
    };
    let cmd = match event.command {
        Some(c) => c,
        None => return,
    };

    let vendor_id = device_id
        .strip_prefix("zigbee2mqtt:")
        .unwrap_or(&device_id);

    let friendly_name = {
        let st = state.lock().await;
        st.vendor_id_map
            .get(vendor_id)
            .cloned()
            .unwrap_or_else(|| vendor_id.to_string())
    };

    let payload_str = match build_mqtt_command(&cmd) {
        Some(p) => p,
        None => {
            tracing::debug!(
                "zigbee2mqtt: unhandled command {}/{} for '{friendly_name}'",
                cmd.capability, cmd.action
            );
            return;
        }
    };

    let topic = format!("zigbee2mqtt/{friendly_name}/set");
    tracing::debug!("zigbee2mqtt: publish {topic} ← {payload_str}");

    let _ = cmd_tx
        .send(MqttPublish {
            topic,
            payload: payload_str.into_bytes(),
        })
        .await;
}

/// Translate a HomeCmdr device command to a Zigbee2MQTT MQTT payload string.
fn build_mqtt_command(cmd: &DeviceCommand) -> Option<String> {
    match (cmd.capability.as_str(), cmd.action.as_str()) {
        ("power", "on") => Some(r#"{"state":"ON"}"#.to_string()),
        ("power", "off") => Some(r#"{"state":"OFF"}"#.to_string()),
        ("power", "toggle") => Some(r#"{"state":"TOGGLE"}"#.to_string()),
        ("brightness", "set") => {
            let pct = cmd.value.as_ref()?.as_f64()?;
            let raw = (pct / 100.0 * 254.0).round().min(254.0) as u64;
            Some(format!(r#"{{"brightness":{raw}}}"#))
        }
        ("color_temperature", "set") => {
            let kelvin = cmd.value.as_ref()?.as_f64()?;
            let mireds = if kelvin > 0.0 {
                (1_000_000.0 / kelvin).round() as u64
            } else {
                370 // ~2700 K
            };
            Some(format!(r#"{{"color_temp":{mireds}}}"#))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Kind inference (from Zigbee2MQTT expose types)
// ---------------------------------------------------------------------------

fn infer_kind(definition: &Option<BridgeDefinition>) -> String {
    let def = match definition {
        Some(d) => d,
        None => return "sensor".to_string(),
    };
    let types: Vec<&str> = def.exposes.iter().map(|e| e.expose_type.as_str()).collect();
    if types.contains(&"light") {
        "light".to_string()
    } else if types.contains(&"switch") {
        "switch".to_string()
    } else {
        "sensor".to_string()
    }
}

// ---------------------------------------------------------------------------
// Attribute mapping (Zigbee2MQTT state → HomeCmdr AttributeValue)
// ---------------------------------------------------------------------------

fn build_attributes(
    state: &HashMap<String, Value>,
    definition: &Option<BridgeDefinition>,
) -> Value {
    let mut attrs = serde_json::Map::new();

    // Device metadata from the bridge definition.
    if let Some(def) = definition {
        if !def.model.is_empty() {
            attrs.insert(
                "custom.zigbee2mqtt.model".to_string(),
                Value::String(def.model.clone()),
            );
        }
        if !def.vendor.is_empty() {
            attrs.insert(
                "custom.zigbee2mqtt.vendor".to_string(),
                Value::String(def.vendor.clone()),
            );
        }
        if !def.description.is_empty() {
            attrs.insert(
                "custom.zigbee2mqtt.description".to_string(),
                Value::String(def.description.clone()),
            );
        }
    }

    // Map known state fields to canonical HomeCmdr attribute keys.
    for (key, value) in state {
        match key.as_str() {
            "state" => {
                let on = match value {
                    Value::String(s) => s.eq_ignore_ascii_case("ON"),
                    Value::Bool(b) => *b,
                    _ => false,
                };
                attrs.insert(
                    "power".to_string(),
                    Value::String(if on { "on" } else { "off" }.to_string()),
                );
            }
            "brightness" => {
                if let Some(raw) = value.as_f64() {
                    let normalised = (raw / 254.0 * 100.0).round() as i64;
                    attrs.insert(
                        "brightness".to_string(),
                        serde_json::json!(normalised),
                    );
                }
            }
            "color_temp" => {
                if let Some(mireds) = value.as_f64() {
                    if mireds > 0.0 {
                        let kelvin = (1_000_000.0 / mireds).round() as i64;
                        attrs.insert(
                            "color_temperature".to_string(),
                            serde_json::json!(kelvin),
                        );
                    }
                }
            }
            "temperature" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "temperature_outdoor".to_string(),
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
            other => {
                attrs.insert(format!("custom.zigbee2mqtt.{}", other), value.clone());
            }
        }
    }

    Value::Object(attrs)
}

// ---------------------------------------------------------------------------
// Vendor ID sanitisation
// ---------------------------------------------------------------------------

/// Replace characters that are not safe in a device vendor_id with `_`.
/// Mirrors the logic in the old WASM plugin so device IDs are stable.
fn sanitise_vendor_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_friendly_name_spaces() {
        assert_eq!(sanitise_vendor_id("living room light"), "living_room_light");
    }

    #[test]
    fn sanitise_ieee_address() {
        assert_eq!(
            sanitise_vendor_id("0x00158d0001234567"),
            "0x00158d0001234567"
        );
    }

    #[test]
    fn infer_kind_light() {
        let def = BridgeDefinition {
            exposes: vec![Expose { expose_type: "light".to_string() }],
            ..Default::default()
        };
        assert_eq!(infer_kind(&Some(def)), "light");
    }

    #[test]
    fn infer_kind_switch() {
        let def = BridgeDefinition {
            exposes: vec![Expose { expose_type: "switch".to_string() }],
            ..Default::default()
        };
        assert_eq!(infer_kind(&Some(def)), "switch");
    }

    #[test]
    fn infer_kind_fallback_sensor() {
        let def = BridgeDefinition {
            exposes: vec![Expose { expose_type: "numeric".to_string() }],
            ..Default::default()
        };
        assert_eq!(infer_kind(&Some(def)), "sensor");
    }

    #[test]
    fn power_state_on() {
        let mut s = HashMap::new();
        s.insert("state".to_string(), serde_json::json!("ON"));
        let attrs = build_attributes(&s, &None);
        assert_eq!(attrs["power"], serde_json::json!("on"));
    }

    #[test]
    fn power_state_off() {
        let mut s = HashMap::new();
        s.insert("state".to_string(), serde_json::json!("OFF"));
        let attrs = build_attributes(&s, &None);
        assert_eq!(attrs["power"], serde_json::json!("off"));
    }

    #[test]
    fn brightness_normalisation() {
        let mut s = HashMap::new();
        s.insert("brightness".to_string(), serde_json::json!(127.0_f64));
        let attrs = build_attributes(&s, &None);
        assert_eq!(attrs["brightness"], serde_json::json!(50));
    }

    #[test]
    fn color_temp_mired_to_kelvin() {
        let mut s = HashMap::new();
        s.insert("color_temp".to_string(), serde_json::json!(250.0_f64));
        let attrs = build_attributes(&s, &None);
        assert_eq!(attrs["color_temperature"], serde_json::json!(4000));
    }

    #[test]
    fn mqtt_command_power_on() {
        let cmd = DeviceCommand {
            capability: "power".to_string(),
            action: "on".to_string(),
            value: None,
        };
        assert_eq!(build_mqtt_command(&cmd).as_deref(), Some(r#"{"state":"ON"}"#));
    }

    #[test]
    fn mqtt_command_brightness_set() {
        let cmd = DeviceCommand {
            capability: "brightness".to_string(),
            action: "set".to_string(),
            value: Some(serde_json::json!(50.0)),
        };
        assert_eq!(
            build_mqtt_command(&cmd).as_deref(),
            Some(r#"{"brightness":127}"#)
        );
    }
}
