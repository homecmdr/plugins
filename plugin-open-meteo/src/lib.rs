//! Open-Meteo weather WASM plugin for HomeCmdr.
//!
//! Fetches current weather from the Open-Meteo free API and reports 12
//! read-only sensor devices, matching the device IDs and attribute shapes
//! produced by the native `adapter-open-meteo` crate.
//!
//! Devices reported (vendor_id → attribute key):
//!   temperature_outdoor  → temperature_outdoor  {value, unit: "celsius"}
//!   wind_speed           → wind_speed           {value, unit: "km/h"}
//!   wind_direction       → wind_direction       i64 (degrees)
//!   temperature_apparent → temperature_apparent {value, unit: "celsius"}
//!   humidity             → humidity             {value, unit: "percent"}
//!   rainfall             → rainfall             {value, unit: "mm", period: "hour"}
//!   cloud_coverage       → cloud_coverage       i64 (0–100)
//!   uv_index             → uv_index             f64
//!   pressure             → pressure             {value, unit: "hPa"}
//!   wind_gust            → wind_gust            {value, unit: "km/h"}
//!   weather_condition    → weather_condition    string (WMO description)
//!   is_day               → custom.open_meteo.is_day  bool
//!
//! Build:
//!   cargo build --release
//!   cp target/wasm32-wasip2/release/plugin_open_meteo_wasm.wasm \
//!      <workspace>/config/plugins/open_meteo.wasm

wit_bindgen::generate!({
    world: "adapter",
    path: "wit/homecmdr-plugin.wit",
});

use exports::homecmdr::plugin::plugin::{CommandResult, DeviceUpdate, Guest};
use homecmdr::plugin::host_http;
use homecmdr::plugin::host_log;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_BASE_URL: &str = "https://api.open-meteo.com";

/// All `current` fields requested from the Open-Meteo API in one call.
const CURRENT_FIELDS: &str = "temperature_2m,apparent_temperature,\
    relative_humidity_2m,precipitation,cloud_cover,uv_index,\
    surface_pressure,wind_speed_10m,wind_gusts_10m,\
    wind_direction_10m,weather_code,is_day";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    enabled: bool,
    latitude: f64,
    longitude: f64,
    #[serde(default = "default_base_url")]
    base_url: String,
}

fn default_true() -> bool { true }
fn default_base_url() -> String { DEFAULT_BASE_URL.to_string() }

// ---------------------------------------------------------------------------
// Open-Meteo API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    current: CurrentWeather,
}

#[derive(Debug, Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    apparent_temperature: f64,
    relative_humidity_2m: f64,
    precipitation: f64,
    cloud_cover: u8,
    uv_index: f64,
    surface_pressure: f64,
    wind_speed_10m: f64,
    wind_gusts_10m: f64,
    wind_direction_10m: f64,
    weather_code: u8,
    is_day: u8,
}

// ---------------------------------------------------------------------------
// Plugin state (thread_local — WASM is single-threaded)
// ---------------------------------------------------------------------------

thread_local! {
    static STATE: std::cell::RefCell<Option<Config>> = std::cell::RefCell::new(None);
}

// ---------------------------------------------------------------------------
// Guest implementation
// ---------------------------------------------------------------------------

struct OpenMeteoPlugin;

impl Guest for OpenMeteoPlugin {
    fn name() -> String {
        "open_meteo".to_string()
    }

    fn init(config_json: String) -> Result<(), String> {
        let config: Config = serde_json::from_str(&config_json)
            .map_err(|e| format!("failed to parse open_meteo config: {e}"))?;

        if !config.enabled {
            host_log::log("info", "open_meteo plugin is disabled by config");
            return Ok(());
        }

        host_log::log(
            "info",
            &format!(
                "open_meteo initialised for lat={} lon={}",
                config.latitude, config.longitude
            ),
        );

        STATE.with(|s| {
            *s.borrow_mut() = Some(config);
        });

        Ok(())
    }

