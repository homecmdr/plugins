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

// ── Config ───────────────────────────────────────────────────────────────────
// Settings read at startup from the HOMECMDR_ADAPTER_CONFIG environment
// variable.  Any field left out will use the default shown below.

/// Adapter configuration, parsed from the JSON in `HOMECMDR_ADAPTER_CONFIG`.
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

// ── Zigbee2MQTT bridge/devices message types ──────────────────────────────────
// Zigbee2MQTT publishes a full device list on `bridge/devices` when it starts
// up and whenever a device joins or leaves the network.  These types mirror
// that JSON so we can deserialise it.

/// One entry in the Zigbee2MQTT `bridge/devices` list.
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

/// Static information about a device from its Zigbee2MQTT definition.
/// Used to infer the device kind and to populate vendor metadata.
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

/// A single capability the device exposes (e.g. `"light"`, `"switch"`,
/// `"numeric"`).  We only look at the type string to infer the device kind.
#[derive(Deserialize, Clone)]
struct Expose {
    #[serde(rename = "type", default)]
    expose_type: String,
}

// ── HomeCmdr ingest API types ─────────────────────────────────────────────────
// The JSON shapes we POST to `POST /ingest/devices` when we have device state
// to report.  One request per adapter, with all updated devices in a flat list.

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
// Events that arrive over the HomeCmdr `/events` WebSocket.  We only act on
// `device.command_dispatched` — everything else is ignored.

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
    /// Hardware transition duration in seconds (Zigbee2MQTT `transition` field).
    #[serde(default)]
    transition_secs: Option<f64>,
}

// ── Outbound MQTT publish ─────────────────────────────────────────────────────
// A message queued by the WebSocket task to be sent out on the MQTT broker.
// Using a channel lets both tasks run independently without blocking each other.

/// A topic + payload pair ready to publish on the MQTT broker.
struct MqttPublish {
    topic: String,
    payload: Vec<u8>,
}

// ── Shared state ──────────────────────────────────────────────────────────────
// Data that both the MQTT task and the WebSocket task need to read or write.
// A mutex ensures they don't step on each other.

#[derive(Default)]
struct SharedState {
    /// vendor_id → original friendly_name (for reversing sanitisation in commands)
    vendor_id_map: HashMap<String, String>,
    /// friendly_name → device kind string (populated from bridge/devices)
    device_kinds: HashMap<String, String>,
    /// friendly_name → bridge definition (for attribute metadata)
    device_definitions: HashMap<String, BridgeDefinition>,
}

// ── Entry point ───────────────────────────────────────────────────────────────
// Reads environment variables, parses config, then launches the MQTT and
// WebSocket tasks in parallel.  Both tasks run indefinitely; the program only
// exits if the adapter is disabled in config.

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

// ── MQTT task ─────────────────────────────────────────────────────────────────
// Connects to the MQTT broker, subscribes to all Zigbee2MQTT topics, and
// dispatches each incoming message to the right handler.  Also drains the
// outbound command channel so API commands get published back to devices.
// rumqttc reconnects automatically if the broker drops the connection.

/// Run the MQTT event loop forever.
///
/// Handles incoming device state messages from the broker and outgoing command
/// publishes from the WebSocket task.  Never returns unless the process exits.
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

// ── bridge/devices handler ────────────────────────────────────────────────────
// Zigbee2MQTT sends a full snapshot of every paired device on this topic.
// We use it to rebuild our name→kind and name→definition caches, then ingest
// all devices so the API has up-to-date state even before any live changes.

/// Parse the full device list from `bridge/devices` and push every device to
/// the HomeCmdr API.  Also refreshes the in-memory name and kind caches used
/// later by per-device state messages and command dispatch.
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

            let (attributes, metadata) = build_attributes(&d.state, &d.definition);
            ingest.push(IngestDevice { vendor_id, kind, attributes, metadata });
        }
    }

    if !ingest.is_empty() {
        post_ingest(http, api_url, api_token, ingest).await;
    }
}

