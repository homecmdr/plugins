//! Zigbee2MQTT WASM plugin for HomeCmdr.
//!
//! Polls the Zigbee2MQTT frontend HTTP API for device state.  Requires
//! Zigbee2MQTT 1.36+ with the built-in web frontend enabled (the default).
//! The API is available at `http://<host>:8080/api/devices`.
//!
//! Each Zigbee end-device or router is exposed as a HomeCmdr device.  The
//! Coordinator is skipped.  Device kind is inferred from the `definition`
//! expose types: `light`, `switch`, or `sensor`.
//!
//! Known state fields are mapped to canonical HomeCmdr attribute keys.
//! Unknown fields are stored under `custom.zigbee2mqtt.<field>`.
//!
//! State mutations use POST to the Zigbee2MQTT device-state endpoint
//! (`POST /api/device/state`, Zigbee2MQTT ≥ 1.36).
//!
//! Build:
//!   cargo build --release
//!   cp target/wasm32-wasip2/release/plugin_zigbee2mqtt_wasm.wasm \
//!      <workspace>/config/plugins/zigbee2mqtt.wasm

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
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    /// HTTP base URL of the Zigbee2MQTT frontend (default: http://127.0.0.1:8080).
    #[serde(default = "default_base_url")]
    base_url: String,
}

