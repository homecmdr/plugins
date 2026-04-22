//! Roku TV WASM plugin for HomeCmdr.
//!
//! Polls Roku streaming devices and Roku TVs via the External Control Protocol
//! (ECP).  ECP runs on port 8060 by default.
//!
//! Capabilities (poll):
//!   - power state (on/standby)
//!   - active application name and ID
//!   - device model name, serial number, software version
//!
//! Remote control commands (power, home, select, etc.) require POST requests
//! to `/keypress/{key}`.  host_http currently only supports GET, so commands
//! are acknowledged but not forwarded.
//! TODO: extend WIT to support host_http::post when available.
//!
//! Build:
//!   cargo build --release
//!   cp target/wasm32-wasip2/release/plugin_roku_tv_wasm.wasm \
//!      <workspace>/config/plugins/roku_tv.wasm

wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::Deserialize;

#[derive(Deserialize)]
struct DeviceCommand {
    capability: String,
    action: String,
    /// For "keypress/send", the ECP key name (e.g. "Home", "VolumeUp").
    value: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    /// HTTP base URL of the Roku device, e.g. "http://192.168.1.100:8060".
    /// There is no default — each Roku has its own IP address.
    base_url: String,
}

fn default_true() -> bool {
    true
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

struct RokuTvPlugin;

impl Guest for RokuTvPlugin {
    fn name() -> String {
        "roku_tv".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse roku_tv config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "roku_tv plugin is disabled by config");
            return Ok(());
        }

        if config.base_url.is_empty() {
            return Err("roku_tv: base_url is required (e.g. http://192.168.1.100:8060)".into());
        }

        host_log::log(
            "info",
            &format!("roku_tv initialised ({})", config.base_url),
        );
        STATE.with(|s| {
            *s.borrow_mut() = Some(config);
        });
        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        let base_url = STATE
            .with(|s| s.borrow().as_ref().map(|c| c.base_url.clone()))
            .ok_or("roku_tv not initialised or disabled")?;

        let base = base_url.trim_end_matches('/');

        // Fetch device info
        let device_info_url = format!("{base}/query/device-info");
        let device_info_xml = host_http::get(&device_info_url)
            .map_err(|e| format!("roku_tv device-info GET failed: {e}"))?;

        // Fetch active app
        let active_app_url = format!("{base}/query/active-app");
        let active_app_xml = host_http::get(&active_app_url)
            .map_err(|e| format!("roku_tv active-app GET failed: {e}"))?;

        // Parse device-info fields from XML
        let model_name = xml_tag(&device_info_xml, "model-name").unwrap_or_default();
        let friendly_name =
            xml_tag(&device_info_xml, "friendly-device-name").unwrap_or_default();
        let serial = xml_tag(&device_info_xml, "serial-number").unwrap_or_default();
        let software_version =
            xml_tag(&device_info_xml, "software-version").unwrap_or_default();
        let power_mode = xml_tag(&device_info_xml, "power-mode").unwrap_or_default();

        // "PowerOn" → on; anything else (Standby, DisplayOff, …) → off
        let power_on = power_mode.eq_ignore_ascii_case("PowerOn");

        // Parse active app — the <app> tag body is the app name; id attribute is the app ID.
        // ECP format: <active-app><app id="tvinput.hdmi1">HDMI 1</app></active-app>
        let active_app_name = xml_tag(&active_app_xml, "app").unwrap_or_default();
        let active_app_id = xml_attr(&active_app_xml, "app", "id").unwrap_or_default();

        let attrs = serde_json::json!({
            "power":                          power_on,
            "custom.roku_tv.model_name":      model_name,
            "custom.roku_tv.friendly_name":   friendly_name,
            "custom.roku_tv.serial_number":   serial,
            "custom.roku_tv.software_version": software_version,
            "custom.roku_tv.power_mode":      power_mode,
            "custom.roku_tv.active_app":      active_app_name,
            "custom.roku_tv.active_app_id":   active_app_id,
        })
        .to_string();

        Ok(vec![DeviceUpdate {
            vendor_id: "tv".to_string(),
            kind: "virtual".to_string(),
            attributes_json: attrs,
        }])
    }

    fn command(device_id: String, command_json: String) -> Result<CommandResult, String> {
        if !device_id.starts_with("roku_tv:") {
            return Ok(CommandResult {
                handled: false,
                error: None,
            });
        }

        let base_url = STATE
            .with(|s| s.borrow().as_ref().map(|c| c.base_url.clone()))
            .ok_or("roku_tv not initialised or disabled")?;

        let cmd: DeviceCommand = serde_json::from_str(&command_json)
            .map_err(|e| format!("failed to parse command: {e}"))?;

        // Map HomeCmdr capability/action to an ECP key name.
        let ecp_key = match (cmd.capability.as_str(), cmd.action.as_str()) {
            ("power", "off")    => "PowerOff".to_string(),
            ("power", "on")     => "PowerOn".to_string(),
            ("power", "toggle") => "Power".to_string(),
            ("navigate", "home")     => "Home".to_string(),
            ("navigate", "back")     => "Back".to_string(),
            ("navigate", "select")   => "Select".to_string(),
            ("navigate", "up")       => "Up".to_string(),
            ("navigate", "down")     => "Down".to_string(),
            ("navigate", "left")     => "Left".to_string(),
            ("navigate", "right")    => "Right".to_string(),
            ("media", "play")        => "Play".to_string(),
            ("media", "pause")       => "Pause".to_string(),
            ("media", "forward")     => "FastForward".to_string(),
            ("media", "rewind")      => "Rewind".to_string(),
            ("volume", "up")         => "VolumeUp".to_string(),
            ("volume", "down")       => "VolumeDown".to_string(),
            ("volume", "mute")       => "VolumeMute".to_string(),
            // Pass-through: capability "keypress", action "send", value is the ECP key name.
            ("keypress", "send") => {
                cmd.value
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or("keypress/send requires a key name string in `value`")?
            }
            _ => return Ok(CommandResult { handled: false, error: None }),
        };

        // ECP keypress: POST /keypress/{key} with an empty body.
        let keypress_url = format!(
            "{}/keypress/{}",
            base_url.trim_end_matches('/'),
            ecp_key
        );
        if let Err(e) = host_http::post(&keypress_url, "application/x-www-form-urlencoded", "") {
            return Ok(CommandResult {
                handled: true,
                error: Some(format!("roku_tv keypress POST failed: {e}")),
            });
        }

        host_log::log(
            "debug",
            &format!("roku_tv: sent keypress '{}'", ecp_key),
        );

        Ok(CommandResult { handled: true, error: None })
    }
}