// ── Per-device state message handler ─────────────────────────────────────────
// Called for every MQTT message that looks like a live device state update —
// a top-level `{base_topic}/{name}` message carrying a JSON object.

/// Parse a single device's live state update and forward it to the API.
///
/// Skips non-JSON payloads (e.g. availability strings like `"online"`).
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

    let (attributes, metadata) = build_attributes(&state_map, &definition);
    let devices = vec![IngestDevice { vendor_id, kind, attributes, metadata }];
    post_ingest(http, api_url, api_token, devices).await;
}

// ── HTTP ingest helper ────────────────────────────────────────────────────────
// Sends a batch of device updates to the HomeCmdr API.  Logs a warning on
// failure but never panics — a missed update will be corrected on the next
// publish from the device.

/// POST a batch of device updates to `POST /ingest/devices`.
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

// ── WebSocket task ────────────────────────────────────────────────────────────
// Connects to the HomeCmdr API's `/events` WebSocket and listens for
// `device.command_dispatched` events.  Reconnects with exponential back-off
// so short outages don't require a restart.

/// Maintain the WebSocket connection to the HomeCmdr API.
///
/// Reconnects automatically after any failure, backing off from 1 s up to
/// 60 s between attempts.  Never returns unless the process exits.
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

/// Handle one WebSocket connection lifetime.
///
/// Returns `Ok(())` when the server closes the connection cleanly, or an
/// error if something goes wrong.  The caller (`run_websocket`) will reconnect.
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

// ── Command dispatch ──────────────────────────────────────────────────────────
// Translates a `device.command_dispatched` WebSocket event into a Zigbee2MQTT
// MQTT payload and queues it for publishing.  Commands for devices that don't
// belong to this adapter are silently ignored.

