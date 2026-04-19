use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use homecmdr_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use homecmdr_core::bus::EventBus;
use homecmdr_core::capability::{MEDIA_APP, MEDIA_PLAYBACK, MEDIA_SOURCE, POWER, STATE};
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::config::AdapterConfig;
use homecmdr_core::event::Event;
use homecmdr_core::http::{external_http_client, send_with_retry};
use homecmdr_core::model::{AttributeValue, Attributes, Device, DeviceId, DeviceKind, Metadata};
use homecmdr_core::registry::DeviceRegistry;
use tokio::time::{sleep, Duration};

const ADAPTER_NAME: &str = "roku_tv";
const DEVICE_VENDOR_ID: &str = "tv";

#[derive(Debug, Clone, Deserialize)]
pub struct RokuTvConfig {
    pub enabled: bool,
    pub ip_address: String,
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub test_poll_interval_ms: Option<u64>,
}

pub struct RokuTvFactory;

static ROKU_TV_FACTORY: RokuTvFactory = RokuTvFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &ROKU_TV_FACTORY,
    }
}

pub struct RokuTvAdapter {
    client: Client,
    poll_interval: Duration,
    base_url: String,
}

impl RokuTvAdapter {
    pub fn new(config: RokuTvConfig) -> Result<Self> {
        let poll_interval = config
            .test_poll_interval_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(config.poll_interval_secs));

        Ok(Self {
            client: external_http_client()?,
            base_url: format!("http://{}:8060", config.ip_address.trim()),
            poll_interval,
        })
    }

    #[cfg(test)]
    fn with_base_url(config: RokuTvConfig, base_url: impl Into<String>) -> Result<Self> {
        let poll_interval = config
            .test_poll_interval_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(config.poll_interval_secs));

        Ok(Self {
            client: external_http_client()?,
            poll_interval,
            base_url: base_url.into(),
        })
    }

    async fn poll_once(&self, registry: &DeviceRegistry) -> Result<()> {
        let info = self.fetch_roku_state().await?;
        let previous = registry.get(&roku_device_id());
        let device = build_device(info, previous.as_ref());

        registry
            .upsert(device)
            .await
            .context("failed to upsert Roku TV device")?;

        Ok(())
    }

    async fn fetch_device_info(&self) -> Result<RokuDeviceInfo> {
        let body = self
            .get_text("/query/device-info", "Roku device info")
            .await?;

        parse_device_info(&body)
    }

    async fn fetch_active_app(&self) -> Result<Option<RokuActiveApp>> {
        let body = self
            .get_text("/query/active-app", "Roku active app")
            .await?;
        Ok(parse_active_app(&body))
    }

    async fn fetch_media_player(&self) -> Result<Option<RokuMediaPlayer>> {
        let body = self
            .get_text("/query/media-player", "Roku media player")
            .await?;
        Ok(parse_media_player(&body))
    }

    async fn fetch_apps(&self) -> Result<Vec<RokuApp>> {
        let body = self.get_text("/query/apps", "Roku app list").await?;
        Ok(parse_apps(&body))
    }

    async fn fetch_roku_state(&self) -> Result<RokuState> {
        let device_info = self.fetch_device_info().await?;
        let active_app = self.fetch_active_app().await?;
        let media_player = self.fetch_media_player().await?;

        Ok(RokuState {
            device_info,
            active_app,
            media_player,
        })
    }

    async fn get_text(&self, path: &str, operation: &str) -> Result<String> {
        send_with_retry(
            self.client.get(format!("{}{}", self.base_url(), path)),
            operation,
        )
        .await?
        .text()
        .await
        .with_context(|| format!("failed to read {operation} response"))
    }

    async fn send_keypress(&self, key: &str) -> Result<()> {
        send_with_retry(
            self.client
                .post(format!("{}/keypress/{key}", self.base_url())),
            &format!("Roku keypress '{key}'"),
        )
        .await?;

        Ok(())
    }

    async fn launch_app(&self, app_id: &str) -> Result<()> {
        send_with_retry(
            self.client
                .post(format!("{}/launch/{app_id}", self.base_url())),
            &format!("Roku launch app '{app_id}'"),
        )
        .await?;

        Ok(())
    }

    async fn execute_media_source(&self, source: &str) -> Result<()> {
        let key = match normalize_token(source).as_str() {
            "home" => Some("Home"),
            "tv" | "tuner" | "livetv" | "tvinputdtv" => Some("InputTuner"),
            "hdmi1" | "input_hdmi1" => Some("InputHDMI1"),
            "hdmi2" | "input_hdmi2" => Some("InputHDMI2"),
            "hdmi3" | "input_hdmi3" => Some("InputHDMI3"),
            "hdmi4" | "input_hdmi4" => Some("InputHDMI4"),
            "av1" | "input_av1" => Some("InputAV1"),
            _ => None,
        }
        .with_context(|| format!("unsupported Roku media source '{source}'"))?;

        self.send_keypress(key).await
    }

    async fn execute_media_app(&self, app_selector: &str) -> Result<()> {
        let normalized_selector = normalize_token(app_selector);
        let app = self
            .fetch_apps()
            .await?
            .into_iter()
            .find(|candidate| {
                candidate.id == app_selector
                    || normalize_token(&candidate.name) == normalized_selector
            })
            .with_context(|| format!("Roku app '{app_selector}' was not found"))?;

        self.launch_app(&app.id).await
    }

    async fn handle_command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: DeviceRegistry,
    ) -> Result<bool> {
        if *device_id != roku_device_id() {
            return Ok(false);
        }

        match (
            command.capability.as_str(),
            command.action.as_str(),
            command.value.as_ref(),
        ) {
            (POWER, "on", _) => self.send_keypress("PowerOn").await?,
            (POWER, "off", _) => self.send_keypress("PowerOff").await?,
            (POWER, "toggle", _) => self.send_keypress("Power").await?,
            (MEDIA_PLAYBACK, "play", _) => self.send_keypress("Play").await?,
            (MEDIA_PLAYBACK, "pause", _) => self.send_keypress("Play").await?,
            (MEDIA_PLAYBACK, "stop", _) => self.send_keypress("Home").await?,
            (MEDIA_PLAYBACK, "next", _) => self.send_keypress("Fwd").await?,
            (MEDIA_PLAYBACK, "previous", _) => self.send_keypress("Rev").await?,
            (MEDIA_SOURCE, "set", Some(AttributeValue::Text(source))) => {
                self.execute_media_source(source).await?
            }
            (MEDIA_APP, "set", Some(AttributeValue::Text(app))) => {
                self.execute_media_app(app).await?
            }
            _ => return Ok(false),
        }

        let info = self.fetch_roku_state().await?;
        let previous = registry.get(device_id);
        registry
            .upsert(build_device(info, previous.as_ref()))
            .await
            .with_context(|| {
                format!(
                    "failed to update registry for '{}': command applied",
                    device_id.0
                )
            })?;

        Ok(true)
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl AdapterFactory for RokuTvFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: RokuTvConfig =
            serde_json::from_value(config).context("failed to parse roku_tv adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(RokuTvAdapter::new(config)?)))
    }
}

