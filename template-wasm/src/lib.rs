//! TODO: Replace this module doc with a description of what your plugin controls.
//!
//! WASM plugin template for HomeCmdr.
//!
//! This plugin is compiled to `wasm32-wasip2` and loaded by the HomeCmdr WASM
//! plugin host at startup.  The host calls:
//!
//!   - `init(config_json)`  — once at startup, before any poll or command
//!   - `poll()`             — on the interval set in `[runtime] poll_interval_secs`
//!   - `command(device_id, command_json)` — when a command is dispatched to a device
//!
//! All network I/O must go through `host_http::{get, post, put}` — WASM
//! components cannot open sockets directly.
//!
//! Build:
//!   cargo build --release
//!   # Output: target/wasm32-wasip2/release/plugin_my_plugin_wasm.wasm
//!
//! Deploy:
//!   cp target/wasm32-wasip2/release/plugin_my_plugin_wasm.wasm \
//!      <homecmdr-api>/config/plugins/my_plugin.wasm
//!   cp my_plugin.plugin.toml \
//!      <homecmdr-api>/config/plugins/my_plugin.plugin.toml

// The macro reads `wit/homecmdr-plugin.wit` and generates the trait and types
// that this crate must implement.
wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::Deserialize;

// ── Config ────────────────────────────────────────────────────────────────────
// Fields here must match the keys declared in [[config.fields]] of
// my_plugin.plugin.toml.  Any field with a `default` in the TOML should have
// a corresponding `#[serde(default = "...")]` here so the plugin still works
// when the user omits it.

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    // TODO: add config fields here to match my_plugin.plugin.toml.
    #[serde(default = "default_base_url")]
    base_url: String,
}

fn default_true() -> bool { true }
fn default_base_url() -> String { "http://127.0.0.1:8080".to_string() }

// ── API response types ────────────────────────────────────────────────────────
// TODO: replace with the real response shape from your device/service API.

/// Example: the JSON shape returned by the device's status endpoint.
#[derive(Debug, Deserialize)]
struct DeviceStatus {
    /// true = on, false = off/standby.
    on: bool,
    // TODO: add more fields as needed.
}

// ── Command shape ─────────────────────────────────────────────────────────────
// HomeCmdr serialises commands as JSON before passing them to `command()`.

#[derive(Deserialize)]
struct DeviceCommand {
    capability: String,
    action: String,
    value: Option<serde_json::Value>,
}

// ── Plugin state ──────────────────────────────────────────────────────────────
// WASM components are single-threaded; `thread_local!` is the idiomatic way to
// hold mutable state without unsafe code.

thread_local! {
    static STATE: std::cell::RefCell<Option<Config>> = std::cell::RefCell::new(None);
}

// ── Plugin implementation ─────────────────────────────────────────────────────

// TODO: rename MyPlugin to match your plugin.
struct MyPlugin;

impl Guest for MyPlugin {
    fn name() -> String {
        // TODO: must match the `name` in my_plugin.plugin.toml and the
        //       [adapters.<name>] key in the HomeCmdr config.
        "my_plugin".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse my_plugin config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "my_plugin is disabled by config");
            return Ok(());
        }

        host_log::log("info", &format!("my_plugin initialised ({})", config.base_url));
        STATE.with(|s| { *s.borrow_mut() = Some(config); });
        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        // Retrieve the config saved during init().
        let base_url = STATE
            .with(|s| s.borrow().as_ref().map(|c| c.base_url.clone()))
            .ok_or("my_plugin not initialised or disabled")?;

        // TODO: replace with your device/service's actual status endpoint.
        let url = format!("{}/status", base_url.trim_end_matches('/'));
        let body = host_http::get(&url)
            .map_err(|e| format!("my_plugin HTTP GET failed: {e}"))?;

        // TODO: replace DeviceStatus with your actual response type.
        let status: DeviceStatus = serde_json::from_str(&body)
            .map_err(|e| format!("my_plugin parse error: {e}"))?;

        // Build the HomeCmdr attribute map.
        // Keys must be canonical capability names (e.g. "state", "brightness")
        // or namespaced custom attributes (e.g. "custom.my_plugin.signal_strength").
        let attributes = serde_json::json!({
            // TODO: map your device state to HomeCmdr capability keys.
            "state": if status.on { "on" } else { "off" },
        })
        .to_string();

        Ok(vec![DeviceUpdate {
            // TODO: set the vendor_id to a stable identifier for this device.
            // The full device ID seen by HomeCmdr will be "<plugin_name>:<vendor_id>",
            // e.g. "my_plugin:device_0".
            vendor_id: "device_0".to_string(),
            // TODO: "light" | "switch" | "sensor" | "virtual"
            kind: "switch".to_string(),
            attributes_json: attributes,
        }])
    }

    fn command(device_id: String, command_json: String) -> Result<CommandResult, String> {
        // Reject commands not addressed to this plugin.
        if !device_id.starts_with("my_plugin:") {
            return Ok(CommandResult { handled: false, error: None });
        }

        let base_url = STATE
            .with(|s| s.borrow().as_ref().map(|c| c.base_url.clone()))
            .ok_or("my_plugin not initialised or disabled")?;

        let cmd: DeviceCommand = serde_json::from_str(&command_json)
            .map_err(|e| format!("failed to parse command: {e}"))?;

        match (cmd.capability.as_str(), cmd.action.as_str()) {
            // TODO: add capability/action pairs that this plugin handles.
            ("state", "on") => {
                let url = format!("{}/on", base_url.trim_end_matches('/'));
                host_http::post(&url, "application/json", "{}")
                    .map_err(|e| format!("my_plugin POST failed: {e}"))?;
            }
            ("state", "off") => {
                let url = format!("{}/off", base_url.trim_end_matches('/'));
                host_http::post(&url, "application/json", "{}")
                    .map_err(|e| format!("my_plugin POST failed: {e}"))?;
            }
            ("state", "toggle") => {
                let url = format!("{}/toggle", base_url.trim_end_matches('/'));
                host_http::post(&url, "application/json", "{}")
                    .map_err(|e| format!("my_plugin POST failed: {e}"))?;
            }
            // TODO: handle other capabilities (e.g. "brightness", "color_temperature")
            // or return handled: false for anything unrecognised.
            _ => return Ok(CommandResult { handled: false, error: None }),
        }

        Ok(CommandResult { handled: true, error: None })
    }
}

// Required: registers MyPlugin as the WASM component export.
// TODO: update the struct name if you renamed MyPlugin above.
export!(MyPlugin);