fn default_true() -> bool {
    true
}
fn default_base_url() -> String {
    "http://127.0.0.1:8080".to_string()
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

struct Z2mState {
    base_url: String,
    /// vendor_id (sanitised friendly_name) → original friendly_name
    /// Populated on each poll() so command() can resolve the API identifier.
    device_map: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Zigbee2MQTT API response types
// ---------------------------------------------------------------------------

/// A single device entry returned by `GET /api/devices`.
#[derive(Debug, Deserialize)]
struct Z2mDevice {
    /// IEEE 802.15.4 address, e.g. "0x00158d0001234567".
    #[allow(dead_code)]
    ieee_address: String,
    /// Zigbee role: "Coordinator" | "Router" | "EndDevice" | "Unknown".
    #[serde(rename = "type", default)]
    device_type: String,
    /// User-assigned name, e.g. "living_room_light".
    #[serde(default)]
    friendly_name: String,
    /// Device capabilities definition (optional — not all devices have it).
    #[serde(default)]
    definition: Option<Z2mDefinition>,
    /// Current device state as a free-form JSON map.
    #[serde(default)]
    state: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct Z2mDefinition {
    #[serde(default)]
    model: String,
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    exposes: Vec<Z2mExpose>,
}

/// A single expose entry — we only care about the `type` field for kind inference.
#[derive(Debug, Deserialize)]
struct Z2mExpose {
    #[serde(rename = "type", default)]
    expose_type: String,
}

// ---------------------------------------------------------------------------
// Device command shape
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeviceCommand {
    capability: String,
    action: String,
    value: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Plugin state
// ---------------------------------------------------------------------------

thread_local! {
    static STATE: std::cell::RefCell<Option<Z2mState>> = std::cell::RefCell::new(None);
}

// ---------------------------------------------------------------------------
// Guest implementation
// ---------------------------------------------------------------------------

struct Zigbee2MqttPlugin;

impl Guest for Zigbee2MqttPlugin {
    fn name() -> String {
        "zigbee2mqtt".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse zigbee2mqtt config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "zigbee2mqtt plugin is disabled by config");
            return Ok(());
        }

        host_log::log(
            "info",
            &format!("zigbee2mqtt initialised ({})", config.base_url),
        );
        STATE.with(|s| {
            *s.borrow_mut() = Some(Z2mState {
                base_url: config.base_url,
                device_map: HashMap::new(),
            });
        });
        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        let base_url = STATE
            .with(|s| s.borrow().as_ref().map(|st| st.base_url.clone()))
            .ok_or("zigbee2mqtt not initialised or disabled")?;

        let url = format!("{}/api/devices", base_url.trim_end_matches('/'));
        let body = host_http::get(&url)
            .map_err(|e| format!("zigbee2mqtt HTTP GET failed: {e}"))?;

        let devices: Vec<Z2mDevice> =
            serde_json::from_str(&body).map_err(|e| format!("zigbee2mqtt parse error: {e}"))?;

        let mut updates = Vec::new();
        let mut new_device_map: HashMap<String, String> = HashMap::new();

        for device in &devices {
            // Skip the Zigbee coordinator — it's infrastructure, not a device.
            if device.device_type.eq_ignore_ascii_case("Coordinator") {
                continue;
            }

            let kind = infer_kind(&device.definition);

            // Derive vendor_id from friendly_name (preferred) or ieee_address.
            let source_name = if !device.friendly_name.is_empty() {
                &device.friendly_name
            } else {
                &device.ieee_address
            };
            let vendor_id = sanitise_vendor_id(source_name);
            // Record the mapping so command() can find the original name.
            new_device_map.insert(vendor_id.clone(), source_name.to_string());

            let attributes_json = build_attributes(&device.state, &device.definition);

            updates.push(DeviceUpdate {
                vendor_id,
                kind,
                attributes_json,
            });
        }

        // Persist updated device map.
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.device_map = new_device_map;
            }
        });

        Ok(updates)
    }

    fn command(device_id: String, command_json: String) -> Result<CommandResult, String> {
        if !device_id.starts_with("zigbee2mqtt:") {
            return Ok(CommandResult {
                handled: false,
                error: None,
            });
        }

        let (base_url, device_map) = STATE
            .with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|st| (st.base_url.clone(), st.device_map.clone()))
            })
            .ok_or("zigbee2mqtt not initialised or disabled")?;

        // Strip the adapter prefix to get the vendor_id.
        let vendor_id = device_id
            .strip_prefix("zigbee2mqtt:")
            .unwrap_or(&device_id);

        // Resolve the original friendly_name for the API call.
        let friendly_name = device_map
            .get(vendor_id)
            .cloned()
            .unwrap_or_else(|| vendor_id.to_string());

        let cmd: DeviceCommand = serde_json::from_str(&command_json)
            .map_err(|e| format!("failed to parse command: {e}"))?;

        // Build the Zigbee2MQTT state payload from the capability/action.
        // These mirror what would be published to `zigbee2mqtt/<name>/set`.
        let z2m_state = match (cmd.capability.as_str(), cmd.action.as_str()) {
            ("power", "on")     => serde_json::json!({ "state": "ON" }),
            ("power", "off")    => serde_json::json!({ "state": "OFF" }),
            ("power", "toggle") => serde_json::json!({ "state": "TOGGLE" }),
            ("brightness", "set") => {
                let pct = cmd
                    .value
                    .as_ref()
                    .and_then(|v| v.as_f64())
                    .ok_or("brightness/set requires a numeric value (0–100)")?;
                // Normalise from 0–100 % to Zigbee 0–254 range.
                let raw = (pct / 100.0 * 254.0).round() as u64;
                serde_json::json!({ "brightness": raw.min(254) })
            }
            ("color_temperature", "set") => {
                let kelvin = cmd
                    .value
                    .as_ref()
                    .and_then(|v| v.as_f64())
                    .ok_or("color_temperature/set requires a numeric kelvin value")?;
                // Convert kelvin → mireds.
                let mireds = if kelvin > 0.0 {
                    (1_000_000.0 / kelvin).round() as u64
                } else {
                    370 // ~2700 K
                };
                serde_json::json!({ "color_temp": mireds })
            }
            _ => return Ok(CommandResult { handled: false, error: None }),
        };

        // POST to /api/device/state  (Zigbee2MQTT ≥ 1.36 REST API).
        // Body: {"id": "<friendly_name>", "state": <z2m_state>}
        let api_body = serde_json::json!({
            "id":    friendly_name,
            "state": z2m_state,
        })
        .to_string();

        let state_url = format!("{}/api/device/state", base_url.trim_end_matches('/'));
        if let Err(e) = host_http::post(&state_url, "application/json", &api_body) {
            return Ok(CommandResult {
                handled: true,
                error: Some(format!("zigbee2mqtt POST failed: {e}")),
            });
        }

        host_log::log(
            "debug",
            &format!("zigbee2mqtt: state update sent for '{}'", friendly_name),
        );

        Ok(CommandResult { handled: true, error: None })
    }
}

export!(Zigbee2MqttPlugin);

// ---------------------------------------------------------------------------
// Device kind inference
// ---------------------------------------------------------------------------

/// Infer a HomeCmdr device kind from the Zigbee2MQTT expose types.
/// Precedence: light > switch > sensor.
fn infer_kind(definition: &Option<Z2mDefinition>) -> String {
    let def = match definition {
        Some(d) => d,
        None => return "sensor".to_string(),
    };

    let expose_types: Vec<&str> = def.exposes.iter().map(|e| e.expose_type.as_str()).collect();

    if expose_types.contains(&"light") {
        "light".to_string()
    } else if expose_types.contains(&"switch") {
        "switch".to_string()
    } else {
        "sensor".to_string()
    }
}

// ---------------------------------------------------------------------------
// Attribute mapping
// ---------------------------------------------------------------------------