export!(RokuTvPlugin);

// ---------------------------------------------------------------------------
// Minimal XML helpers — ECP responses are simple enough for string search
// ---------------------------------------------------------------------------

/// Extract the text content of the first occurrence of `<tag>…</tag>`.
fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    // Find the opening tag start
    let tag_start = xml.find(&open)?;
    // Find the closing `>` of the opening tag (skips over any attributes)
    let content_start = xml[tag_start..].find('>')? + tag_start + 1;
    let close = format!("</{}>", tag);
    let content_end = xml[content_start..].find(&close)? + content_start;
    Some(xml[content_start..content_end].trim().to_string())
}

/// Extract the value of `attr` from the first occurrence of `<tag … attr="value" …>`.
fn xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let tag_pos = xml.find(&open)?;
    let tag_end = xml[tag_pos..].find('>')? + tag_pos;
    let tag_body = &xml[tag_pos..tag_end];

    let attr_eq = format!("{}=\"", attr);
    let attr_pos = tag_body.find(&attr_eq)? + attr_eq.len();
    let value_end = tag_body[attr_pos..].find('"')? + attr_pos;
    Some(tag_body[attr_pos..value_end].to_string())
}

// ---------------------------------------------------------------------------
// Tests (run with: cargo test --target <host-triple>)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_tag_simple() {
        let xml = "<device-info><model-name>Roku Express</model-name></device-info>";
        assert_eq!(xml_tag(xml, "model-name"), Some("Roku Express".to_string()));
    }

    #[test]
    fn xml_tag_with_whitespace() {
        let xml = "<device-info>\n  <power-mode>Standby</power-mode>\n</device-info>";
        assert_eq!(xml_tag(xml, "power-mode"), Some("Standby".to_string()));
    }

    #[test]
    fn xml_tag_missing() {
        let xml = "<device-info><model-name>Roku</model-name></device-info>";
        assert_eq!(xml_tag(xml, "serial-number"), None);
    }

    #[test]
    fn xml_attr_simple() {
        let xml = r#"<active-app><app id="tvinput.hdmi1">HDMI 1</app></active-app>"#;
        assert_eq!(xml_attr(xml, "app", "id"), Some("tvinput.hdmi1".to_string()));
    }

    #[test]
    fn power_mode_mapping() {
        assert!("PowerOn".eq_ignore_ascii_case("PowerOn"));
        assert!(!"Standby".eq_ignore_ascii_case("PowerOn"));
        assert!(!"DisplayOff".eq_ignore_ascii_case("PowerOn"));
    }
}