/// Convert a `device.command_dispatched` event into an MQTT publish.
///
/// Looks up the device's original friendly name (to reverse the ID
/// sanitisation), builds the Zigbee2MQTT payload string, and sends it to the
/// MQTT task via the command channel.
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
///
/// Returns `None` for any capability/action pair we don't handle yet — the
/// caller will log a debug message and skip the publish.
fn build_mqtt_command(cmd: &DeviceCommand) -> Option<String> {
    match (cmd.capability.as_str(), cmd.action.as_str()) {
        ("state", "on") => Some(r#"{"state":"ON"}"#.to_string()),
        ("state", "off") => Some(r#"{"state":"OFF"}"#.to_string()),
        ("state", "toggle") => Some(r#"{"state":"TOGGLE"}"#.to_string()),
        ("brightness", "set") => {
            let pct = cmd.value.as_ref()?.as_f64()?;
            let raw = (pct / 100.0 * 254.0).round().min(254.0) as u64;
            if let Some(secs) = cmd.transition_secs {
                Some(format!(r#"{{"brightness":{raw},"transition":{secs}}}"#))
            } else {
                Some(format!(r#"{{"brightness":{raw}}}"#))
            }
        }
        ("color_temperature", "set") => {
            // Value arrives as either a plain number (legacy) or a
            // Measurement object {"value": <kelvin>, "unit": "kelvin"/"mireds"}.
            let v = cmd.value.as_ref()?;
            let (kelvin, mireds_override) = if let Some(obj) = v.as_object() {
                let raw = obj.get("value")?.as_f64()?;
                let unit = obj.get("unit").and_then(|u| u.as_str()).unwrap_or("kelvin");
                if unit == "mireds" {
                    // Already mireds — use directly.
                    let mireds = raw.round() as u64;
                    let k = if raw > 0.0 { 1_000_000.0 / raw } else { 0.0 };
                    (k, Some(mireds))
                } else {
                    (raw, None)
                }
            } else {
                (v.as_f64()?, None)
            };
            let mireds = mireds_override.unwrap_or_else(|| {
                if kelvin > 0.0 {
                    (1_000_000.0 / kelvin).round() as u64
                } else {
                    370 // ~2700 K
                }
            });
            if let Some(secs) = cmd.transition_secs {
                Some(format!(r#"{{"color_temp":{mireds},"transition":{secs}}}"#))
            } else {
                Some(format!(r#"{{"color_temp":{mireds}}}"#))
            }
        }
        ("color_xy", "set") => {
            let obj = cmd.value.as_ref()?.as_object()?;
            let x = obj.get("x")?.as_f64()?;
            let y = obj.get("y")?.as_f64()?;
            if let Some(secs) = cmd.transition_secs {
                Some(format!(r#"{{"color":{{"x":{x},"y":{y}}},"transition":{secs}}}"#))
            } else {
                Some(format!(r#"{{"color":{{"x":{x},"y":{y}}}}}"#))
            }
        }
        ("color_hs", "set") => {
            let obj = cmd.value.as_ref()?.as_object()?;
            let hue = obj.get("hue")?.as_i64()?;
            let sat = obj.get("saturation")?.as_i64()?;
            if let Some(secs) = cmd.transition_secs {
                Some(format!(
                    r#"{{"color":{{"hue":{hue},"saturation":{sat}}},"transition":{secs}}}"#
                ))
            } else {
                Some(format!(r#"{{"color":{{"hue":{hue},"saturation":{sat}}}}}"#))
            }
        }
        ("color_rgb", "set") => {
            let obj = cmd.value.as_ref()?.as_object()?;
            let r = obj.get("r")?.as_i64()?;
            let g = obj.get("g")?.as_i64()?;
            let b = obj.get("b")?.as_i64()?;
            if let Some(secs) = cmd.transition_secs {
                Some(format!(
                    r#"{{"color":{{"r":{r},"g":{g},"b":{b}}},"transition":{secs}}}"#
                ))
            } else {
                Some(format!(r#"{{"color":{{"r":{r},"g":{g},"b":{b}}}}}"#))
            }
        }
        _ => None,
    }
}

// ── Kind inference ────────────────────────────────────────────────────────────
// Picks the HomeCmdr device kind (light / switch / sensor) based on what
// capabilities Zigbee2MQTT says the device exposes.  Defaults to "sensor" when
// we can't tell.

/// Pick a HomeCmdr device kind from the Zigbee2MQTT expose list.
///
/// "light" and "switch" are matched by name; everything else — sensors,
/// buttons, locks, and so on — falls back to "sensor".
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

// ── Attribute mapping ─────────────────────────────────────────────────────────
// The core translation layer.  Takes the raw key/value pairs that Zigbee2MQTT
// publishes and converts them into the attribute shapes HomeCmdr expects.
// Keys with no canonical match fall through to `custom.zigbee2mqtt.*` so no
// data is silently dropped.

/// Map a Zigbee2MQTT device state payload to HomeCmdr canonical attributes and
/// vendor metadata.
///
/// Returns `(attributes, metadata)`:
/// - `attributes` — canonical capability keys plus `custom.zigbee2mqtt.*` for
///   anything without a canonical home
/// - `metadata`   — opaque device info (model, vendor, description) stored by
///   the API under `metadata.vendor_specific`
fn build_attributes(
    state: &HashMap<String, Value>,
    definition: &Option<BridgeDefinition>,
) -> (Value, Value) {
    let mut attrs = serde_json::Map::new();
    let mut metadata = serde_json::Map::new();

    // Bridge definition fields are descriptive metadata — not device state.
    if let Some(def) = definition {
        if !def.model.is_empty() {
            metadata.insert("model".to_string(), Value::String(def.model.clone()));
        }
        if !def.vendor.is_empty() {
            metadata.insert("vendor".to_string(), Value::String(def.vendor.clone()));
        }
        if !def.description.is_empty() {
            metadata.insert("description".to_string(), Value::String(def.description.clone()));
        }
    }

    // Map known state fields to canonical HomeCmdr attribute keys.
    for (key, value) in state {
        match key.as_str() {
            // ── On/Off state ───────────────────────────────────────────────
            "state" => {
                let on = match value {
                    Value::String(s) => s.eq_ignore_ascii_case("ON"),
                    Value::Bool(b) => *b,
                    _ => false,
                };
                attrs.insert(
                    "state".to_string(),
                    Value::String(if on { "on" } else { "off" }.to_string()),
                );
            }

            // ── Lighting ───────────────────────────────────────────────────
            "brightness" => {
                if let Some(raw) = value.as_f64() {
                    let normalised = (raw / 254.0 * 100.0).round() as i64;
                    attrs.insert("brightness".to_string(), serde_json::json!(normalised));
                }
            }
            "color_temp" => {
                if let Some(mireds) = value.as_f64() {
                    if mireds > 0.0 {
                        let kelvin = (1_000_000.0 / mireds).round() as i64;
                        attrs.insert(
                            "color_temperature".to_string(),
                            serde_json::json!({ "value": kelvin, "unit": "kelvin" }),
                        );
                    }
                }
            }
            "color_mode" => {
                // Deliberately discarded. Z2M's color_mode merely reflects the type
                // of the last command sent (xy/hs/color_temp) — it is not a device
                // capability descriptor and is misleading as API state.
            }
            "color" => {
                // Z2M sometimes sends all representations simultaneously, e.g.:
                // { "x": 0.5056, "y": 0.4152, "hue": 27, "saturation": 92 }
                // Use independent `if` blocks (not `else if`) so every sub-format
                // present in the object is mapped — none are silently dropped.
                if let Value::Object(obj) = value {
                    let mut mapped = false;
                    if obj.contains_key("x") && obj.contains_key("y") {
                        attrs.insert(
                            "color_xy".to_string(),
                            serde_json::json!({
                                "x": obj.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                                "y": obj.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
                            }),
                        );
                        mapped = true;
                    }
                    if obj.contains_key("hue") && obj.contains_key("saturation") {
                        attrs.insert(
                            "color_hs".to_string(),
                            serde_json::json!({
                                "hue": obj.get("hue").and_then(|v| v.as_i64()).unwrap_or(0),
                                "saturation": obj.get("saturation").and_then(|v| v.as_i64()).unwrap_or(0),
                            }),
                        );
                        mapped = true;
                    }
                    if !mapped {
                        attrs.insert(format!("custom.zigbee2mqtt.{key}"), value.clone());
                    }
                } else {
                    attrs.insert(format!("custom.zigbee2mqtt.{key}"), value.clone());
                }
            }
            "led_indication" => {
                if let Some(b) = value.as_bool() {
                    attrs.insert("led_indication".to_string(), Value::Bool(b));
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
            // illuminance_lux is the human-readable lux value; raw illuminance is vendor-specific.
            "illuminance_lux" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert("illuminance".to_string(), serde_json::json!(v.round() as i64));
                }
            }
            "occupancy" => {
                if let Some(b) = value.as_bool() {
                    attrs.insert(
                        "occupancy".to_string(),
                        Value::String(if b { "occupied" } else { "unoccupied" }.to_string()),
                    );
                }
            }
            "contact" => {
                // Z2M: true = contact present = door/window closed.
                if let Some(b) = value.as_bool() {
                    attrs.insert(
                        "contact".to_string(),
                        Value::String(if b { "closed" } else { "open" }.to_string()),
                    );
                }
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
            // Z2M uses "power" for instantaneous watts.
            "power" => {
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
            "energy_today" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "energy_today".to_string(),
                        serde_json::json!({ "value": v, "unit": "kWh", "period": "today" }),
                    );
                }
            }
            "energy_yesterday" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "energy_yesterday".to_string(),
                        serde_json::json!({ "value": v, "unit": "kWh", "period": "yesterday" }),
                    );
                }
            }
            "energy_month" => {
                if let Some(v) = value.as_f64() {
                    attrs.insert(
                        "energy_month".to_string(),
                        serde_json::json!({ "value": v, "unit": "kWh", "period": "month" }),
                    );
                }
            }

            // ── Fallthrough ────────────────────────────────────────────────
            other => {
                attrs.insert(format!("custom.zigbee2mqtt.{other}"), value.clone());
            }
        }
    }

    (Value::Object(attrs), Value::Object(metadata))
}

// ── Vendor ID sanitisation ────────────────────────────────────────────────────
// Zigbee2MQTT friendly names can contain spaces and other special characters.
// This converts them into a safe identifier HomeCmdr can use as a stable device
// ID, matching the sanitisation the old WASM plugin used.

/// Replace any character that isn't alphanumeric, `-`, or `_` with `_`.
///
/// Zigbee2MQTT friendly names are human-readable (e.g. "Living Room Light")
/// but HomeCmdr device IDs must be stable URL-safe strings.  This function
/// produces the same result the legacy WASM plugin did, so existing devices
/// keep their IDs after the migration.
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

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: run build_attributes and return only the attributes Value.
    fn attrs(state: &HashMap<String, Value>) -> Value {
        build_attributes(state, &None).0
    }

    // Helper: run build_attributes and return only the metadata Value.
    fn meta(state: &HashMap<String, Value>, def: &Option<BridgeDefinition>) -> Value {
        build_attributes(state, def).1
    }

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

    // ── On/Off state ──────────────────────────────────────────────────────────

    #[test]
    fn state_on() {
        let mut s = HashMap::new();
        s.insert("state".to_string(), serde_json::json!("ON"));
        assert_eq!(attrs(&s)["state"], serde_json::json!("on"));
    }

    #[test]
    fn state_off() {
        let mut s = HashMap::new();
        s.insert("state".to_string(), serde_json::json!("OFF"));
        assert_eq!(attrs(&s)["state"], serde_json::json!("off"));
    }

    // ── Lighting ──────────────────────────────────────────────────────────────

    #[test]
    fn brightness_normalisation() {
        let mut s = HashMap::new();
        s.insert("brightness".to_string(), serde_json::json!(127.0_f64));
        assert_eq!(attrs(&s)["brightness"], serde_json::json!(50));
    }

    #[test]
    fn color_temp_mired_to_kelvin() {
        let mut s = HashMap::new();
        s.insert("color_temp".to_string(), serde_json::json!(250.0_f64));
        assert_eq!(
            attrs(&s)["color_temperature"],
            serde_json::json!({ "value": 4000_i64, "unit": "kelvin" })
        );
    }

    #[test]
    fn color_mode_mapped() {
        let mut s = HashMap::new();
        s.insert("color_mode".to_string(), serde_json::json!("color_temperature"));
        assert_eq!(attrs(&s)["color_mode"], serde_json::json!("color_temp"));
    }

    #[test]
    fn color_mode_passthrough() {
        let mut s = HashMap::new();
        s.insert("color_mode".to_string(), serde_json::json!("xy"));
        assert_eq!(attrs(&s)["color_mode"], serde_json::json!("xy"));
    }

    #[test]
    fn color_xy() {
        let mut s = HashMap::new();
        s.insert("color".to_string(), serde_json::json!({ "x": 0.3, "y": 0.6 }));
        assert_eq!(attrs(&s)["color_xy"], serde_json::json!({ "x": 0.3, "y": 0.6 }));
    }

    #[test]
    fn color_hs() {
        let mut s = HashMap::new();
        s.insert("color".to_string(), serde_json::json!({ "hue": 120, "saturation": 80 }));
        assert_eq!(attrs(&s)["color_hs"], serde_json::json!({ "hue": 120, "saturation": 80 }));
    }

    #[test]
    fn led_indication() {
        let mut s = HashMap::new();
        s.insert("led_indication".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["led_indication"], serde_json::json!(true));
    }

    // ── Sensors ───────────────────────────────────────────────────────────────

    #[test]
    fn temperature_sensor() {
        let mut s = HashMap::new();
        s.insert("temperature".to_string(), serde_json::json!(21.5_f64));
        let a = attrs(&s);
        assert_eq!(a["temperature"], serde_json::json!({ "value": 21.5, "unit": "celsius" }));
        assert!(a.get("temperature_outdoor").is_none());
    }

    #[test]
    fn humidity_sensor() {
        let mut s = HashMap::new();
        s.insert("humidity".to_string(), serde_json::json!(55.0_f64));
        assert_eq!(
            attrs(&s)["humidity"],
            serde_json::json!({ "value": 55.0, "unit": "percent" })
        );
    }

    #[test]
    fn pressure_sensor() {
        let mut s = HashMap::new();
        s.insert("pressure".to_string(), serde_json::json!(1013.0_f64));
        assert_eq!(
            attrs(&s)["pressure"],
            serde_json::json!({ "value": 1013.0, "unit": "hPa" })
        );
    }

    #[test]
    fn co2_sensor() {
        let mut s = HashMap::new();
        s.insert("co2".to_string(), serde_json::json!(850.0_f64));
        assert_eq!(attrs(&s)["co2"], serde_json::json!({ "value": 850.0, "unit": "ppm" }));
    }

    #[test]
    fn battery_percentage() {
        let mut s = HashMap::new();
        s.insert("battery".to_string(), serde_json::json!(87.0_f64));
        assert_eq!(attrs(&s)["battery"], serde_json::json!(87_i64));
    }

    #[test]
    fn illuminance_lux_to_canonical() {
        let mut s = HashMap::new();
        s.insert("illuminance_lux".to_string(), serde_json::json!(450.0_f64));
        let a = attrs(&s);
        assert_eq!(a["illuminance"], serde_json::json!(450_i64));
        assert!(a.get("illuminance_lux").is_none());
    }

    #[test]
    fn occupancy_detected() {
        let mut s = HashMap::new();
        s.insert("occupancy".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["occupancy"], serde_json::json!("occupied"));
    }

    #[test]
    fn occupancy_clear() {
        let mut s = HashMap::new();
        s.insert("occupancy".to_string(), serde_json::json!(false));
        assert_eq!(attrs(&s)["occupancy"], serde_json::json!("unoccupied"));
    }

    #[test]
    fn contact_closed() {
        let mut s = HashMap::new();
        s.insert("contact".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["contact"], serde_json::json!("closed"));
    }

    #[test]
    fn contact_open() {
        let mut s = HashMap::new();
        s.insert("contact".to_string(), serde_json::json!(false));
        assert_eq!(attrs(&s)["contact"], serde_json::json!("open"));
    }

    #[test]
    fn smoke_detected() {
        let mut s = HashMap::new();
        s.insert("smoke".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["smoke"], serde_json::json!(true));
    }

    #[test]
    fn water_leak_detected() {
        let mut s = HashMap::new();
        s.insert("water_leak".to_string(), serde_json::json!(true));
        assert_eq!(attrs(&s)["water_leak"], serde_json::json!(true));
    }

    // ── Energy ────────────────────────────────────────────────────────────────

    #[test]
    fn power_consumption() {
        let mut s = HashMap::new();
        s.insert("power".to_string(), serde_json::json!(120.5_f64));
        assert_eq!(
            attrs(&s)["power_consumption"],
            serde_json::json!({ "value": 120.5, "unit": "W" })
        );
    }

    #[test]
    fn voltage_measurement() {
        let mut s = HashMap::new();
        s.insert("voltage".to_string(), serde_json::json!(230.0_f64));
        assert_eq!(
            attrs(&s)["voltage"],
            serde_json::json!({ "value": 230.0, "unit": "V" })
        );
    }

    #[test]
    fn current_measurement() {
        let mut s = HashMap::new();
        s.insert("current".to_string(), serde_json::json!(0.52_f64));
        assert_eq!(
            attrs(&s)["current"],
            serde_json::json!({ "value": 0.52, "unit": "A" })
        );
    }

    #[test]
    fn energy_total() {
        let mut s = HashMap::new();
        s.insert("energy".to_string(), serde_json::json!(12.3_f64));
        assert_eq!(
            attrs(&s)["energy_total"],
            serde_json::json!({ "value": 12.3, "unit": "kWh", "period": "lifetime" })
        );
    }

    #[test]
    fn energy_today() {
        let mut s = HashMap::new();
        s.insert("energy_today".to_string(), serde_json::json!(0.5_f64));
        assert_eq!(
            attrs(&s)["energy_today"],
            serde_json::json!({ "value": 0.5, "unit": "kWh", "period": "today" })
        );
    }

    #[test]
    fn energy_yesterday() {
        let mut s = HashMap::new();
        s.insert("energy_yesterday".to_string(), serde_json::json!(1.2_f64));
        assert_eq!(
            attrs(&s)["energy_yesterday"],
            serde_json::json!({ "value": 1.2, "unit": "kWh", "period": "yesterday" })
        );
    }

    #[test]
    fn energy_month() {
        let mut s = HashMap::new();
        s.insert("energy_month".to_string(), serde_json::json!(0.04_f64));
        assert_eq!(
            attrs(&s)["energy_month"],
            serde_json::json!({ "value": 0.04, "unit": "kWh", "period": "month" })
        );
    }

    // ── Metadata ──────────────────────────────────────────────────────────────

    #[test]
    fn definition_fields_go_to_metadata_not_attributes() {
        let def = Some(BridgeDefinition {
            model: "E1743".to_string(),
            vendor: "IKEA".to_string(),
            description: "On/off switch".to_string(),
            ..Default::default()
        });
        let s = HashMap::new();
        let m = meta(&s, &def);
        let a = attrs(&s);
        // Not in attributes
        assert!(a.get("custom.zigbee2mqtt.model").is_none());
        assert!(a.get("custom.zigbee2mqtt.vendor").is_none());
        assert!(a.get("custom.zigbee2mqtt.description").is_none());
        // Present in metadata
        assert_eq!(m["model"], serde_json::json!("E1743"));
        assert_eq!(m["vendor"], serde_json::json!("IKEA"));
        assert_eq!(m["description"], serde_json::json!("On/off switch"));
    }

    // ── Fallthrough ───────────────────────────────────────────────────────────

    #[test]
    fn unknown_key_falls_through_to_custom() {
        let mut s = HashMap::new();
        s.insert("action".to_string(), serde_json::json!("single"));
        assert_eq!(
            attrs(&s)["custom.zigbee2mqtt.action"],
            serde_json::json!("single")
        );
    }

    // ── Commands ──────────────────────────────────────────────────────────────

    #[test]
    fn mqtt_command_state_on() {
        let cmd = DeviceCommand {
            capability: "state".to_string(),
            action: "on".to_string(),
            value: None,
            transition_secs: None,
        };
        assert_eq!(build_mqtt_command(&cmd).as_deref(), Some(r#"{"state":"ON"}"#));
    }

    #[test]
    fn mqtt_command_brightness_set() {
        let cmd = DeviceCommand {
            capability: "brightness".to_string(),
            action: "set".to_string(),
            value: Some(serde_json::json!(50.0)),
            transition_secs: None,
        };
        assert_eq!(
            build_mqtt_command(&cmd).as_deref(),
            Some(r#"{"brightness":127}"#)
        );
    }

    #[test]
    fn mqtt_command_brightness_set_with_transition() {
        let cmd = DeviceCommand {
            capability: "brightness".to_string(),
            action: "set".to_string(),
            value: Some(serde_json::json!(70.0)),
            transition_secs: Some(30.0),
        };
        assert_eq!(
            build_mqtt_command(&cmd).as_deref(),
            Some(r#"{"brightness":178,"transition":30}"#)
        );
    }

    #[test]
    fn mqtt_command_color_temp_with_transition() {
        let cmd = DeviceCommand {
            capability: "color_temperature".to_string(),
            action: "set".to_string(),
            value: Some(serde_json::json!(2700.0)),
            transition_secs: Some(10.0),
        };
        let result = build_mqtt_command(&cmd).unwrap();
        assert!(result.contains("\"transition\":10"), "got: {result}");
        assert!(result.contains("\"color_temp\":"), "got: {result}");
    }
}