    fn poll() -> Result<Vec<DeviceUpdate>, String> {
        let (lat, lon, base_url) = STATE
            .with(|s| {
                s.borrow()
                    .as_ref()
                    .map(|c| (c.latitude, c.longitude, c.base_url.clone()))
            })
            .ok_or("open_meteo not initialised or disabled")?;

        let url = build_url(&base_url, lat, lon);

        host_log::log("debug", &format!("open_meteo polling {url}"));

        let body = host_http::get(&url)
            .map_err(|e| format!("open_meteo HTTP GET failed: {e}"))?;

        let resp: ForecastResponse = serde_json::from_str(&body)
            .map_err(|e| format!("open_meteo parse error: {e}"))?;

        Ok(build_updates(&resp.current))
    }

    fn command(_device_id: String, _command_json: String) -> Result<CommandResult, String> {
        // Open-Meteo is a read-only weather source — no commands are handled.
        Ok(CommandResult { handled: false, error: None })
    }
}

export!(OpenMeteoPlugin);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the Open-Meteo forecast URL with all required current fields.
fn build_url(base_url: &str, lat: f64, lon: f64) -> String {
    format!(
        "{}/v1/forecast?latitude={}&longitude={}&current={}",
        base_url.trim_end_matches('/'),
        lat,
        lon,
        CURRENT_FIELDS,
    )
}

/// Convert a parsed `CurrentWeather` into the 12 `DeviceUpdate` values that
/// the host will upsert into the device registry.
///
/// Attribute JSON shapes are identical to those produced by the native
/// `adapter-open-meteo` crate so that device IDs and history are stable
/// across the native → WASM migration.
fn build_updates(w: &CurrentWeather) -> Vec<DeviceUpdate> {
    let condition = wmo_code_to_description(w.weather_code);
    let is_day = w.is_day != 0;

    vec![
        DeviceUpdate {
            vendor_id: "temperature_outdoor".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "temperature_outdoor": { "value": w.temperature_2m, "unit": "celsius" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "wind_speed".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "wind_speed": { "value": w.wind_speed_10m, "unit": "km/h" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "wind_direction".into(),
            kind: "sensor".into(),
            // Integer degrees; matches AttributeValue::Integer in the native adapter.
            attributes_json: serde_json::json!({
                "wind_direction": w.wind_direction_10m as i64
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "temperature_apparent".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "temperature_apparent": { "value": w.apparent_temperature, "unit": "celsius" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "humidity".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "humidity": { "value": w.relative_humidity_2m, "unit": "percent" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "rainfall".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "rainfall": { "value": w.precipitation, "unit": "mm", "period": "hour" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "cloud_coverage".into(),
            kind: "sensor".into(),
            // Integer 0–100; matches AttributeValue::Integer in the native adapter.
            attributes_json: serde_json::json!({
                "cloud_coverage": w.cloud_cover as i64
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "uv_index".into(),
            kind: "sensor".into(),
            // Float; matches AttributeValue::Float in the native adapter.
            attributes_json: serde_json::json!({
                "uv_index": w.uv_index
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "pressure".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "pressure": { "value": w.surface_pressure, "unit": "hPa" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "wind_gust".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "wind_gust": { "value": w.wind_gusts_10m, "unit": "km/h" }
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "weather_condition".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "weather_condition": condition
            })
            .to_string(),
        },
        DeviceUpdate {
            vendor_id: "is_day".into(),
            kind: "sensor".into(),
            attributes_json: serde_json::json!({
                "custom.open_meteo.is_day": is_day
            })
            .to_string(),
        },
    ]
}

/// Map a WMO weather interpretation code to a human-readable description.
///
/// Codes and descriptions are identical to those used in the native
/// `adapter-open-meteo` crate.
fn wmo_code_to_description(code: u8) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 => "Light drizzle",
        53 => "Moderate drizzle",
        55 => "Dense drizzle",
        56 | 57 => "Freezing drizzle",
        61 => "Slight rain",
        63 => "Moderate rain",
        65 => "Heavy rain",
        66 | 67 => "Freezing rain",
        71 => "Slight snow",
        73 => "Moderate snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 => "Slight showers",
        81 => "Moderate showers",
        82 => "Violent showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
    }
}