/// Map a Zigbee2MQTT device state + definition to a HomeCmdr attributes JSON.
fn build_attributes(
    state: &HashMap<String, serde_json::Value>,
    definition: &Option<Z2mDefinition>,
) -> String {
    let mut attrs = serde_json::Map::new();

    // Include device metadata from definition if present.
    if let Some(def) = definition {
        if !def.model.is_empty() {
            attrs.insert(
                "custom.zigbee2mqtt.model".to_string(),
                serde_json::Value::String(def.model.clone()),
            );
        }
        if !def.vendor.is_empty() {
            attrs.insert(
                "custom.zigbee2mqtt.vendor".to_string(),
                serde_json::Value::String(def.vendor.clone()),
            );
        }
        if !def.description.is_empty() {
            attrs.insert(
                "custom.zigbee2mqtt.description".to_string(),
                serde_json::Value::String(def.description.clone()),
            );
        }
    }

    // Map state fields.
    for (key, value) in state {
        match key.as_str() {
            // ---- Power / switch ----
            "state" => {
                let on = match value {
                    serde_json::Value::String(s) => s.eq_ignore_ascii_case("ON"),
                    serde_json::Value::Bool(b) => *b,
                    _ => false,
                };
                attrs.insert("power".to_string(), serde_json::Value::Bool(on));
            }

            // ---- Light ----
            "brightness" => {
                if let Some(raw) = value.as_f64() {
                    let normalised = (raw / 254.0 * 100.0).round() as i64;
                    attrs.insert(
                        "brightness".to_string(),
                        serde_json::Value::Number(normalised.into()),
                    );
                }
            }
            "color_temp" => {
                if let Some(mireds) = value.as_f64() {
                    if mireds > 0.0 {
                        let kelvin = (1_000_000.0 / mireds).round() as i64;
                        attrs.insert(
                            "color_temperature".to_string(),
                            serde_json::Value::Number(kelvin.into()),
                        );
                    }
                }
            }

            // ---- Climate sensors ----
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

            // ---- Everything else → custom namespace ----
            other => {
                attrs.insert(format!("custom.zigbee2mqtt.{}", other), value.clone());
            }
        }
    }

    serde_json::Value::Object(attrs).to_string()
}

// ---------------------------------------------------------------------------
// Vendor ID sanitisation
// ---------------------------------------------------------------------------

/// Replace characters that are not safe in a device vendor_id with `_`.
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
    fn sanitise_ieee_address() {
        assert_eq!(
            sanitise_vendor_id("0x00158d0001234567"),
            "0x00158d0001234567"
        );
    }

    #[test]
    fn sanitise_friendly_name_with_spaces() {
        assert_eq!(sanitise_vendor_id("living room light"), "living_room_light");
    }

    #[test]
    fn power_state_on() {
        let mut state = HashMap::new();
        state.insert("state".to_string(), serde_json::json!("ON"));
        let attrs = build_attributes(&state, &None);
        let parsed: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert_eq!(parsed["power"], serde_json::json!(true));
    }

    #[test]
    fn power_state_off() {
        let mut state = HashMap::new();
        state.insert("state".to_string(), serde_json::json!("OFF"));
        let attrs = build_attributes(&state, &None);
        let parsed: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert_eq!(parsed["power"], serde_json::json!(false));
    }

    #[test]
    fn brightness_normalisation() {
        let mut state = HashMap::new();
        state.insert("brightness".to_string(), serde_json::json!(127.0_f64));
        let attrs = build_attributes(&state, &None);
        let parsed: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert_eq!(parsed["brightness"], serde_json::json!(50));
    }

    #[test]
    fn color_temp_mired_to_kelvin() {
        let mut state = HashMap::new();
        state.insert("color_temp".to_string(), serde_json::json!(250.0_f64));
        let attrs = build_attributes(&state, &None);
        let parsed: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert_eq!(parsed["color_temperature"], serde_json::json!(4000));
    }

    #[test]
    fn temperature_attribute() {
        let mut state = HashMap::new();
        state.insert("temperature".to_string(), serde_json::json!(22.5_f64));
        let attrs = build_attributes(&state, &None);
        let parsed: serde_json::Value = serde_json::from_str(&attrs).unwrap();
        assert_eq!(
            parsed["temperature_outdoor"]["value"],
            serde_json::json!(22.5)
        );
        assert_eq!(
            parsed["temperature_outdoor"]["unit"],
            serde_json::json!("celsius")
        );
    }

    #[test]
    fn infer_kind_light() {
        let def = Z2mDefinition {
            exposes: vec![Z2mExpose {
                expose_type: "light".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(infer_kind(&Some(def)), "light");
    }

    #[test]
    fn infer_kind_switch() {
        let def = Z2mDefinition {
            exposes: vec![Z2mExpose {
                expose_type: "switch".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(infer_kind(&Some(def)), "switch");
    }

    #[test]
    fn infer_kind_fallback_sensor() {
        let def = Z2mDefinition {
            exposes: vec![Z2mExpose {
                expose_type: "numeric".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(infer_kind(&Some(def)), "sensor");
    }
}
