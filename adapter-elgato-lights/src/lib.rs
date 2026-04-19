use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use homecmdr_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use homecmdr_core::bus::EventBus;
use homecmdr_core::capability::{BRIGHTNESS, COLOR_TEMPERATURE, POWER, STATE};
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::config::AdapterConfig;
use homecmdr_core::event::Event;
use homecmdr_core::http::{external_http_client, send_with_retry};
use homecmdr_core::model::{AttributeValue, Attributes, Device, DeviceId, DeviceKind, Metadata};
use homecmdr_core::registry::DeviceRegistry;
use tokio::time::{sleep, Duration};

const ADAPTER_NAME: &str = "elgato_lights";

#[derive(Debug, Clone, Deserialize)]
pub struct ElgatoLightsConfig {
    pub enabled: bool,
    pub base_url: String,
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub test_poll_interval_ms: Option<u64>,
}

pub struct ElgatoLightsFactory;

static ELGATO_LIGHTS_FACTORY: ElgatoLightsFactory = ElgatoLightsFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &ELGATO_LIGHTS_FACTORY,
    }
}

pub struct ElgatoLightsAdapter {
    client: Client,
    config: ElgatoLightsConfig,
    poll_interval: Duration,
    known_lights: RwLock<HashSet<usize>>,
}

impl ElgatoLightsAdapter {
    pub fn new(config: ElgatoLightsConfig) -> Result<Self> {
        let poll_interval = config
            .test_poll_interval_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(config.poll_interval_secs));

        Ok(Self {
            client: external_http_client()?,
            config,
            poll_interval,
            known_lights: RwLock::new(HashSet::new()),
        })
    }

    async fn poll_once(&self, registry: &DeviceRegistry) -> Result<()> {
        let response = self.fetch_lights().await?;
        let mut seen = HashSet::new();

        for (index, light) in response.lights.into_iter().enumerate() {
            let previous = registry.get(&device_id(index));
            let device = build_device(index, light, previous.as_ref(), "online");
            seen.insert(index);
            registry
                .upsert(device)
                .await
                .with_context(|| format!("failed to upsert Elgato light '{index}'"))?;
        }

        let stale_ids = {
            let mut known = self
                .known_lights
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let stale = known.difference(&seen).copied().collect::<Vec<_>>();
            *known = seen;
            stale
        };

        for index in stale_ids {
            registry.remove(&device_id(index)).await;
        }

        Ok(())
    }

    async fn mark_devices_unavailable(&self, registry: &DeviceRegistry) {
        let known = self
            .known_lights
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for index in known {
            if let Some(mut device) = registry.get(&device_id(index)) {
                let already_unavailable = device
                    .attributes
                    .get(STATE)
                    .and_then(|v| match v {
                        AttributeValue::Text(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .map(|s| s == "unavailable")
                    .unwrap_or(false);
                if !already_unavailable {
                    device.attributes.insert(
                        STATE.to_string(),
                        AttributeValue::Text("unavailable".to_string()),
                    );
                    device.updated_at = Utc::now();
                    let _ = registry.upsert(device).await;
                }
            }
        }
    }

    async fn fetch_lights(&self) -> Result<ElgatoLightsResponse> {
        send_with_retry(
            self.client.get(format!(
                "{}/elgato/lights",
                self.config.base_url.trim_end_matches('/')
            )),
            "Elgato light state",
        )
        .await?
        .json()
        .await
        .context("failed to parse Elgato light state response")
    }

    async fn update_lights(&self, request: &ElgatoLightsResponse) -> Result<()> {
        send_with_retry(
            self.client
                .put(format!(
                    "{}/elgato/lights",
                    self.config.base_url.trim_end_matches('/')
                ))
                .json(request),
            "Elgato light update",
        )
        .await?;

        Ok(())
    }

    async fn handle_command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: DeviceRegistry,
    ) -> Result<bool> {
        let Some(index) = parse_light_index(device_id) else {
            return Ok(false);
        };

        validate_elgato_command(&command)?;

        let current = registry
            .get(device_id)
            .with_context(|| format!("device '{}' not found in registry", device_id.0))?;

        let mut request = self.fetch_lights().await?;
        let light = request
            .lights
            .get_mut(index)
            .with_context(|| format!("light index {index} not returned by Elgato API"))?;

        apply_command(light, &current, &command)?;
        self.update_lights(&request).await?;

        let updated_light = self
            .wait_for_command_application(index, &current, &command)
            .await?;

        registry
            .upsert(build_device(index, updated_light, Some(&current), "online"))
            .await
            .with_context(|| {
                format!(
                    "failed to update registry for '{}': command applied",
                    device_id.0
                )
            })?;

        Ok(true)
    }

    async fn wait_for_command_application(
        &self,
        index: usize,
        current: &Device,
        command: &DeviceCommand,
    ) -> Result<ElgatoLight> {
        for _ in 0..5 {
            let response = self.fetch_lights().await?;
            let Some(light) = response.lights.get(index).cloned() else {
                bail!("light index {index} not returned by Elgato API after command");
            };

            if command_matches_light_state(&light, current, command)? {
                return Ok(light);
            }

            sleep(Duration::from_millis(50)).await;
        }

        bail!(
            "elgato_lights did not apply '{}' command for capability '{}'",
            command.action,
            command.capability
        )
    }
}

impl AdapterFactory for ElgatoLightsFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: ElgatoLightsConfig = serde_json::from_value(config)
            .context("failed to parse elgato_lights adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(ElgatoLightsAdapter::new(config)?)))
    }
}

