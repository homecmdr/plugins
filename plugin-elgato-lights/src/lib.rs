//! Elgato Key Light WASM plugin for HomeCmdr.
//!
//! Controls Elgato Key Light and Key Light Air devices directly
//! via HTTP API (default port 9123).
//!
//! Capabilities:
//!   - state  (on/off/toggle)
//!   - brightness (0–100)
//!   - color_temperature (kelvin)

wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default = "default_poll_interval")]
    #[allow(dead_code)]
    poll_interval_secs: u64,
}

fn default_true() -> bool { true }
fn default_base_url() -> String { "http://127.0.0.1:9123".to_string() }
fn default_poll_interval() -> u64 { 30 }

// ---------------------------------------------------------------------------
// Elgato API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LightsResponse {
    lights: Vec<LightState>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LightState {
    on: u8,
    brightness: u8,
    temperature: u16, // mired: 143 (7000 K) – 344 (2900 K)
}

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
    static STATE: std::cell::RefCell<Option<Config>> = std::cell::RefCell::new(None);
}

// ---------------------------------------------------------------------------
// Guest implementation
// ---------------------------------------------------------------------------

struct ElgatoLightsPlugin;

impl Guest for ElgatoLightsPlugin {
    fn name() -> String {
        "elgato_lights".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse elgato_lights config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "elgato_lights plugin is disabled by config");
            return Ok(());
        }

        host_log::log("info", &format!("elgato_lights initialised ({})", config.base_url));
        STATE.with(|s| { *s.borrow_mut() = Some(config); });
        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        let base_url = STATE.with(|s| {
            s.borrow().as_ref().map(|c| c.base_url.clone())
        }).ok_or("elgato_lights not initialised or disabled")?;

        let url = format!("{}/elgato/lights", base_url.trim_end_matches('/'));
        let body = host_http::get(&url)
            .map_err(|e| format!("elgato_lights HTTP GET failed: {e}"))?;

        let resp: LightsResponse = serde_json::from_str(&body)
            .map_err(|e| format!("elgato_lights parse error: {e}"))?;

        let mut updates = Vec::new();
        for (i, light) in resp.lights.iter().enumerate() {
            let vendor_id = format!("light:{}", i);
            let on = light.on != 0;
            let kelvin = mired_to_kelvin(light.temperature);

            let attrs = serde_json::json!({
                "state": if on { "on" } else { "off" },
                "brightness": light.brightness as i64,
                "color_temperature": { "value": kelvin as i64, "unit": "kelvin" },
            })
            .to_string();

            updates.push(DeviceUpdate {
                vendor_id,
                kind: "light".to_string(),
                attributes_json: attrs,
            });
        }

        Ok(updates)
    }

    fn command(device_id: String, command_json: String) -> Result<CommandResult, String> {
        let base_url = STATE.with(|s| {
            s.borrow().as_ref().map(|c| c.base_url.clone())
        }).ok_or("elgato_lights not initialised or disabled")?;

        // Verify this device belongs to us
        if !device_id.starts_with("elgato_lights:") {
            return Ok(CommandResult { handled: false, error: None });
        }

        // Parse light index from device_id: "elgato_lights:light:0"
        let parts: Vec<&str> = device_id.split(':').collect();
        let light_index: u32 = parts
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let cmd: DeviceCommand = serde_json::from_str(&command_json)
            .map_err(|e| format!("failed to parse command: {e}"))?;

        // Fetch current state first
        let url = format!("{}/elgato/lights", base_url.trim_end_matches('/'));
        let current_body = host_http::get(&url)
            .map_err(|e| format!("elgato_lights HTTP GET failed: {e}"))?;
        let mut current: LightsResponse = serde_json::from_str(&current_body)
            .map_err(|e| format!("elgato_lights parse error: {e}"))?;

        let light = current.lights.get_mut(light_index as usize)
            .ok_or_else(|| format!("light index {} out of range", light_index))?;

        match (cmd.capability.as_str(), cmd.action.as_str()) {
            ("state", "on")     => light.on = 1,
            ("state", "off")    => light.on = 0,
            ("state", "toggle") => light.on = if light.on != 0 { 0 } else { 1 },
            ("brightness", "set") => {
                let v = cmd.value.as_ref()
                    .and_then(|v| v.as_u64())
                    .ok_or("brightness set requires a numeric value")? as u8;
                light.brightness = v.min(100);
            }
            ("color_temperature", "set") => {
                let kelvin = cmd.value.as_ref()
                    .and_then(|v| v.as_u64())
                    .ok_or("color_temperature set requires a numeric value")? as u16;
                light.temperature = kelvin_to_mired(kelvin).clamp(143, 344);
            }
            _ => return Ok(CommandResult { handled: false, error: None }),
        }

        // Push updated state via PUT.
        let payload = serde_json::json!({ "lights": [light] }).to_string();
        let put_url = format!("{}/elgato/lights", base_url.trim_end_matches('/'));
        if let Err(e) = host_http::put(&put_url, "application/json", &payload) {
            return Ok(CommandResult {
                handled: true,
                error: Some(format!("elgato_lights PUT failed: {e}")),
            });
        }

        Ok(CommandResult { handled: true, error: None })
    }
}

export!(ElgatoLightsPlugin);

// ---------------------------------------------------------------------------
// Unit conversion helpers
// ---------------------------------------------------------------------------

/// Elgato mired value → kelvin (rounded to nearest 100 K)
fn mired_to_kelvin(mired: u16) -> u16 {
    if mired == 0 { return 0; }
    (1_000_000u32 / mired as u32) as u16
}

/// Kelvin → Elgato mired value, clamped to 143–344
fn kelvin_to_mired(kelvin: u16) -> u16 {
    if kelvin == 0 { return 143; }
    ((1_000_000u32 / kelvin as u32) as u16).clamp(143, 344)
}