#[async_trait]
impl Adapter for RokuTvAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            if let Err(error) = self.poll_once(&registry).await {
                tracing::error!(error = %error, "Roku TV poll failed");
                bus.publish(Event::SystemError {
                    message: format!("roku_tv poll failed: {error}"),
                });
            }

            sleep(self.poll_interval).await;
        }
    }

    async fn command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: DeviceRegistry,
    ) -> Result<bool> {
        self.handle_command(device_id, command, registry).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RokuDeviceInfo {
    power_mode: String,
    friendly_name: Option<String>,
    model_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RokuActiveApp {
    id: String,
    name: String,
    is_screensaver: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RokuMediaPlayer {
    state: String,
    plugin_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RokuApp {
    id: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RokuState {
    device_info: RokuDeviceInfo,
    active_app: Option<RokuActiveApp>,
    media_player: Option<RokuMediaPlayer>,
}

fn validate_config(config: &RokuTvConfig) -> Result<()> {
    if config.ip_address.trim().is_empty() {
        bail!("adapters.roku_tv.ip_address must not be empty");
    }

    if config.poll_interval_secs == 0 && config.test_poll_interval_ms.is_none() {
        bail!("adapters.roku_tv.poll_interval_secs must be >= 1");
    }

    Ok(())
}

fn build_device(state: RokuState, previous: Option<&Device>) -> Device {
    let now = Utc::now();
    let power = if is_powered_on(&state.device_info.power_mode) {
        "on"
    } else {
        "off"
    };
    let mut attributes = Attributes::from([
        (POWER.to_string(), AttributeValue::Text(power.to_string())),
        (
            STATE.to_string(),
            AttributeValue::Text("online".to_string()),
        ),
    ]);

    if let Some(active_app) = state.active_app.as_ref() {
        attributes.insert(
            MEDIA_APP.to_string(),
            AttributeValue::Text(active_app.name.clone()),
        );
        attributes.insert(
            MEDIA_SOURCE.to_string(),
            AttributeValue::Text(media_source_value(active_app).to_string()),
        );
    }

    if let Some(playback) = media_playback_value(state.media_player.as_ref()) {
        attributes.insert(
            MEDIA_PLAYBACK.to_string(),
            AttributeValue::Text(playback.to_string()),
        );
    }

    let mut vendor_specific = HashMap::from([(
        "power_mode".to_string(),
        serde_json::json!(state.device_info.power_mode),
    )]);
    if let Some(name) = state.device_info.friendly_name {
        vendor_specific.insert("friendly_name".to_string(), serde_json::json!(name));
    }
    if let Some(model) = state.device_info.model_name {
        vendor_specific.insert("model_name".to_string(), serde_json::json!(model));
    }
    if let Some(active_app) = state.active_app {
        vendor_specific.insert(
            "active_app_id".to_string(),
            serde_json::json!(active_app.id),
        );
        vendor_specific.insert(
            "active_app_is_screensaver".to_string(),
            serde_json::json!(active_app.is_screensaver),
        );
    }
    if let Some(media_player) = state.media_player {
        vendor_specific.insert(
            "media_player_state".to_string(),
            serde_json::json!(media_player.state),
        );
        if let Some(plugin_id) = media_player.plugin_id {
            vendor_specific.insert(
                "media_player_plugin_id".to_string(),
                serde_json::json!(plugin_id),
            );
        }
    }

    let metadata = Metadata {
        source: ADAPTER_NAME.to_string(),
        accuracy: None,
        vendor_specific,
    };
    let updated_at = previous
        .filter(|device| {
            device.kind == DeviceKind::Virtual
                && device.attributes == attributes
                && device.metadata == metadata
        })
        .map(|device| device.updated_at)
        .unwrap_or(now);

    Device {
        id: roku_device_id(),
        room_id: previous.and_then(|device| device.room_id.clone()),
        kind: DeviceKind::Virtual,
        attributes,
        metadata,
        updated_at,
        last_seen: now,
    }
}

fn roku_device_id() -> DeviceId {
    DeviceId(format!("{ADAPTER_NAME}:{DEVICE_VENDOR_ID}"))
}

fn is_powered_on(power_mode: &str) -> bool {
    !matches!(power_mode, "PowerOff" | "DisplayOff" | "Suspend")
}

fn parse_device_info(xml: &str) -> Result<RokuDeviceInfo> {
    Ok(RokuDeviceInfo {
        power_mode: extract_xml_tag(xml, "power-mode").unwrap_or_else(|| "PowerOn".to_string()),
        friendly_name: extract_xml_tag(xml, "friendly-device-name"),
        model_name: extract_xml_tag(xml, "model-name"),
    })
}

fn parse_active_app(xml: &str) -> Option<RokuActiveApp> {
    if let Some(id) = extract_xml_attribute(xml, "app", "id") {
        return Some(RokuActiveApp {
            id,
            name: extract_xml_element_text(xml, "app")?,
            is_screensaver: false,
        });
    }

    extract_xml_attribute(xml, "screensaver", "id").and_then(|id| {
        Some(RokuActiveApp {
            id,
            name: extract_xml_element_text(xml, "screensaver")?,
            is_screensaver: true,
        })
    })
}

fn parse_media_player(xml: &str) -> Option<RokuMediaPlayer> {
    let state = extract_xml_root_attribute(xml, "state")?;
    Some(RokuMediaPlayer {
        state,
        plugin_id: extract_xml_attribute(xml, "plugin", "id"),
    })
}

fn parse_apps(xml: &str) -> Vec<RokuApp> {
    let mut apps = Vec::new();
    let mut rest = xml;
    let start = "<app ";
    let end = "</app>";

    while let Some(start_index) = rest.find(start) {
        let candidate = &rest[start_index..];
        let Some(end_index) = candidate.find(end) else {
            break;
        };
        let entry = &candidate[..end_index + end.len()];
        if let Some(id) = extract_xml_attribute(entry, "app", "id") {
            if let Some(name) = extract_xml_element_text(entry, "app") {
                apps.push(RokuApp { id, name });
            }
        }
        rest = &candidate[end_index + end.len()..];
    }

    apps
}

fn media_source_value(active_app: &RokuActiveApp) -> &str {
    match active_app.id.as_str() {
        "tvinput.dtv" => "tuner",
        "tvinput.hdmi1" => "hdmi1",
        "tvinput.hdmi2" => "hdmi2",
        "tvinput.hdmi3" => "hdmi3",
        "tvinput.hdmi4" => "hdmi4",
        "tvinput.av1" => "av1",
        _ if active_app.is_screensaver => "screensaver",
        _ => "app",
    }
}

fn media_playback_value(media_player: Option<&RokuMediaPlayer>) -> Option<&'static str> {
    let media_player = media_player?;
    match normalize_token(&media_player.state).as_str() {
        "play" | "playing" => Some("playing"),
        "pause" | "paused" => Some("paused"),
        "buffer" | "buffering" => Some("buffering"),
        "close" | "closed" | "stop" | "stopped" => Some("stopped"),
        _ => Some("idle"),
    }
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let start_index = xml.find(&start)? + start.len();
    let rest = &xml[start_index..];
    let end_index = rest.find(&end)?;
    Some(rest[..end_index].trim().to_string())
}

fn extract_xml_element_text(xml: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}");
    let start_index = xml.find(&start)?;
    let candidate = &xml[start_index..];
    let tag_end = candidate.find('>')? + 1;
    let rest = &candidate[tag_end..];
    let end = format!("</{tag}>");
    let end_index = rest.find(&end)?;
    Some(rest[..end_index].trim().to_string())
}

fn extract_xml_attribute(xml: &str, tag: &str, attribute: &str) -> Option<String> {
    let start = format!("<{tag}");
    let start_index = xml.find(&start)?;
    let candidate = &xml[start_index..];
    let tag_end = candidate.find('>')?;
    let open_tag = &candidate[..tag_end];
    let attribute_start = format!("{attribute}=\"");
    let value_index = open_tag.find(&attribute_start)? + attribute_start.len();
    let value = &open_tag[value_index..];
    let end_index = value.find('"')?;
    Some(value[..end_index].to_string())
}

fn extract_xml_root_attribute(xml: &str, attribute: &str) -> Option<String> {
    let root_start = xml.find('<')?;
    let candidate = &xml[root_start..];
    let root_end = candidate.find('>')?;
    let open_tag = &candidate[..root_end];
    let attribute_start = format!("{attribute}=\"");
    let value_index = open_tag.find(&attribute_start)? + attribute_start.len();
    let value = &open_tag[value_index..];
    let end_index = value.find('"')?;
    Some(value[..end_index].to_string())
}

fn normalize_token(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use homecmdr_core::capability::{MEDIA_APP, MEDIA_PLAYBACK, MEDIA_SOURCE};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    struct MockResponse {
        status_line: &'static str,
        content_type: &'static str,
        body: String,
    }

    struct MockServer {
        addr: SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<()>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock roku server");
            let addr = listener.local_addr().expect("get mock roku server address");
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

            let handle = tokio::spawn({
                let responses = Arc::clone(&responses);
                let requests = Arc::clone(&requests);
                async move {
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => break,
                            accept_result = listener.accept() => {
                                let (mut socket, _) = accept_result.expect("accept mock connection");
                                let responses = Arc::clone(&responses);
                                let requests = Arc::clone(&requests);

                                tokio::spawn(async move {
                                    let mut buffer = [0_u8; 4096];
                                    let bytes = socket.read(&mut buffer).await.expect("read request bytes");
                                    requests
                                        .lock()
                                        .expect("request log lock")
                                        .push(String::from_utf8_lossy(&buffer[..bytes]).to_string());

                                    let response = responses
                                        .lock()
                                        .expect("mock response queue lock")
                                        .pop_front()
                                        .unwrap_or(MockResponse {
                                            status_line: "HTTP/1.1 500 Internal Server Error",
                                            content_type: "text/plain",
                                            body: "no queued response".to_string(),
                                        });

                                    let reply = format!(
                                        "{}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                        response.status_line,
                                        response.content_type,
                                        response.body.len(),
                                        response.body,
                                    );

                                    let _ = socket.write_all(reply.as_bytes()).await;
                                });
                            }
                        }
                    }
                }
            });

            Self {
                addr,
                shutdown: Some(shutdown_tx),
                handle,
                requests,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("request log lock").clone()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            self.handle.abort();
        }
    }

    fn adapter_config(ip_address: String) -> RokuTvConfig {
        RokuTvConfig {
            enabled: true,
            ip_address,
            poll_interval_secs: 30,
            test_poll_interval_ms: Some(25),
        }
    }

    fn device_info_xml(power_mode: &str) -> &'static str {
        match power_mode {
            "PowerOff" => "<device-info><power-mode>PowerOff</power-mode><friendly-device-name>Living Room TV</friendly-device-name><model-name>Roku TV</model-name></device-info>",
            _ => "<device-info><power-mode>PowerOn</power-mode><friendly-device-name>Living Room TV</friendly-device-name><model-name>Roku TV</model-name></device-info>",
        }
    }

    fn active_app_xml(app_id: &str, name: &str) -> String {
        format!("<active-app><app id=\"{app_id}\" version=\"1.0.0\">{name}</app></active-app>")
    }

    fn screensaver_xml(app_id: &str, name: &str) -> String {
        format!(
            "<active-app><screensaver id=\"{app_id}\" version=\"1.0.0\">{name}</screensaver></active-app>"
        )
    }

    fn media_player_xml(state: &str, plugin_id: &str) -> String {
        format!(
            "<player state=\"{state}\"><plugin id=\"{plugin_id}\"/><position>12000 ms</position><duration>30000 ms</duration></player>"
        )
    }

    fn apps_xml() -> &'static str {
        "<apps><app id=\"12\" version=\"1.0.0\">Netflix</app><app id=\"13\" version=\"1.0.0\">Plex</app><app id=\"tvinput.hdmi1\" version=\"1.0.0\">HDMI 1</app></apps>"
    }

    #[tokio::test]
    async fn adapter_polls_roku_power_state() {
        let active_app = active_app_xml("12", "Netflix");
        let media_player = media_player_xml("play", "12");
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: active_app,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: media_player,
            },
        ])
        .await;

        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);
        let adapter = RokuTvAdapter::with_base_url(
            adapter_config(server.addr.ip().to_string()),
            format!("http://{}", server.addr),
        )
        .expect("adapter builds");

        adapter.poll_once(&registry).await.expect("poll succeeds");

        let device = registry
            .get(&roku_device_id())
            .expect("roku tv device present in registry");
        assert_eq!(device.kind, DeviceKind::Virtual);
        assert_eq!(
            device.attributes.get(POWER),
            Some(&AttributeValue::Text("on".to_string()))
        );
        assert_eq!(
            device.attributes.get(STATE),
            Some(&AttributeValue::Text("online".to_string()))
        );
        assert_eq!(
            device.attributes.get(MEDIA_APP),
            Some(&AttributeValue::Text("Netflix".to_string()))
        );
        assert_eq!(
            device.attributes.get(MEDIA_SOURCE),
            Some(&AttributeValue::Text("app".to_string()))
        );
        assert_eq!(
            device.attributes.get(MEDIA_PLAYBACK),
            Some(&AttributeValue::Text("playing".to_string()))
        );
    }

    #[tokio::test]
    async fn adapter_command_sends_power_off_keypress() {
        let active_before = active_app_xml("12", "Netflix");
        let active_after = active_app_xml("12", "Netflix");
        let media_before = media_player_xml("play", "12");
        let media_after = media_player_xml("close", "12");
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: active_before,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: media_before,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "text/plain",
                body: "".to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOff").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: active_after,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: media_after,
            },
        ])
        .await;

        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);
        let adapter = RokuTvAdapter::with_base_url(
            adapter_config(server.addr.ip().to_string()),
            format!("http://{}", server.addr),
        )
        .expect("adapter builds");

        adapter
            .poll_once(&registry)
            .await
            .expect("initial poll succeeds");
        assert!(adapter
            .command(
                &roku_device_id(),
                DeviceCommand {
                    capability: POWER.to_string(),
                    action: "off".to_string(),
                    value: None,
                    transition_secs: None,
                },
                registry.clone(),
            )
            .await
            .expect("command succeeds"));

        let requests = server.requests();
        assert!(requests
            .iter()
            .any(|request| request.starts_with("POST /keypress/PowerOff HTTP/1.1")));

        let device = registry
            .get(&roku_device_id())
            .expect("roku tv device present after command");
        assert_eq!(
            device.attributes.get(POWER),
            Some(&AttributeValue::Text("off".to_string()))
        );
        assert_eq!(
            device.attributes.get(MEDIA_PLAYBACK),
            Some(&AttributeValue::Text("stopped".to_string()))
        );
    }

    #[tokio::test]
    async fn adapter_command_launches_media_app_by_name() {
        let initial_active = active_app_xml("11", "Home");
        let initial_media = media_player_xml("close", "11");
        let final_active = active_app_xml("13", "Plex");
        let final_media = media_player_xml("play", "13");
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: initial_active,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: initial_media,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: apps_xml().to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "text/plain",
                body: "".to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: final_active,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: final_media,
            },
        ])
        .await;

        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);
        let adapter = RokuTvAdapter::with_base_url(
            adapter_config(server.addr.ip().to_string()),
            format!("http://{}", server.addr),
        )
        .expect("adapter builds");

        adapter
            .poll_once(&registry)
            .await
            .expect("initial poll succeeds");
        assert!(adapter
            .command(
                &roku_device_id(),
                DeviceCommand {
                    capability: MEDIA_APP.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Text("Plex".to_string())),
                    transition_secs: None,
                },
                registry.clone(),
            )
            .await
            .expect("command succeeds"));

        let requests = server.requests();
        assert!(requests
            .iter()
            .any(|request| request.starts_with("GET /query/apps HTTP/1.1")));
        assert!(requests
            .iter()
            .any(|request| request.starts_with("POST /launch/13 HTTP/1.1")));

        let device = registry
            .get(&roku_device_id())
            .expect("roku tv device present after app launch");
        assert_eq!(
            device.attributes.get(MEDIA_APP),
            Some(&AttributeValue::Text("Plex".to_string()))
        );
        assert_eq!(
            device.attributes.get(MEDIA_PLAYBACK),
            Some(&AttributeValue::Text("playing".to_string()))
        );
    }

    #[tokio::test]
    async fn adapter_command_switches_media_source() {
        let initial_active = active_app_xml("12", "Netflix");
        let initial_media = media_player_xml("play", "12");
        let final_active = active_app_xml("tvinput.hdmi1", "HDMI 1");
        let final_media = media_player_xml("close", "tvinput.hdmi1");
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: initial_active,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: initial_media,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "text/plain",
                body: "".to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: device_info_xml("PowerOn").to_string(),
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: final_active,
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                content_type: "application/xml",
                body: final_media,
            },
        ])
        .await;

        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus);
        let adapter = RokuTvAdapter::with_base_url(
            adapter_config(server.addr.ip().to_string()),
            format!("http://{}", server.addr),
        )
        .expect("adapter builds");

        adapter
            .poll_once(&registry)
            .await
            .expect("initial poll succeeds");
        assert!(adapter
            .command(
                &roku_device_id(),
                DeviceCommand {
                    capability: MEDIA_SOURCE.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Text("hdmi1".to_string())),
                    transition_secs: None,
                },
                registry.clone(),
            )
            .await
            .expect("command succeeds"));

        let requests = server.requests();
        assert!(requests
            .iter()
            .any(|request| request.starts_with("POST /keypress/InputHDMI1 HTTP/1.1")));

        let device = registry
            .get(&roku_device_id())
            .expect("roku tv device present after source switch");
        assert_eq!(
            device.attributes.get(MEDIA_SOURCE),
            Some(&AttributeValue::Text("hdmi1".to_string()))
        );
        assert_eq!(
            device.attributes.get(MEDIA_APP),
            Some(&AttributeValue::Text("HDMI 1".to_string()))
        );
    }

    #[test]
    fn parse_active_app_reads_app_and_screensaver_entries() {
        assert_eq!(
            parse_active_app(&active_app_xml("12", "Netflix")),
            Some(RokuActiveApp {
                id: "12".to_string(),
                name: "Netflix".to_string(),
                is_screensaver: false,
            })
        );
        assert_eq!(
            parse_active_app(&screensaver_xml("999", "Aquatic Life")),
            Some(RokuActiveApp {
                id: "999".to_string(),
                name: "Aquatic Life".to_string(),
                is_screensaver: true,
            })
        );
    }

    #[test]
    fn config_requires_ip_address() {
        let error = validate_config(&RokuTvConfig {
            enabled: true,
            ip_address: "   ".to_string(),
            poll_interval_secs: 30,
            test_poll_interval_ms: None,
        })
        .expect_err("empty ip address should fail");

        assert_eq!(
            error.to_string(),
            "adapters.roku_tv.ip_address must not be empty"
        );
    }

    #[test]
    fn parse_device_info_extracts_power_and_metadata() {
        let info = parse_device_info(device_info_xml("PowerOff")).expect("xml parses");

        assert_eq!(info.power_mode, "PowerOff");
        assert_eq!(info.friendly_name.as_deref(), Some("Living Room TV"));
        assert_eq!(info.model_name.as_deref(), Some("Roku TV"));
    }
}