#[async_trait]
impl Adapter for ElgatoLightsAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            if let Err(error) = self.poll_once(&registry).await {
                tracing::error!(error = %error, "Elgato light poll failed");
                bus.publish(Event::SystemError {
                    message: format!("elgato_lights poll failed: {error}"),
                });
                self.mark_devices_unavailable(&registry).await;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ElgatoLightsResponse {
    #[serde(rename = "numberOfLights")]
    number_of_lights: usize,
    lights: Vec<ElgatoLight>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ElgatoLight {
    on: u8,
    brightness: i64,
    temperature: i64,
}

fn validate_config(config: &ElgatoLightsConfig) -> Result<()> {
    if config.base_url.trim().is_empty() {
        bail!("adapters.elgato_lights.base_url must not be empty");
    }

    if config.poll_interval_secs == 0 && config.test_poll_interval_ms.is_none() {
        bail!("adapters.elgato_lights.poll_interval_secs must be >= 1");
    }

    Ok(())
}

fn build_device(
    index: usize,
    light: ElgatoLight,
    previous: Option<&Device>,
    state: &str,
) -> Device {
    let now = Utc::now();
    let attributes = Attributes::from([
        (
            POWER.to_string(),
            AttributeValue::Text(if light.on == 0 { "off" } else { "on" }.to_string()),
        ),
        (STATE.to_string(), AttributeValue::Text(state.to_string())),
        (
            BRIGHTNESS.to_string(),
            AttributeValue::Integer(light.brightness),
        ),
        (
            COLOR_TEMPERATURE.to_string(),
            AttributeValue::Object(HashMap::from([
                (
                    "value".to_string(),
                    AttributeValue::Integer(api_temperature_to_kelvin(light.temperature)),
                ),
                (
                    "unit".to_string(),
                    AttributeValue::Text("kelvin".to_string()),
                ),
            ])),
        ),
    ]);
    let metadata = Metadata {
        source: ADAPTER_NAME.to_string(),
        accuracy: None,
        vendor_specific: HashMap::from([("light_index".to_string(), serde_json::json!(index))]),
    };
    let updated_at = previous
        .filter(|device| {
            device.kind == DeviceKind::Light
                && device.attributes == attributes
                && device.metadata == metadata
        })
        .map(|device| device.updated_at)
        .unwrap_or(now);

    Device {
        id: device_id(index),
        room_id: previous.and_then(|device| device.room_id.clone()),
        kind: DeviceKind::Light,
        attributes,
        metadata,
        updated_at,
        last_seen: now,
    }
}

fn apply_command(light: &mut ElgatoLight, current: &Device, command: &DeviceCommand) -> Result<()> {
    match (command.capability.as_str(), command.action.as_str()) {
        (POWER, "on") => {
            light.on = 1;
        }
        (POWER, "off") => {
            light.on = 0;
        }
        (POWER, "toggle") => {
            let current_power = current
                .attributes
                .get(POWER)
                .and_then(|value| match value {
                    AttributeValue::Text(power) => Some(power.as_str()),
                    _ => None,
                })
                .unwrap_or("off");
            light.on = if current_power == "on" { 0 } else { 1 };
        }
        (BRIGHTNESS, "set") => {
            let value = command
                .value
                .as_ref()
                .and_then(|value| match value {
                    AttributeValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .context("brightness command requires integer value")?;
            light.brightness = value;
        }
        (COLOR_TEMPERATURE, "set") => {
            let value = command
                .value
                .as_ref()
                .context("color temperature command requires a value")?;
            let (kelvin, unit) = parse_temperature_value(value)?;
            if unit != "kelvin" {
                bail!("elgato_lights only accepts canonical kelvin input");
            }
            ensure_kelvin_supported(kelvin)?;
            light.temperature = kelvin_to_api_temperature(kelvin);
        }
        _ => return Ok(()),
    }

    Ok(())
}

fn validate_elgato_command(command: &DeviceCommand) -> Result<()> {
    if command.capability == COLOR_TEMPERATURE && command.action == "set" {
        let value = command
            .value
            .as_ref()
            .context("color temperature command requires a value")?;
        let (kelvin, unit) = parse_temperature_value(value)?;
        if unit != "kelvin" {
            bail!("elgato_lights only accepts canonical kelvin input");
        }
        ensure_kelvin_supported(kelvin)?;
    }

    Ok(())
}

fn command_matches_light_state(
    light: &ElgatoLight,
    current: &Device,
    command: &DeviceCommand,
) -> Result<bool> {
    Ok(
        match (command.capability.as_str(), command.action.as_str()) {
            (POWER, "on") => light.on == 1,
            (POWER, "off") => light.on == 0,
            (POWER, "toggle") => {
                let current_power = current
                    .attributes
                    .get(POWER)
                    .and_then(|value| match value {
                        AttributeValue::Text(power) => Some(power.as_str()),
                        _ => None,
                    })
                    .unwrap_or("off");
                if current_power == "on" {
                    light.on == 0
                } else {
                    light.on == 1
                }
            }
            (BRIGHTNESS, "set") => {
                let expected = command
                    .value
                    .as_ref()
                    .and_then(|value| match value {
                        AttributeValue::Integer(value) => Some(*value),
                        _ => None,
                    })
                    .context("brightness command requires integer value")?;
                light.brightness == expected
            }
            (COLOR_TEMPERATURE, "set") => {
                let value = command
                    .value
                    .as_ref()
                    .context("color temperature command requires a value")?;
                let (kelvin, unit) = parse_temperature_value(value)?;
                if unit != "kelvin" {
                    bail!("elgato_lights only accepts canonical kelvin input");
                }
                light.temperature == kelvin_to_api_temperature(kelvin)
            }
            _ => false,
        },
    )
}

fn parse_temperature_value(value: &AttributeValue) -> Result<(i64, &str)> {
    let AttributeValue::Object(fields) = value else {
        bail!("color temperature command requires an object value");
    };

    let kelvin = match fields.get("value") {
        Some(AttributeValue::Integer(value)) => *value,
        _ => bail!("color temperature command requires integer 'value'"),
    };
    let unit = match fields.get("unit") {
        Some(AttributeValue::Text(unit)) => unit.as_str(),
        _ => bail!("color temperature command requires string 'unit'"),
    };

    Ok((kelvin, unit))
}

fn ensure_kelvin_supported(kelvin: i64) -> Result<()> {
    if !(2900..=7000).contains(&kelvin) {
        bail!("elgato_lights supports color temperature between 2900 and 7000 kelvin");
    }

    Ok(())
}

fn api_temperature_to_kelvin(value: i64) -> i64 {
    (value * 20).max(2900)
}

fn kelvin_to_api_temperature(value: i64) -> i64 {
    ((value as f64) * 0.05).round() as i64
}

fn device_id(index: usize) -> DeviceId {
    DeviceId(format!("{ADAPTER_NAME}:light:{index}"))
}

fn parse_light_index(device_id: &DeviceId) -> Option<usize> {
    let (adapter, suffix) = device_id.0.split_once(':')?;
    if adapter != ADAPTER_NAME {
        return None;
    }

    let (_, index) = suffix.split_once(':')?;
    index.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    struct MockResponse {
        status_line: &'static str,
        body: &'static str,
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
                .expect("bind mock server");
            let addr = listener.local_addr().expect("get mock server address");
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
                                            body: "{\"error\":\"no queued response\"}",
                                        });

                                    let reply = format!(
                                        "{}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                        response.status_line,
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

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
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

    fn adapter_config(base_url: String) -> ElgatoLightsConfig {
        ElgatoLightsConfig {
            enabled: true,
            base_url,
            poll_interval_secs: 30,
            test_poll_interval_ms: Some(25),
        }
    }

    #[tokio::test]
    async fn adapter_polls_and_normalizes_elgato_light_state() {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":213}]}",
        }])
        .await;
        let adapter =
            ElgatoLightsAdapter::new(adapter_config(server.base_url())).expect("adapter builds");
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter.poll_once(&registry).await.expect("poll succeeds");

        let device = registry
            .get(&DeviceId("elgato_lights:light:0".to_string()))
            .expect("light device exists");
        assert_eq!(device.kind, DeviceKind::Light);
        assert_eq!(
            device.attributes.get(POWER),
            Some(&AttributeValue::Text("on".to_string()))
        );
        assert_eq!(
            device.attributes.get(BRIGHTNESS),
            Some(&AttributeValue::Integer(20))
        );
        assert_eq!(
            device.attributes.get(COLOR_TEMPERATURE),
            Some(&AttributeValue::Object(HashMap::from([
                ("value".to_string(), AttributeValue::Integer(4260)),
                (
                    "unit".to_string(),
                    AttributeValue::Text("kelvin".to_string())
                ),
            ])))
        );
    }

    #[tokio::test]
    async fn adapter_command_updates_light_and_registry() {
        let server = MockServer::start(vec![
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":213}]}",
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":213}]}",
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":213}]}",
            },
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":350}]}",
            },
        ])
        .await;
        let adapter =
            ElgatoLightsAdapter::new(adapter_config(server.base_url())).expect("adapter builds");
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .poll_once(&registry)
            .await
            .expect("initial poll succeeds");
        let command = DeviceCommand {
            capability: COLOR_TEMPERATURE.to_string(),
            action: "set".to_string(),
            value: Some(AttributeValue::Object(HashMap::from([
                ("value".to_string(), AttributeValue::Integer(7000)),
                (
                    "unit".to_string(),
                    AttributeValue::Text("kelvin".to_string()),
                ),
            ]))),
            transition_secs: None,
        };

        assert!(adapter
            .command(
                &DeviceId("elgato_lights:light:0".to_string()),
                command,
                registry.clone()
            )
            .await
            .expect("command succeeds"));

        let device = registry
            .get(&DeviceId("elgato_lights:light:0".to_string()))
            .expect("light device exists after command");
        assert_eq!(
            device.attributes.get(COLOR_TEMPERATURE),
            Some(&AttributeValue::Object(HashMap::from([
                ("value".to_string(), AttributeValue::Integer(7000)),
                (
                    "unit".to_string(),
                    AttributeValue::Text("kelvin".to_string())
                ),
            ])))
        );

        let requests = server.requests();
        assert!(requests
            .iter()
            .any(|request| request.starts_with("PUT /elgato/lights HTTP/1.1")));
        assert!(requests
            .iter()
            .any(|request| request.contains("\"temperature\":350")));
    }

    #[tokio::test]
    async fn adapter_rejects_out_of_range_kelvin_command() {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":213}]}",
        }])
        .await;
        let adapter =
            ElgatoLightsAdapter::new(adapter_config(server.base_url())).expect("adapter builds");
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .poll_once(&registry)
            .await
            .expect("initial poll succeeds");
        let error = adapter
            .command(
                &DeviceId("elgato_lights:light:0".to_string()),
                DeviceCommand {
                    capability: COLOR_TEMPERATURE.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Object(HashMap::from([
                        ("value".to_string(), AttributeValue::Integer(8000)),
                        (
                            "unit".to_string(),
                            AttributeValue::Text("kelvin".to_string()),
                        ),
                    ]))),
                    transition_secs: None,
                },
                registry,
            )
            .await
            .err()
            .expect("out of range kelvin should fail");

        assert_eq!(
            error.to_string(),
            "elgato_lights supports color temperature between 2900 and 7000 kelvin"
        );
    }

    #[tokio::test]
    async fn adapter_run_publishes_started_and_devices() {
        let server = MockServer::start(vec![MockResponse {
            status_line: "HTTP/1.1 200 OK",
            body: "{\"numberOfLights\":1,\"lights\":[{\"on\":0,\"brightness\":10,\"temperature\":170}]}",
        }])
        .await;
        let adapter = Arc::new(
            ElgatoLightsAdapter::new(adapter_config(server.base_url())).expect("adapter builds"),
        );
        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus.clone());
        let mut subscriber = bus.subscribe();

        let adapter_task = {
            let adapter = Arc::clone(&adapter);
            let registry = registry.clone();
            let bus = bus.clone();
            tokio::spawn(async move { adapter.run(registry, bus).await })
        };

        assert_eq!(
            subscriber.recv().await.expect("adapter started event"),
            Event::AdapterStarted {
                adapter: "elgato_lights".to_string(),
            }
        );

        timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .get(&DeviceId("elgato_lights:light:0".to_string()))
                    .is_some()
                {
                    break;
                }

                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("light appears in registry after polling");

        adapter_task.abort();
        let _ = adapter_task.await;
    }

    #[tokio::test]
    async fn adapter_marks_devices_unavailable_after_poll_failure() {
        let server = MockServer::start(vec![
            // First poll: success — device appears as "online"
            MockResponse {
                status_line: "HTTP/1.1 200 OK",
                body: "{\"numberOfLights\":1,\"lights\":[{\"on\":1,\"brightness\":20,\"temperature\":213}]}",
            },
            // Second poll: server error
            MockResponse {
                status_line: "HTTP/1.1 500 Internal Server Error",
                body: "{\"error\":\"down\"}",
            },
        ])
        .await;
        let adapter = Arc::new(
            ElgatoLightsAdapter::new(adapter_config(server.base_url())).expect("adapter builds"),
        );
        let bus = EventBus::new(16);
        let registry = DeviceRegistry::new(bus.clone());

        // First poll — device should be "online"
        adapter
            .poll_once(&registry)
            .await
            .expect("first poll succeeds");
        let device = registry
            .get(&DeviceId("elgato_lights:light:0".to_string()))
            .expect("device exists after first poll");
        assert_eq!(
            device.attributes.get(STATE),
            Some(&AttributeValue::Text("online".to_string()))
        );

        // Second poll — server error → device should become "unavailable"
        adapter
            .poll_once(&registry)
            .await
            .expect_err("second poll fails");
        adapter.mark_devices_unavailable(&registry).await;

        let device = registry
            .get(&DeviceId("elgato_lights:light:0".to_string()))
            .expect("device still in registry");
        assert_eq!(
            device.attributes.get(STATE),
            Some(&AttributeValue::Text("unavailable".to_string()))
        );
    }
}
