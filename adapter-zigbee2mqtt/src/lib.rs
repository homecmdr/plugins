use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use rumqttc::{
    AsyncClient, Event as MqttRuntimeEvent, EventLoop, Incoming, MqttOptions, Outgoing, QoS,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use homecmdr_core::adapter::{Adapter, AdapterFactory, RegisteredAdapterFactory};
use homecmdr_core::bus::EventBus;
use homecmdr_core::capability::{
    accumulation_value, measurement_value, BATTERY, BRIGHTNESS, COLOR_MODE, COLOR_TEMPERATURE,
    COLOR_XY, CONTACT, COVER_POSITION, COVER_TILT, CURRENT, ENERGY_MONTH, ENERGY_TODAY,
    ENERGY_TOTAL, ENERGY_YESTERDAY, HUMIDITY, LOCK, MOTION, OCCUPANCY, POWER, POWER_CONSUMPTION,
    SMOKE, STATE, TEMPERATURE, VOLTAGE, WATER_LEAK,
};
use homecmdr_core::command::DeviceCommand;
use homecmdr_core::config::AdapterConfig;
use homecmdr_core::event::Event;
use homecmdr_core::model::{AttributeValue, Attributes, Device, DeviceId, DeviceKind, Metadata};
use homecmdr_core::registry::DeviceRegistry;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{timeout, Duration};

const ADAPTER_NAME: &str = "zigbee2mqtt";
const DEFAULT_BASE_TOPIC: &str = "zigbee2mqtt";
const DEFAULT_CLIENT_ID: &str = "homecmdr-zigbee2mqtt";
const DEFAULT_KEEPALIVE_SECS: u64 = 30;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Clone, Deserialize)]
pub struct Zigbee2MqttConfig {
    pub enabled: bool,
    pub server: String,
    #[serde(default = "default_base_topic")]
    pub base_topic: String,
    #[serde(default = "default_client_id")]
    pub client_id: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_keepalive_secs")]
    pub keepalive_secs: u64,
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
}

pub struct Zigbee2MqttFactory;

static ZIGBEE2MQTT_FACTORY: Zigbee2MqttFactory = Zigbee2MqttFactory;

inventory::submit! {
    RegisteredAdapterFactory {
        factory: &ZIGBEE2MQTT_FACTORY,
    }
}

pub struct Zigbee2MqttAdapter {
    config: Zigbee2MqttConfig,
    command_timeout: Duration,
    state: Arc<Mutex<AdapterState>>,
    event_sender: broadcast::Sender<ObservedDeviceUpdate>,
    client_factory: Arc<dyn MqttClientFactory>,
}

impl Zigbee2MqttAdapter {
    pub fn new(config: Zigbee2MqttConfig) -> Result<Self> {
        validate_config(&config)?;

        let (event_sender, _) = broadcast::channel(64);
        Ok(Self {
            command_timeout: Duration::from_secs(config.command_timeout_secs),
            config,
            state: Arc::new(Mutex::new(AdapterState::default())),
            event_sender,
            client_factory: Arc::new(RumqttcClientFactory),
        })
    }

    #[cfg(test)]
    fn with_client_factory(
        config: Zigbee2MqttConfig,
        client_factory: Arc<dyn MqttClientFactory>,
    ) -> Result<Self> {
        validate_config(&config)?;

        let (event_sender, _) = broadcast::channel(64);
        Ok(Self {
            command_timeout: Duration::from_secs(config.command_timeout_secs),
            config,
            state: Arc::new(Mutex::new(AdapterState::default())),
            event_sender,
            client_factory,
        })
    }

    async fn process_message(
        &self,
        topic: &str,
        payload: &[u8],
        registry: &DeviceRegistry,
    ) -> Result<()> {
        let parsed = parse_topic(&self.config.base_topic, topic);
        let Some(topic_kind) = parsed else {
            return Ok(());
        };

        match topic_kind {
            TopicKind::BridgeState => {
                let payload: BridgeStatePayload = serde_json::from_slice(payload)
                    .context("failed to parse Zigbee2MQTT bridge state")?;
                let mut state = self.state.lock().await;
                state.bridge_online = payload.state.eq_ignore_ascii_case("online");
            }
            TopicKind::BridgeDevices => {
                let devices: Vec<BridgeDevice> = serde_json::from_slice(payload)
                    .context("failed to parse Zigbee2MQTT bridge devices")?;
                self.apply_bridge_devices(devices, registry).await?;
            }
            TopicKind::BridgeEvent => {}
            TopicKind::Availability { friendly_name } => {
                let availability = parse_availability_payload(payload)
                    .context("failed to parse Zigbee2MQTT availability payload")?;
                let changed = {
                    let mut state = self.state.lock().await;
                    state
                        .device_availability
                        .insert(friendly_name.clone(), availability.clone());

                    if let Some(observed) = state.observed_devices.get_mut(&friendly_name) {
                        observed.availability = Some(availability);
                        Some(observed.clone())
                    } else {
                        None
                    }
                };

                if let Some(observed) = changed {
                    self.upsert_observed_device(observed, registry).await?;
                }
            }
            TopicKind::DeviceState { friendly_name } => {
                let payload: Value = serde_json::from_slice(payload)
                    .context("failed to parse Zigbee2MQTT device payload")?;
                self.apply_device_payload(&friendly_name, payload, registry)
                    .await?;
            }
        }

        Ok(())
    }

    async fn apply_bridge_devices(
        &self,
        devices: Vec<BridgeDevice>,
        registry: &DeviceRegistry,
    ) -> Result<()> {
        let mut supported = HashMap::new();

        for candidate in devices {
            let Some(discovered) = DiscoveredDevice::from_bridge_device(candidate)? else {
                continue;
            };
            supported.insert(discovered.friendly_name.clone(), discovered);
        }

        let removed_ids = {
            let mut state = self.state.lock().await;
            let previous_names = state.known_devices.keys().cloned().collect::<HashSet<_>>();
            let next_names = supported.keys().cloned().collect::<HashSet<_>>();
            let removed = previous_names
                .difference(&next_names)
                .filter_map(|friendly_name| {
                    state
                        .known_devices
                        .get(friendly_name)
                        .map(|d| d.device_id.clone())
                })
                .collect::<Vec<_>>();

            state.known_devices = supported.clone();
            state
                .observed_devices
                .retain(|friendly_name, _| supported.contains_key(friendly_name));
            state
                .device_availability
                .retain(|friendly_name, _| supported.contains_key(friendly_name));

            removed
        };

        for device_id in removed_ids {
            registry.remove(&device_id).await;
        }

        let discovered_devices = {
            let state = self.state.lock().await;
            state.known_devices.clone()
        };

        for (friendly_name, discovered) in discovered_devices {
            let observed = {
                let state = self.state.lock().await;
                state.observed_devices.get(&friendly_name).cloned()
            };

            if let Some(observed) = observed {
                self.upsert_observed_device(observed, registry).await?;
            } else {
                self.upsert_discovered_device_placeholder(&discovered, registry)
                    .await?;
            }
        }

        Ok(())
    }

    async fn apply_device_payload(
        &self,
        friendly_name: &str,
        payload: Value,
        registry: &DeviceRegistry,
    ) -> Result<()> {
        let observed = {
            let mut state = self.state.lock().await;
            let Some(discovered) = state.known_devices.get(friendly_name).cloned() else {
                return Ok(());
            };
            let availability = state.device_availability.get(friendly_name).cloned();
            let observed = ObservedDevice {
                discovered,
                payload,
                availability,
            };
            state
                .observed_devices
                .insert(friendly_name.to_string(), observed.clone());
            observed
        };

        self.upsert_observed_device(observed, registry).await
    }

    async fn upsert_discovered_device_placeholder(
        &self,
        discovered: &DiscoveredDevice,
        registry: &DeviceRegistry,
    ) -> Result<()> {
        let previous = registry.get(&discovered.device_id);
        let device = build_device_from_discovered(discovered, previous.as_ref());
        registry.upsert(device).await.with_context(|| {
            format!(
                "failed to upsert placeholder for '{}'",
                discovered.device_id.0
            )
        })
    }

    async fn upsert_observed_device(
        &self,
        observed: ObservedDevice,
        registry: &DeviceRegistry,
    ) -> Result<()> {
        let previous = registry.get(&observed.discovered.device_id);
        let device = build_device_from_observed(&observed, previous.as_ref())?;
        registry.upsert(device).await.with_context(|| {
            format!(
                "failed to upsert device '{}'",
                observed.discovered.device_id.0
            )
        })?;

        let _ = self.event_sender.send(ObservedDeviceUpdate {
            device_id: observed.discovered.device_id.clone(),
            payload: observed.payload.clone(),
        });
        Ok(())
    }

    async fn handle_command(
        &self,
        device_id: &DeviceId,
        command: DeviceCommand,
        registry: DeviceRegistry,
    ) -> Result<bool> {
        let discovered = {
            let state = self.state.lock().await;
            state
                .known_devices
                .values()
                .find(|device| device.device_id == *device_id)
                .cloned()
        };
        let Some(discovered) = discovered else {
            return Ok(false);
        };

        if !supports_command(&discovered, &command) {
            return Ok(false);
        }

        let current = registry.get(device_id);
        let publish = build_command_payload(&discovered, &command)?;
        let expected = expected_command_matcher(&discovered, &command, current.as_ref())?;
        let mut receiver = self.event_sender.subscribe();
        let topic = format!(
            "{}/{}/set",
            self.config.base_topic, discovered.friendly_name
        );

        let mut command_config = self.config.clone();
        command_config.client_id = format!(
            "{}-cmd-{}",
            self.config.client_id,
            Utc::now().timestamp_millis()
        );

        let connection = self
            .client_factory
            .connect(&command_config)
            .await
            .context("failed to connect MQTT client for command")?;
        connection
            .publish(&topic, publish.to_string().into_bytes())
            .await
            .with_context(|| format!("failed to publish command to '{topic}'"))?;

        // Cover commands (open/close/stop/set) return immediately — covers
        // move on their own schedule and do not confirm via MQTT state.
        if matches!(expected, CommandExpectation::Immediate) {
            return Ok(true);
        }

        // Transition commands are handled entirely in device firmware — the bulb
        // fades on its own schedule.  Return immediately so the caller (e.g. a Lua
        // scene using ctx:sleep) can own the timing without racing the confirmation.
        if command.transition_secs.is_some() {
            return Ok(true);
        }

        let device_id = discovered.device_id.clone();
        let confirmation: Result<()> = timeout(self.command_timeout, async move {
            loop {
                let update = receiver
                    .recv()
                    .await
                    .context("command confirmation channel closed")?;
                if update.device_id != device_id {
                    continue;
                }
                if expected.matches(&update.payload) {
                    return Ok::<(), anyhow::Error>(());
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "zigbee2mqtt did not confirm '{}' command for capability '{}'",
                command.action, command.capability
            )
        })?;
        confirmation?;
        Ok(true)
    }
}

impl AdapterFactory for Zigbee2MqttFactory {
    fn name(&self) -> &'static str {
        ADAPTER_NAME
    }

    fn build(&self, config: AdapterConfig) -> Result<Option<Box<dyn Adapter>>> {
        let config: Zigbee2MqttConfig =
            serde_json::from_value(config).context("failed to parse zigbee2mqtt adapter config")?;
        validate_config(&config)?;

        if !config.enabled {
            return Ok(None);
        }

        Ok(Some(Box::new(Zigbee2MqttAdapter::new(config)?)))
    }
}

#[async_trait]
impl Adapter for Zigbee2MqttAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    async fn run(&self, registry: DeviceRegistry, bus: EventBus) -> Result<()> {
        bus.publish(Event::AdapterStarted {
            adapter: self.name().to_string(),
        });

        loop {
            let connection = match self.client_factory.connect(&self.config).await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::error!(error = %error, "Zigbee2MQTT connection failed");
                    bus.publish(Event::SystemError {
                        message: format!("zigbee2mqtt connection failed: {error}"),
                    });
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let subscriptions = [
                format!("{}/bridge/state", self.config.base_topic),
                format!("{}/bridge/devices", self.config.base_topic),
                format!("{}/bridge/event", self.config.base_topic),
                format!("{}/+/availability", self.config.base_topic),
                format!("{}/+", self.config.base_topic),
            ];
            let mut failed = false;
            for topic in subscriptions {
                if let Err(error) = connection.subscribe(&topic).await {
                    tracing::error!(error = %error, topic = %topic, "Zigbee2MQTT subscribe failed");
                    bus.publish(Event::SystemError {
                        message: format!("zigbee2mqtt subscribe failed for '{topic}': {error}"),
                    });
                    failed = true;
                    break;
                }
            }
            if failed {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            loop {
                match connection.next().await {
                    Ok(Some(message)) => {
                        if let Err(error) = self
                            .process_message(&message.topic, &message.payload, &registry)
                            .await
                        {
                            tracing::error!(error = %error, topic = %message.topic, "Zigbee2MQTT message handling failed");
                            bus.publish(Event::SystemError {
                                message: format!("zigbee2mqtt message handling failed: {error}"),
                            });
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::error!(error = %error, "Zigbee2MQTT event loop failed");
                        bus.publish(Event::SystemError {
                            message: format!("zigbee2mqtt event loop failed: {error}"),
                        });
                        break;
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
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

#[derive(Debug, Default)]
struct AdapterState {
    known_devices: HashMap<String, DiscoveredDevice>,
    observed_devices: HashMap<String, ObservedDevice>,
    device_availability: HashMap<String, String>,
    bridge_online: bool,
}

#[derive(Debug, Clone)]
struct ObservedDevice {
    discovered: DiscoveredDevice,
    payload: Value,
    availability: Option<String>,
}

#[derive(Debug, Clone)]
struct ObservedDeviceUpdate {
    device_id: DeviceId,
    payload: Value,
}

#[derive(Debug, Clone)]
struct DiscoveredDevice {
    friendly_name: String,
    device_id: DeviceId,
    device_kind: SupportedDeviceKind,
    capability_set: DeviceCapabilitySet,
    metadata: DeviceMetadata,
}

impl DiscoveredDevice {
    fn from_bridge_device(device: BridgeDevice) -> Result<Option<Self>> {
        if device.disabled || !device.supported {
            return Ok(None);
        }
        if device.device_type.eq_ignore_ascii_case("coordinator") {
            return Ok(None);
        }
        if device.friendly_name.trim().is_empty() {
            return Ok(None);
        }

        let Some(definition) = device.definition else {
            return Ok(None);
        };

        let capability_set = DeviceCapabilitySet::from_exposes(&definition.exposes);
        let device_kind = if capability_set.is_light() {
            Some(SupportedDeviceKind::Light)
        } else if capability_set.is_plug() {
            Some(SupportedDeviceKind::Plug)
        } else if capability_set.is_lock() {
            Some(SupportedDeviceKind::Lock)
        } else if capability_set.is_cover() {
            Some(SupportedDeviceKind::Cover)
        } else if capability_set.is_sensor() {
            Some(SupportedDeviceKind::Sensor)
        } else {
            None
        };
        let Some(device_kind) = device_kind else {
            return Ok(None);
        };

        Ok(Some(Self {
            device_id: DeviceId(format!(
                "{ADAPTER_NAME}:{}",
                normalize_device_suffix(&device.friendly_name)
            )),
            friendly_name: device.friendly_name,
            device_kind,
            capability_set,
            metadata: DeviceMetadata {
                ieee_address: device.ieee_address,
                model: definition.model,
                vendor: definition.vendor,
                description: definition.description,
            },
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedDeviceKind {
    Light,
    Plug,
    Sensor,
    Lock,
    Cover,
}

#[derive(Debug, Clone, Default)]
struct DeviceCapabilitySet {
    state: bool,
    brightness: bool,
    color: bool,
    color_mode: bool,
    color_temp: bool,
    power_measurement: bool,
    energy_total: bool,
    energy_today: bool,
    energy_yesterday: bool,
    energy_month: bool,
    voltage: bool,
    current: bool,
    motion: bool,
    contact: bool,
    occupancy: bool,
    smoke: bool,
    water_leak: bool,
    temperature: bool,
    humidity: bool,
    battery: bool,
    lock: bool,
    cover_position: bool,
    cover_tilt: bool,
}

impl DeviceCapabilitySet {
    fn from_exposes(exposes: &[Expose]) -> Self {
        let mut capability_set = Self::default();
        for feature in flatten_exposes(exposes) {
            match feature.property.as_deref().or(feature.name.as_deref()) {
                Some("state") => capability_set.state = true,
                Some("brightness") => capability_set.brightness = true,
                Some("color") => capability_set.color = true,
                Some("color_mode") => capability_set.color_mode = true,
                Some("color_temp") => capability_set.color_temp = true,
                Some("power") => capability_set.power_measurement = true,
                Some("energy") => capability_set.energy_total = true,
                Some("energy_today") => capability_set.energy_today = true,
                Some("energy_yesterday") => capability_set.energy_yesterday = true,
                Some("energy_month") => capability_set.energy_month = true,
                Some("voltage") => capability_set.voltage = true,
                Some("current") => capability_set.current = true,
                Some("motion") => capability_set.motion = true,
                Some("contact") => capability_set.contact = true,
                Some("occupancy") => capability_set.occupancy = true,
                Some("smoke") => capability_set.smoke = true,
                Some("water_leak") => capability_set.water_leak = true,
                Some("temperature") => capability_set.temperature = true,
                Some("humidity") => capability_set.humidity = true,
                Some("battery") => capability_set.battery = true,
                Some("lock_state") => capability_set.lock = true,
                Some("position") => capability_set.cover_position = true,
                Some("tilt") => capability_set.cover_tilt = true,
                _ => {}
            }
        }
        capability_set
    }

    fn is_light(&self) -> bool {
        self.state && self.brightness
    }

    fn is_plug(&self) -> bool {
        self.state && (self.power_measurement || self.energy_total || self.voltage || self.current)
    }

    fn is_lock(&self) -> bool {
        self.lock
    }

    fn is_cover(&self) -> bool {
        self.cover_position
    }

    fn is_sensor(&self) -> bool {
        self.motion
            || self.contact
            || self.occupancy
            || self.smoke
            || self.water_leak
            || self.temperature
            || self.humidity
    }
}

#[derive(Debug, Clone)]
struct DeviceMetadata {
    ieee_address: String,
    model: Option<String>,
    vendor: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Clone)]
enum TopicKind {
    BridgeState,
    BridgeDevices,
    BridgeEvent,
    Availability { friendly_name: String },
    DeviceState { friendly_name: String },
}

#[derive(Debug, Deserialize)]
struct BridgeStatePayload {
    state: String,
}

#[derive(Debug, Deserialize)]
struct BridgeDevice {
    ieee_address: String,
    #[serde(rename = "type")]
    device_type: String,
    supported: bool,
    disabled: bool,
    friendly_name: String,
    definition: Option<BridgeDeviceDefinition>,
}

#[derive(Debug, Deserialize)]
struct BridgeDeviceDefinition {
    model: Option<String>,
    vendor: Option<String>,
    description: Option<String>,
    #[serde(default)]
    exposes: Vec<Expose>,
}

#[derive(Debug, Clone, Deserialize)]
struct Expose {
    #[serde(default)]
    property: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    features: Vec<Expose>,
}

trait MqttClientFactory: Send + Sync {
    fn connect<'a>(
        &'a self,
        config: &'a Zigbee2MqttConfig,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn MqttConnection>>> + Send + 'a>,
    >;
}

trait MqttConnection: Send + Sync {
    fn subscribe<'a>(
        &'a self,
        topic: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

    fn publish<'a>(
        &'a self,
        topic: &'a str,
        payload: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

    fn next<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<MqttMessage>>> + Send + 'a>>;
}

#[derive(Debug, Clone)]
struct MqttMessage {
    topic: String,
    payload: Vec<u8>,
}

struct RumqttcClientFactory;

impl MqttClientFactory for RumqttcClientFactory {
    fn connect<'a>(
        &'a self,
        config: &'a Zigbee2MqttConfig,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn MqttConnection>>> + Send + 'a>,
    > {
        Box::pin(async move {
            let (host, port) = parse_mqtt_server(&config.server)?;
            let mut options = MqttOptions::new(config.client_id.clone(), host, port);
            options.set_keep_alive(Duration::from_secs(config.keepalive_secs));
            options.set_max_packet_size(256 * 1024, 256 * 1024);
            if let Some(username) = config.username.as_deref() {
                options.set_credentials(username, config.password.as_deref().unwrap_or_default());
            }

            let (client, event_loop) = AsyncClient::new(options, 10);
            let connection: Box<dyn MqttConnection> = Box::new(RumqttcConnection {
                client,
                event_loop: Mutex::new(event_loop),
            });
            Ok(connection)
        })
    }
}

struct RumqttcConnection {
    client: AsyncClient,
    event_loop: Mutex<EventLoop>,
}

impl MqttConnection for RumqttcConnection {
    fn subscribe<'a>(
        &'a self,
        topic: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.client
                .subscribe(topic, QoS::AtLeastOnce)
                .await
                .with_context(|| format!("failed to subscribe to '{topic}'"))?;
            Ok(())
        })
    }

    fn publish<'a>(
        &'a self,
        topic: &'a str,
        payload: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.client
                .publish(topic, QoS::AtLeastOnce, false, payload)
                .await
                .with_context(|| format!("failed to publish to '{topic}'"))?;

            for _ in 0..8 {
                let poll_result = tokio::time::timeout(Duration::from_millis(250), async {
                    let mut event_loop = self.event_loop.lock().await;
                    event_loop.poll().await
                })
                .await;

                match poll_result {
                    Ok(Ok(MqttRuntimeEvent::Incoming(Incoming::PubAck(_)))) => break,
                    Ok(Ok(_)) => continue,
                    Ok(Err(error)) => {
                        return Err(anyhow::anyhow!("failed to flush MQTT publish: {error}"))
                    }
                    Err(_) => break,
                }
            }

            Ok(())
        })
    }

    fn next<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<MqttMessage>>> + Send + 'a>>
    {
        Box::pin(async move {
            loop {
                let event = {
                    let mut event_loop = self.event_loop.lock().await;
                    event_loop.poll().await
                }
                .map_err(|error| anyhow::anyhow!("failed to poll MQTT event loop: {error}"))?;
                match event {
                    MqttRuntimeEvent::Incoming(Incoming::Publish(publish)) => {
                        return Ok(Some(MqttMessage {
                            topic: publish.topic,
                            payload: publish.payload.to_vec(),
                        }))
                    }
                    MqttRuntimeEvent::Outgoing(Outgoing::Disconnect) => return Ok(None),
                    _ => {}
                }
            }
        })
    }
}

#[derive(Debug)]
enum CommandExpectation {
    PowerState(&'static str),
    Brightness(i64),
    ColorTemperature(i64),
    ColorXy {
        x: f64,
        y: f64,
    },
    LockState(&'static str),
    /// Return success immediately without waiting for a confirmation message.
    Immediate,
}

impl CommandExpectation {
    fn matches(&self, payload: &Value) -> bool {
        match self {
            Self::PowerState(expected) => payload
                .get("state")
                .and_then(Value::as_str)
                .map(|value| value.eq_ignore_ascii_case(expected))
                .unwrap_or(false),
            Self::Brightness(expected) => payload
                .get("brightness")
                .and_then(Value::as_i64)
                .map(|value| value == *expected)
                .unwrap_or(false),
            Self::ColorTemperature(expected) => payload
                .get("color_temp")
                .and_then(Value::as_i64)
                .map(|value| value == *expected)
                .unwrap_or(false),
            Self::ColorXy { x, y } => payload
                .get("color")
                .and_then(Value::as_object)
                .map(|fields| {
                    fields
                        .get("x")
                        .and_then(Value::as_f64)
                        .zip(fields.get("y").and_then(Value::as_f64))
                        .map(|(actual_x, actual_y)| {
                            (actual_x - *x).abs() < 0.0001 && (actual_y - *y).abs() < 0.0001
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            Self::LockState(expected) => payload
                .get("lock_state")
                .and_then(Value::as_str)
                .map(|value| value.eq_ignore_ascii_case(expected))
                .unwrap_or(false),
            Self::Immediate => true,
        }
    }
}

fn validate_config(config: &Zigbee2MqttConfig) -> Result<()> {
    if config.server.trim().is_empty() {
        bail!("adapters.zigbee2mqtt.server must not be empty");
    }
    if config.base_topic.trim().is_empty() {
        bail!("adapters.zigbee2mqtt.base_topic must not be empty");
    }
    if config.client_id.trim().is_empty() {
        bail!("adapters.zigbee2mqtt.client_id must not be empty");
    }
    if config.keepalive_secs == 0 {
        bail!("adapters.zigbee2mqtt.keepalive_secs must be >= 1");
    }
    if config.command_timeout_secs == 0 {
        bail!("adapters.zigbee2mqtt.command_timeout_secs must be >= 1");
    }
    Ok(())
}

fn default_base_topic() -> String {
    DEFAULT_BASE_TOPIC.to_string()
}

fn default_client_id() -> String {
    DEFAULT_CLIENT_ID.to_string()
}

fn default_keepalive_secs() -> u64 {
    DEFAULT_KEEPALIVE_SECS
}

fn default_command_timeout_secs() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_SECS
}

fn parse_topic(base_topic: &str, topic: &str) -> Option<TopicKind> {
    let prefix = format!("{}/", base_topic.trim_end_matches('/'));
    let suffix = topic.strip_prefix(&prefix)?;
    match suffix {
        "bridge/state" => Some(TopicKind::BridgeState),
        "bridge/devices" => Some(TopicKind::BridgeDevices),
        "bridge/event" => Some(TopicKind::BridgeEvent),
        _ => {
            if let Some((friendly_name, leaf)) = suffix.rsplit_once('/') {
                if leaf == "availability" {
                    return Some(TopicKind::Availability {
                        friendly_name: friendly_name.to_string(),
                    });
                }
            }

            if suffix.starts_with("bridge/") || suffix.contains("/set") {
                return None;
            }

            Some(TopicKind::DeviceState {
                friendly_name: suffix.to_string(),
            })
        }
    }
}

fn parse_availability_payload(payload: &[u8]) -> Result<String> {
    let value: Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(_) => {
            let raw = std::str::from_utf8(payload).context("availability payload was not utf-8")?;
            Value::String(raw.trim().to_string())
        }
    };

    if let Some(state) = value.get("state").and_then(Value::as_str) {
        return Ok(normalize_availability(state));
    }
    if let Some(state) = value.as_str() {
        return Ok(normalize_availability(state));
    }
    bail!("availability payload did not contain a recognizable state")
}

fn normalize_availability(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "online" => "online".to_string(),
        "offline" => "offline".to_string(),
        "unavailable" => "unavailable".to_string(),
        _ => "unknown".to_string(),
    }
}

fn build_device_from_discovered(
    discovered: &DiscoveredDevice,
    previous: Option<&Device>,
) -> Device {
    let now = Utc::now();
    let attributes = Attributes::from([(
        STATE.to_string(),
        AttributeValue::Text("unknown".to_string()),
    )]);
    let metadata = build_metadata(discovered, &Map::new());
    let updated_at = previous
        .filter(|device| {
            device.kind == discovered.device_kind.as_device_kind()
                && device.attributes == attributes
                && device.metadata == metadata
        })
        .map(|device| device.updated_at)
        .unwrap_or(now);

    Device {
        id: discovered.device_id.clone(),
        room_id: previous.and_then(|device| device.room_id.clone()),
        kind: discovered.device_kind.as_device_kind(),
        attributes,
        metadata,
        updated_at,
        last_seen: now,
    }
}

fn build_device_from_observed(
    observed: &ObservedDevice,
    previous: Option<&Device>,
) -> Result<Device> {
    let now = Utc::now();
    let object = observed
        .payload
        .as_object()
        .context("expected device payload to be a JSON object")?;
    let mut attributes = Attributes::new();

    // Only Light and Plug have a meaningful on/off power toggle.
    match observed.discovered.device_kind {
        SupportedDeviceKind::Light | SupportedDeviceKind::Plug => {
            let power_state = object
                .get("state")
                .and_then(Value::as_str)
                .map(|value| {
                    if value.eq_ignore_ascii_case("on") {
                        "on"
                    } else {
                        "off"
                    }
                })
                .unwrap_or("off");
            attributes.insert(
                POWER.to_string(),
                AttributeValue::Text(power_state.to_string()),
            );
        }
        _ => {}
    }

    let availability = observed
        .availability
        .clone()
        .unwrap_or_else(|| "online".to_string());
    attributes.insert(STATE.to_string(), AttributeValue::Text(availability));

    match observed.discovered.device_kind {
        SupportedDeviceKind::Light => {
            fill_light_attributes(&mut attributes, object, &observed.discovered.capability_set)?
        }
        SupportedDeviceKind::Plug => {
            fill_plug_attributes(&mut attributes, object, &observed.discovered.capability_set)
        }
        SupportedDeviceKind::Sensor => {
            fill_sensor_attributes(&mut attributes, object, &observed.discovered.capability_set)
        }
        SupportedDeviceKind::Lock => {
            fill_lock_attributes(&mut attributes, object, &observed.discovered.capability_set)
        }
        SupportedDeviceKind::Cover => {
            fill_cover_attributes(&mut attributes, object, &observed.discovered.capability_set)
        }
    }

    let metadata = build_metadata(&observed.discovered, object);
    let updated_at = previous
        .filter(|device| {
            device.kind == observed.discovered.device_kind.as_device_kind()
                && device.attributes == attributes
                && device.metadata == metadata
        })
        .map(|device| device.updated_at)
        .unwrap_or(now);

    Ok(Device {
        id: observed.discovered.device_id.clone(),
        room_id: previous.and_then(|device| device.room_id.clone()),
        kind: observed.discovered.device_kind.as_device_kind(),
        attributes,
        metadata,
        updated_at,
        last_seen: now,
    })
}

fn fill_light_attributes(
    attributes: &mut Attributes,
    object: &Map<String, Value>,
    capability_set: &DeviceCapabilitySet,
) -> Result<()> {
    if capability_set.brightness {
        if let Some(brightness) = object.get("brightness").and_then(Value::as_i64) {
            attributes.insert(
                BRIGHTNESS.to_string(),
                AttributeValue::Integer(scale_brightness_to_percent(brightness)),
            );
        }
    }

    if capability_set.color {
        if let Some(color) = object.get("color").and_then(Value::as_object) {
            if let Some((x, y)) = color
                .get("x")
                .and_then(Value::as_f64)
                .zip(color.get("y").and_then(Value::as_f64))
            {
                attributes.insert(
                    COLOR_XY.to_string(),
                    AttributeValue::Object(HashMap::from([
                        ("x".to_string(), AttributeValue::Float(x)),
                        ("y".to_string(), AttributeValue::Float(y)),
                    ])),
                );
            }
        }
    }

    if capability_set.color_temp {
        if let Some(color_temp) = object.get("color_temp").and_then(Value::as_i64) {
            if (153..=556).contains(&color_temp) {
                attributes.insert(
                    COLOR_TEMPERATURE.to_string(),
                    AttributeValue::Object(HashMap::from([
                        ("value".to_string(), AttributeValue::Integer(color_temp)),
                        (
                            "unit".to_string(),
                            AttributeValue::Text("mireds".to_string()),
                        ),
                    ])),
                );
            }
        }
    }

    if capability_set.color_mode {
        if let Some(color_mode) = object.get("color_mode").and_then(Value::as_str) {
            let normalized = match color_mode {
                "color_temp" => Some("color_temp"),
                "xy" => Some("xy"),
                _ => None,
            };
            if let Some(mode) = normalized {
                attributes.insert(
                    COLOR_MODE.to_string(),
                    AttributeValue::Text(mode.to_string()),
                );
            }
        }
    }

    Ok(())
}

fn fill_plug_attributes(
    attributes: &mut Attributes,
    object: &Map<String, Value>,
    capability_set: &DeviceCapabilitySet,
) {
    if capability_set.power_measurement {
        if let Some(power) = object.get("power").and_then(Value::as_f64) {
            attributes.insert(
                POWER_CONSUMPTION.to_string(),
                measurement_value(power, "watt"),
            );
        }
    }
    if capability_set.energy_total {
        if let Some(energy) = object.get("energy").and_then(Value::as_f64) {
            attributes.insert(
                ENERGY_TOTAL.to_string(),
                accumulation_value(energy, "kwh", "lifetime"),
            );
        }
    }
    if capability_set.energy_today {
        if let Some(energy) = object.get("energy_today").and_then(Value::as_f64) {
            attributes.insert(
                ENERGY_TODAY.to_string(),
                accumulation_value(energy, "kwh", "day"),
            );
        }
    }
    if capability_set.energy_yesterday {
        if let Some(energy) = object.get("energy_yesterday").and_then(Value::as_f64) {
            attributes.insert(
                ENERGY_YESTERDAY.to_string(),
                accumulation_value(energy, "kwh", "day"),
            );
        }
    }
    if capability_set.energy_month {
        if let Some(energy) = object.get("energy_month").and_then(Value::as_f64) {
            attributes.insert(
                ENERGY_MONTH.to_string(),
                accumulation_value(energy, "kwh", "month"),
            );
        }
    }
    if capability_set.voltage {
        if let Some(voltage) = object.get("voltage").and_then(Value::as_f64) {
            attributes.insert(VOLTAGE.to_string(), measurement_value(voltage, "volt"));
        }
    }
    if capability_set.current {
        if let Some(current) = object.get("current").and_then(Value::as_f64) {
            attributes.insert(CURRENT.to_string(), measurement_value(current, "ampere"));
        }
    }
}

fn fill_sensor_attributes(
    attributes: &mut Attributes,
    object: &Map<String, Value>,
    capability_set: &DeviceCapabilitySet,
) {
    if capability_set.motion {
        if let Some(detected) = object.get("motion").and_then(Value::as_bool) {
            attributes.insert(
                MOTION.to_string(),
                AttributeValue::Text(if detected { "detected" } else { "clear" }.to_string()),
            );
        }
    }
    if capability_set.contact {
        if let Some(closed) = object.get("contact").and_then(Value::as_bool) {
            // In Zigbee2MQTT: contact=true means the contact is made (closed).
            attributes.insert(
                CONTACT.to_string(),
                AttributeValue::Text(if closed { "closed" } else { "open" }.to_string()),
            );
        }
    }
    if capability_set.occupancy {
        if let Some(occupied) = object.get("occupancy").and_then(Value::as_bool) {
            attributes.insert(
                OCCUPANCY.to_string(),
                AttributeValue::Text(if occupied { "occupied" } else { "unoccupied" }.to_string()),
            );
        }
    }
    if capability_set.smoke {
        if let Some(detected) = object.get("smoke").and_then(Value::as_bool) {
            attributes.insert(SMOKE.to_string(), AttributeValue::Bool(detected));
        }
    }
    if capability_set.water_leak {
        if let Some(leaking) = object.get("water_leak").and_then(Value::as_bool) {
            attributes.insert(WATER_LEAK.to_string(), AttributeValue::Bool(leaking));
        }
    }
    if capability_set.temperature {
        if let Some(temp) = object.get("temperature").and_then(Value::as_f64) {
            attributes.insert(TEMPERATURE.to_string(), measurement_value(temp, "celsius"));
        }
    }
    if capability_set.humidity {
        if let Some(hum) = object.get("humidity").and_then(Value::as_f64) {
            attributes.insert(HUMIDITY.to_string(), measurement_value(hum, "percent"));
        }
    }
    fill_battery_attribute(attributes, object, capability_set);
}

fn fill_lock_attributes(
    attributes: &mut Attributes,
    object: &Map<String, Value>,
    capability_set: &DeviceCapabilitySet,
) {
    if capability_set.lock {
        if let Some(state) = object.get("lock_state").and_then(Value::as_str) {
            let normalized = match state.to_ascii_lowercase().as_str() {
                "locked" => "locked",
                "unlocked" => "unlocked",
                "jammed" => "jammed",
                _ => "unlocked",
            };
            attributes.insert(
                LOCK.to_string(),
                AttributeValue::Text(normalized.to_string()),
            );
        }
    }
    fill_battery_attribute(attributes, object, capability_set);
}

fn fill_cover_attributes(
    attributes: &mut Attributes,
    object: &Map<String, Value>,
    capability_set: &DeviceCapabilitySet,
) {
    if capability_set.cover_position {
        if let Some(pos) = object.get("position").and_then(Value::as_i64) {
            attributes.insert(COVER_POSITION.to_string(), AttributeValue::Integer(pos));
        }
    }
    if capability_set.cover_tilt {
        if let Some(tilt) = object.get("tilt").and_then(Value::as_i64) {
            attributes.insert(COVER_TILT.to_string(), AttributeValue::Integer(tilt));
        }
    }
    fill_battery_attribute(attributes, object, capability_set);
}

fn fill_battery_attribute(
    attributes: &mut Attributes,
    object: &Map<String, Value>,
    capability_set: &DeviceCapabilitySet,
) {
    if capability_set.battery {
        if let Some(battery) = object.get("battery").and_then(Value::as_i64) {
            attributes.insert(BATTERY.to_string(), AttributeValue::Integer(battery));
        }
    }
}

fn build_metadata(discovered: &DiscoveredDevice, object: &Map<String, Value>) -> Metadata {
    let mut vendor_specific = HashMap::from([
        (
            "friendly_name".to_string(),
            Value::String(discovered.friendly_name.clone()),
        ),
        (
            "ieee_address".to_string(),
            Value::String(discovered.metadata.ieee_address.clone()),
        ),
    ]);
    if let Some(model) = discovered.metadata.model.as_ref() {
        vendor_specific.insert("model".to_string(), Value::String(model.clone()));
    }
    if let Some(vendor) = discovered.metadata.vendor.as_ref() {
        vendor_specific.insert("vendor".to_string(), Value::String(vendor.clone()));
    }
    if let Some(description) = discovered.metadata.description.as_ref() {
        vendor_specific.insert(
            "description".to_string(),
            Value::String(description.clone()),
        );
    }

    for key in [
        "linkquality",
        "color_temp_startup",
        "network_indicator",
        "outlet_control_protect",
        "overload_protection",
        "power_on_behavior",
        "update",
    ] {
        if let Some(value) = object.get(key) {
            vendor_specific.insert(key.to_string(), value.clone());
        }
    }

    Metadata {
        source: ADAPTER_NAME.to_string(),
        accuracy: None,
        vendor_specific,
    }
}

fn build_command_payload(discovered: &DiscoveredDevice, command: &DeviceCommand) -> Result<Value> {
    let mut payload = match (
        discovered.device_kind,
        command.capability.as_str(),
        command.action.as_str(),
    ) {
        (_, POWER, "on") => serde_json::json!({ "state": "ON" }),
        (_, POWER, "off") => serde_json::json!({ "state": "OFF" }),
        (_, POWER, "toggle") => serde_json::json!({ "state": "TOGGLE" }),
        (SupportedDeviceKind::Light, BRIGHTNESS, "set") => {
            let percent = command
                .value
                .as_ref()
                .and_then(|value| match value {
                    AttributeValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .context("brightness command requires integer value")?;
            serde_json::json!({ "brightness": scale_percent_to_brightness(percent) })
        }
        (SupportedDeviceKind::Light, COLOR_XY, "set") => {
            let (x, y) = parse_xy_command_value(command.value.as_ref())?;
            serde_json::json!({ "color": { "x": x, "y": y } })
        }
        (SupportedDeviceKind::Light, COLOR_TEMPERATURE, "set") => {
            let mireds = parse_color_temperature_mireds(command.value.as_ref())?;
            serde_json::json!({ "color_temp": mireds })
        }
        (SupportedDeviceKind::Lock, LOCK, "lock") => serde_json::json!({ "state": "LOCK" }),
        (SupportedDeviceKind::Lock, LOCK, "unlock") => serde_json::json!({ "state": "UNLOCK" }),
        (SupportedDeviceKind::Cover, COVER_POSITION, "open") => {
            serde_json::json!({ "state": "OPEN" })
        }
        (SupportedDeviceKind::Cover, COVER_POSITION, "close") => {
            serde_json::json!({ "state": "CLOSE" })
        }
        (SupportedDeviceKind::Cover, COVER_POSITION, "stop") => {
            serde_json::json!({ "state": "STOP" })
        }
        (SupportedDeviceKind::Cover, COVER_POSITION, "set") => {
            let pos = command
                .value
                .as_ref()
                .and_then(|value| match value {
                    AttributeValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .context("cover position set requires integer value")?;
            serde_json::json!({ "position": pos })
        }
        (SupportedDeviceKind::Cover, COVER_TILT, "set") => {
            let tilt = command
                .value
                .as_ref()
                .and_then(|value| match value {
                    AttributeValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .context("cover tilt set requires integer value")?;
            serde_json::json!({ "tilt": tilt })
        }
        _ => bail!(
            "zigbee2mqtt does not support '{}' command for capability '{}'",
            command.action,
            command.capability
        ),
    };

    // Attach the hardware transition duration when requested.  Power commands
    // do not benefit from a transition so we only add it for light-level changes.
    if let (Some(secs), Some(obj)) = (command.transition_secs, payload.as_object_mut()) {
        match command.capability.as_str() {
            BRIGHTNESS | COLOR_XY | COLOR_TEMPERATURE => {
                obj.insert("transition".to_string(), serde_json::json!(secs));
            }
            _ => {}
        }
    }

    Ok(payload)
}

fn expected_command_matcher(
    discovered: &DiscoveredDevice,
    command: &DeviceCommand,
    current: Option<&Device>,
) -> Result<CommandExpectation> {
    match (
        discovered.device_kind,
        command.capability.as_str(),
        command.action.as_str(),
    ) {
        (_, POWER, "on") => Ok(CommandExpectation::PowerState("ON")),
        (_, POWER, "off") => Ok(CommandExpectation::PowerState("OFF")),
        (_, POWER, "toggle") => {
            let current_state = current
                .and_then(|device| device.attributes.get(POWER))
                .and_then(|value| match value {
                    AttributeValue::Text(value) => Some(value.as_str()),
                    _ => None,
                })
                .unwrap_or("off");
            if current_state == "on" {
                Ok(CommandExpectation::PowerState("OFF"))
            } else {
                Ok(CommandExpectation::PowerState("ON"))
            }
        }
        (SupportedDeviceKind::Light, BRIGHTNESS, "set") => {
            let percent = command
                .value
                .as_ref()
                .and_then(|value| match value {
                    AttributeValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .context("brightness command requires integer value")?;
            Ok(CommandExpectation::Brightness(scale_percent_to_brightness(
                percent,
            )))
        }
        (SupportedDeviceKind::Light, COLOR_XY, "set") => {
            let (x, y) = parse_xy_command_value(command.value.as_ref())?;
            Ok(CommandExpectation::ColorXy { x, y })
        }
        (SupportedDeviceKind::Light, COLOR_TEMPERATURE, "set") => {
            Ok(CommandExpectation::ColorTemperature(
                parse_color_temperature_mireds(command.value.as_ref())?,
            ))
        }
        (SupportedDeviceKind::Lock, LOCK, "lock") => Ok(CommandExpectation::LockState("locked")),
        (SupportedDeviceKind::Lock, LOCK, "unlock") => {
            Ok(CommandExpectation::LockState("unlocked"))
        }
        // Cover commands return immediately — covers move asynchronously.
        (SupportedDeviceKind::Cover, COVER_POSITION, "open" | "close" | "stop" | "set") => {
            Ok(CommandExpectation::Immediate)
        }
        (SupportedDeviceKind::Cover, COVER_TILT, "set") => Ok(CommandExpectation::Immediate),
        _ => bail!(
            "zigbee2mqtt does not support '{}' command for capability '{}'",
            command.action,
            command.capability
        ),
    }
}

fn supports_command(discovered: &DiscoveredDevice, command: &DeviceCommand) -> bool {
    match (
        discovered.device_kind,
        command.capability.as_str(),
        command.action.as_str(),
    ) {
        (_, POWER, "on" | "off" | "toggle") => true,
        (SupportedDeviceKind::Light, BRIGHTNESS, "set") => discovered.capability_set.brightness,
        (SupportedDeviceKind::Light, COLOR_XY, "set") => discovered.capability_set.color,
        (SupportedDeviceKind::Light, COLOR_TEMPERATURE, "set") => {
            discovered.capability_set.color_temp
        }
        (SupportedDeviceKind::Lock, LOCK, "lock" | "unlock") => discovered.capability_set.lock,
        (SupportedDeviceKind::Cover, COVER_POSITION, "open" | "close" | "stop" | "set") => {
            discovered.capability_set.cover_position
        }
        (SupportedDeviceKind::Cover, COVER_TILT, "set") => discovered.capability_set.cover_tilt,
        _ => false,
    }
}

fn parse_xy_command_value(value: Option<&AttributeValue>) -> Result<(f64, f64)> {
    let AttributeValue::Object(fields) = value.context("xy color command requires a value")? else {
        bail!("xy color command requires an object value");
    };

    let x = match fields.get("x") {
        Some(AttributeValue::Float(value)) => *value,
        Some(AttributeValue::Integer(value)) => *value as f64,
        _ => bail!("xy color command requires numeric 'x'"),
    };
    let y = match fields.get("y") {
        Some(AttributeValue::Float(value)) => *value,
        Some(AttributeValue::Integer(value)) => *value as f64,
        _ => bail!("xy color command requires numeric 'y'"),
    };
    Ok((x, y))
}

fn parse_color_temperature_mireds(value: Option<&AttributeValue>) -> Result<i64> {
    let AttributeValue::Object(fields) =
        value.context("color temperature command requires a value")?
    else {
        bail!("color temperature command requires an object value");
    };

    let raw_value = match fields.get("value") {
        Some(AttributeValue::Integer(value)) => *value,
        _ => bail!("color temperature command requires integer 'value'"),
    };
    let unit = match fields.get("unit") {
        Some(AttributeValue::Text(unit)) => unit.as_str(),
        _ => bail!("color temperature command requires string 'unit'"),
    };

    match unit {
        "mireds" => Ok(raw_value),
        "kelvin" => Ok(kelvin_to_mireds(raw_value)),
        _ => bail!("color temperature command requires unit 'mireds' or 'kelvin'"),
    }
}

fn kelvin_to_mireds(kelvin: i64) -> i64 {
    ((1_000_000.0 / kelvin as f64).round()) as i64
}

fn scale_brightness_to_percent(value: i64) -> i64 {
    ((value as f64 / 254.0) * 100.0).round() as i64
}

fn scale_percent_to_brightness(value: i64) -> i64 {
    ((value as f64 / 100.0) * 254.0).round() as i64
}

fn flatten_exposes(exposes: &[Expose]) -> Vec<&Expose> {
    let mut flattened = Vec::new();
    for expose in exposes {
        flattened.push(expose);
        flattened.extend(flatten_exposes(&expose.features));
    }
    flattened
}

fn normalize_device_suffix(friendly_name: &str) -> String {
    friendly_name
        .chars()
        .map(|ch| if ch == '/' { '_' } else { ch })
        .collect()
}

fn parse_mqtt_server(server: &str) -> Result<(String, u16)> {
    let value = server.trim();
    let without_scheme = value
        .strip_prefix("mqtt://")
        .or_else(|| value.strip_prefix("tcp://"))
        .unwrap_or(value);
    let (host, port) = without_scheme
        .rsplit_once(':')
        .context("MQTT server must include host and port")?;
    let port = port
        .parse::<u16>()
        .context("MQTT server port must be numeric")?;
    Ok((host.to_string(), port))
}

impl SupportedDeviceKind {
    fn as_device_kind(self) -> DeviceKind {
        match self {
            Self::Light => DeviceKind::Light,
            Self::Plug => DeviceKind::Switch,
            Self::Sensor => DeviceKind::Sensor,
            Self::Lock => DeviceKind::Switch,
            Self::Cover => DeviceKind::Switch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use homecmdr_core::bus::EventBus;
    use homecmdr_core::model::RoomId;
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Default)]
    struct FakeBroker {
        published: TokioMutex<Vec<(String, Vec<u8>)>>,
        subscriptions: TokioMutex<Vec<String>>,
        incoming: TokioMutex<Vec<MqttMessage>>,
    }

    struct FakeMqttClientFactory {
        broker: Arc<FakeBroker>,
    }

    struct FakeConnection {
        broker: Arc<FakeBroker>,
    }

    impl MqttClientFactory for FakeMqttClientFactory {
        fn connect<'a>(
            &'a self,
            _config: &'a Zigbee2MqttConfig,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn MqttConnection>>> + Send + 'a>,
        > {
            let broker = Arc::clone(&self.broker);
            Box::pin(async move {
                let connection: Box<dyn MqttConnection> = Box::new(FakeConnection { broker });
                Ok(connection)
            })
        }
    }

    impl MqttConnection for FakeConnection {
        fn subscribe<'a>(
            &'a self,
            topic: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            let broker = Arc::clone(&self.broker);
            let topic = topic.to_string();
            Box::pin(async move {
                broker.subscriptions.lock().await.push(topic);
                Ok(())
            })
        }

        fn publish<'a>(
            &'a self,
            topic: &'a str,
            payload: Vec<u8>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            let broker = Arc::clone(&self.broker);
            let topic = topic.to_string();
            Box::pin(async move {
                broker.published.lock().await.push((topic, payload));
                Ok(())
            })
        }

        fn next<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<MqttMessage>>> + Send + 'a>,
        > {
            let broker = Arc::clone(&self.broker);
            Box::pin(async move {
                let mut incoming = broker.incoming.lock().await;
                if incoming.is_empty() {
                    return Ok(None);
                }
                Ok(Some(incoming.remove(0)))
            })
        }
    }

    fn adapter_config() -> Zigbee2MqttConfig {
        Zigbee2MqttConfig {
            enabled: true,
            server: "mqtt://127.0.0.1:1883".to_string(),
            base_topic: DEFAULT_BASE_TOPIC.to_string(),
            client_id: DEFAULT_CLIENT_ID.to_string(),
            username: None,
            password: None,
            keepalive_secs: DEFAULT_KEEPALIVE_SECS,
            command_timeout_secs: 1,
        }
    }

    async fn build_test_adapter(broker: Arc<FakeBroker>) -> Zigbee2MqttAdapter {
        Zigbee2MqttAdapter::with_client_factory(
            adapter_config(),
            Arc::new(FakeMqttClientFactory { broker }),
        )
        .expect("adapter builds")
    }

    fn bridge_devices_payload() -> Value {
        serde_json::json!([
            {
                "ieee_address": "0xbulb1",
                "type": "Router",
                "supported": true,
                "disabled": false,
                "friendly_name": "living_room_bulb",
                "definition": {
                    "model": "Bulb A",
                    "vendor": "Acme",
                    "description": "Color bulb",
                    "exposes": [
                        {"property": "state"},
                        {"property": "brightness"},
                        {"property": "color"},
                        {"property": "color_mode"},
                        {"property": "color_temp"}
                    ]
                }
            },
            {
                "ieee_address": "0xbulb2",
                "type": "Router",
                "supported": true,
                "disabled": false,
                "friendly_name": "desk_bulb",
                "definition": {
                    "model": "Bulb B",
                    "vendor": "Acme",
                    "description": "Color bulb",
                    "exposes": [
                        {"property": "state"},
                        {"property": "brightness"},
                        {"property": "color"},
                        {"property": "color_mode"},
                        {"property": "color_temp"}
                    ]
                }
            },
            {
                "ieee_address": "0xplug",
                "type": "Router",
                "supported": true,
                "disabled": false,
                "friendly_name": "kettle_plug",
                "definition": {
                    "model": "Plug A",
                    "vendor": "Acme",
                    "description": "Power plug",
                    "exposes": [
                        {"property": "state"},
                        {"property": "power"},
                        {"property": "energy"},
                        {"property": "energy_today"},
                        {"property": "energy_yesterday"},
                        {"property": "energy_month"},
                        {"property": "voltage"},
                        {"property": "current"}
                    ]
                }
            },
            {
                "ieee_address": "0xcoordinator",
                "type": "Coordinator",
                "supported": false,
                "disabled": false,
                "friendly_name": "Coordinator",
                "definition": null
            }
        ])
    }

    #[test]
    fn factory_returns_none_when_disabled() {
        let adapter = ZIGBEE2MQTT_FACTORY
            .build(serde_json::json!({
                "enabled": false,
                "server": "mqtt://127.0.0.1:1883"
            }))
            .expect("disabled config parses");

        assert!(adapter.is_none());
    }

    #[test]
    fn config_requires_server() {
        let error = validate_config(&Zigbee2MqttConfig {
            enabled: true,
            server: " ".to_string(),
            ..adapter_config()
        })
        .expect_err("empty server should fail");

        assert_eq!(
            error.to_string(),
            "adapters.zigbee2mqtt.server must not be empty"
        );
    }

    #[tokio::test]
    async fn bridge_devices_discover_supported_bulbs_and_plug() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        assert!(registry
            .get(&DeviceId("zigbee2mqtt:living_room_bulb".to_string()))
            .is_some());
        assert!(registry
            .get(&DeviceId("zigbee2mqtt:desk_bulb".to_string()))
            .is_some());
        let plug = registry
            .get(&DeviceId("zigbee2mqtt:kettle_plug".to_string()))
            .expect("plug exists");
        assert_eq!(plug.kind, DeviceKind::Switch);
    }

    #[tokio::test]
    async fn plug_state_maps_power_and_energy_metrics() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");
        adapter
            .process_message(
                "zigbee2mqtt/kettle_plug",
                serde_json::json!({
                    "current": 0.72,
                    "energy": 328.4,
                    "energy_month": 112.6,
                    "energy_today": 4.2,
                    "energy_yesterday": 5.1,
                    "power": 87.5,
                    "state": "OFF",
                    "voltage": 120.3,
                    "linkquality": 178
                })
                .to_string()
                .as_bytes(),
                &registry,
            )
            .await
            .expect("plug payload applies");

        let plug = registry
            .get(&DeviceId("zigbee2mqtt:kettle_plug".to_string()))
            .expect("plug exists");
        assert_eq!(
            plug.attributes.get(POWER),
            Some(&AttributeValue::Text("off".to_string()))
        );
        assert_eq!(
            plug.attributes.get(POWER_CONSUMPTION),
            Some(&measurement_value(87.5, "watt"))
        );
        assert_eq!(
            plug.attributes.get(ENERGY_TOTAL),
            Some(&accumulation_value(328.4, "kwh", "lifetime"))
        );
        assert_eq!(
            plug.attributes.get(ENERGY_TODAY),
            Some(&accumulation_value(4.2, "kwh", "day"))
        );
        assert_eq!(
            plug.attributes.get(ENERGY_YESTERDAY),
            Some(&accumulation_value(5.1, "kwh", "day"))
        );
        assert_eq!(
            plug.attributes.get(ENERGY_MONTH),
            Some(&accumulation_value(112.6, "kwh", "month"))
        );
        assert_eq!(
            plug.attributes.get(VOLTAGE),
            Some(&measurement_value(120.3, "volt"))
        );
        assert_eq!(
            plug.attributes.get(CURRENT),
            Some(&measurement_value(0.72, "ampere"))
        );
    }

    #[tokio::test]
    async fn bulb_state_maps_brightness_xy_and_color_temperature() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");
        adapter
            .process_message(
                "zigbee2mqtt/living_room_bulb",
                serde_json::json!({
                    "brightness": 254,
                    "color": {"x": 0.5504, "y": 0.4079},
                    "color_mode": "color_temp",
                    "color_temp": 556,
                    "color_temp_startup": 153,
                    "linkquality": 138,
                    "state": "ON"
                })
                .to_string()
                .as_bytes(),
                &registry,
            )
            .await
            .expect("bulb payload applies");

        let bulb = registry
            .get(&DeviceId("zigbee2mqtt:living_room_bulb".to_string()))
            .expect("bulb exists");
        assert_eq!(
            bulb.attributes.get(BRIGHTNESS),
            Some(&AttributeValue::Integer(100))
        );
        assert_eq!(
            bulb.attributes.get(COLOR_MODE),
            Some(&AttributeValue::Text("color_temp".to_string()))
        );
        assert_eq!(
            bulb.attributes.get(COLOR_TEMPERATURE),
            Some(&AttributeValue::Object(HashMap::from([
                ("value".to_string(), AttributeValue::Integer(556)),
                (
                    "unit".to_string(),
                    AttributeValue::Text("mireds".to_string())
                )
            ])))
        );
        assert_eq!(
            bulb.attributes.get(COLOR_XY),
            Some(&AttributeValue::Object(HashMap::from([
                ("x".to_string(), AttributeValue::Float(0.5504)),
                ("y".to_string(), AttributeValue::Float(0.4079)),
            ])))
        );
    }

    #[tokio::test]
    async fn command_publishes_brightness_and_confirms_state_change() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(Arc::clone(&broker)).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        let sender = adapter.event_sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send(ObservedDeviceUpdate {
                device_id: DeviceId("zigbee2mqtt:living_room_bulb".to_string()),
                payload: serde_json::json!({"brightness": 127}),
            });
        });

        assert!(adapter
            .command(
                &DeviceId("zigbee2mqtt:living_room_bulb".to_string()),
                DeviceCommand {
                    capability: BRIGHTNESS.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Integer(50)),
                    transition_secs: None,
                },
                registry,
            )
            .await
            .expect("command succeeds"));

        let published = broker.published.lock().await;
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "zigbee2mqtt/living_room_bulb/set");
        assert_eq!(
            serde_json::from_slice::<Value>(&published[0].1).expect("published payload parses"),
            serde_json::json!({"brightness": 127})
        );
    }

    #[tokio::test]
    async fn command_publishes_color_temperature_from_kelvin() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(Arc::clone(&broker)).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        let sender = adapter.event_sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send(ObservedDeviceUpdate {
                device_id: DeviceId("zigbee2mqtt:living_room_bulb".to_string()),
                payload: serde_json::json!({"color_temp": 333}),
            });
        });

        assert!(adapter
            .command(
                &DeviceId("zigbee2mqtt:living_room_bulb".to_string()),
                DeviceCommand {
                    capability: COLOR_TEMPERATURE.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Object(HashMap::from([
                        ("value".to_string(), AttributeValue::Integer(3000)),
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
            .expect("command succeeds"));

        let published = broker.published.lock().await;
        assert_eq!(
            serde_json::from_slice::<Value>(&published[0].1).expect("published payload parses"),
            serde_json::json!({"color_temp": 333})
        );
    }

    #[tokio::test]
    async fn removes_stale_devices_when_bridge_inventory_changes() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("first bridge devices applies");

        let reduced = serde_json::json!([
            {
                "ieee_address": "0xbulb1",
                "type": "Router",
                "supported": true,
                "disabled": false,
                "friendly_name": "living_room_bulb",
                "definition": {
                    "model": "Bulb A",
                    "vendor": "Acme",
                    "description": "Color bulb",
                    "exposes": [
                        {"property": "state"},
                        {"property": "brightness"}
                    ]
                }
            }
        ]);

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                reduced.to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("second bridge devices applies");

        assert!(registry
            .get(&DeviceId("zigbee2mqtt:desk_bulb".to_string()))
            .is_none());
        assert!(registry
            .get(&DeviceId("zigbee2mqtt:kettle_plug".to_string()))
            .is_none());
    }

    #[tokio::test]
    async fn preserves_room_assignment_across_refresh() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));
        registry
            .upsert_room(homecmdr_core::model::Room {
                id: RoomId("living_room".to_string()),
                name: "Living Room".to_string(),
            })
            .await;

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices applies");
        adapter
            .process_message(
                "zigbee2mqtt/living_room_bulb",
                serde_json::json!({
                    "brightness": 200,
                    "state": "ON"
                })
                .to_string()
                .as_bytes(),
                &registry,
            )
            .await
            .expect("initial payload applies");

        registry
            .assign_device_to_room(
                &DeviceId("zigbee2mqtt:living_room_bulb".to_string()),
                Some(RoomId("living_room".to_string())),
            )
            .await
            .expect("room assignment succeeds");

        adapter
            .process_message(
                "zigbee2mqtt/living_room_bulb",
                serde_json::json!({
                    "brightness": 220,
                    "state": "ON"
                })
                .to_string()
                .as_bytes(),
                &registry,
            )
            .await
            .expect("updated payload applies");

        let bulb = registry
            .get(&DeviceId("zigbee2mqtt:living_room_bulb".to_string()))
            .expect("bulb exists");
        assert_eq!(bulb.room_id, Some(RoomId("living_room".to_string())));
    }

    #[tokio::test]
    async fn identical_payload_keeps_updated_at_stable() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices applies");
        let payload = serde_json::json!({
            "brightness": 127,
            "state": "ON",
            "color": {"x": 0.4, "y": 0.5},
            "color_mode": "xy",
            "color_temp": 300
        });

        adapter
            .process_message(
                "zigbee2mqtt/living_room_bulb",
                payload.to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("first payload applies");
        let first = registry
            .get(&DeviceId("zigbee2mqtt:living_room_bulb".to_string()))
            .expect("bulb exists after first payload");

        tokio::time::sleep(Duration::from_millis(5)).await;

        adapter
            .process_message(
                "zigbee2mqtt/living_room_bulb",
                payload.to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("second payload applies");
        let second = registry
            .get(&DeviceId("zigbee2mqtt:living_room_bulb".to_string()))
            .expect("bulb exists after second payload");

        assert_eq!(second.updated_at, first.updated_at);
        assert!(second.last_seen > first.last_seen);
    }

    #[tokio::test]
    async fn command_with_transition_includes_transition_field_in_payload() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(Arc::clone(&broker)).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_devices_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        // No event sender spawned — transition mode skips confirmation wait.
        assert!(adapter
            .command(
                &DeviceId("zigbee2mqtt:living_room_bulb".to_string()),
                DeviceCommand {
                    capability: BRIGHTNESS.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Integer(0)),
                    transition_secs: Some(10.0),
                },
                registry,
            )
            .await
            .expect("command with transition succeeds"));

        let published = broker.published.lock().await;
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "zigbee2mqtt/living_room_bulb/set");
        assert_eq!(
            serde_json::from_slice::<Value>(&published[0].1).expect("published payload parses"),
            serde_json::json!({"brightness": 0, "transition": 10.0})
        );
    }

    fn bridge_sensor_payload() -> Value {
        serde_json::json!([
            {
                "ieee_address": "0xmotion1",
                "type": "EndDevice",
                "supported": true,
                "disabled": false,
                "friendly_name": "hallway_motion",
                "definition": {
                    "model": "MS-S1",
                    "vendor": "Acme",
                    "description": "Motion sensor",
                    "exposes": [
                        {"property": "motion"},
                        {"property": "battery"},
                        {"property": "temperature"},
                        {"property": "humidity"}
                    ]
                }
            },
            {
                "ieee_address": "0xcontact1",
                "type": "EndDevice",
                "supported": true,
                "disabled": false,
                "friendly_name": "front_door_contact",
                "definition": {
                    "model": "CS-1",
                    "vendor": "Acme",
                    "description": "Door/window sensor",
                    "exposes": [
                        {"property": "contact"},
                        {"property": "battery"}
                    ]
                }
            },
            {
                "ieee_address": "0xlock1",
                "type": "EndDevice",
                "supported": true,
                "disabled": false,
                "friendly_name": "front_door_lock",
                "definition": {
                    "model": "L-1",
                    "vendor": "Acme",
                    "description": "Smart lock",
                    "exposes": [
                        {"property": "lock_state"},
                        {"property": "battery"}
                    ]
                }
            },
            {
                "ieee_address": "0xcover1",
                "type": "EndDevice",
                "supported": true,
                "disabled": false,
                "friendly_name": "living_room_blind",
                "definition": {
                    "model": "CV-1",
                    "vendor": "Acme",
                    "description": "Roller blind",
                    "exposes": [
                        {"property": "position"},
                        {"property": "tilt"},
                        {"property": "battery"}
                    ]
                }
            }
        ])
    }

    #[tokio::test]
    async fn sensor_lock_cover_devices_discovered_from_bridge() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_sensor_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        let motion = registry
            .get(&DeviceId("zigbee2mqtt:hallway_motion".to_string()))
            .expect("motion sensor exists");
        assert_eq!(motion.kind, DeviceKind::Sensor);

        let contact = registry
            .get(&DeviceId("zigbee2mqtt:front_door_contact".to_string()))
            .expect("contact sensor exists");
        assert_eq!(contact.kind, DeviceKind::Sensor);

        let lock = registry
            .get(&DeviceId("zigbee2mqtt:front_door_lock".to_string()))
            .expect("lock exists");
        assert_eq!(lock.kind, DeviceKind::Switch);

        let cover = registry
            .get(&DeviceId("zigbee2mqtt:living_room_blind".to_string()))
            .expect("cover exists");
        assert_eq!(cover.kind, DeviceKind::Switch);
    }

    #[tokio::test]
    async fn sensor_state_maps_motion_temperature_humidity_battery() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_sensor_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");
        adapter
            .process_message(
                "zigbee2mqtt/hallway_motion",
                serde_json::json!({
                    "motion": true,
                    "battery": 85,
                    "temperature": 21.5,
                    "humidity": 58.0,
                    "linkquality": 120
                })
                .to_string()
                .as_bytes(),
                &registry,
            )
            .await
            .expect("motion sensor payload applies");

        let device = registry
            .get(&DeviceId("zigbee2mqtt:hallway_motion".to_string()))
            .expect("sensor exists");
        assert_eq!(
            device.attributes.get(MOTION),
            Some(&AttributeValue::Text("detected".to_string()))
        );
        assert_eq!(
            device.attributes.get(BATTERY),
            Some(&AttributeValue::Integer(85))
        );
        assert_eq!(
            device.attributes.get(TEMPERATURE),
            Some(&measurement_value(21.5, "celsius"))
        );
        assert_eq!(
            device.attributes.get(HUMIDITY),
            Some(&measurement_value(58.0, "percent"))
        );
        // Sensors have no POWER attribute
        assert!(device.attributes.get(POWER).is_none());
    }

    #[tokio::test]
    async fn contact_sensor_maps_open_closed() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(broker).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_sensor_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        // contact=false → open
        adapter
            .process_message(
                "zigbee2mqtt/front_door_contact",
                serde_json::json!({"contact": false, "battery": 90})
                    .to_string()
                    .as_bytes(),
                &registry,
            )
            .await
            .expect("contact payload applies");

        let device = registry
            .get(&DeviceId("zigbee2mqtt:front_door_contact".to_string()))
            .expect("contact sensor exists");
        assert_eq!(
            device.attributes.get(CONTACT),
            Some(&AttributeValue::Text("open".to_string()))
        );

        // contact=true → closed
        adapter
            .process_message(
                "zigbee2mqtt/front_door_contact",
                serde_json::json!({"contact": true, "battery": 90})
                    .to_string()
                    .as_bytes(),
                &registry,
            )
            .await
            .expect("contact closed payload applies");

        let device = registry
            .get(&DeviceId("zigbee2mqtt:front_door_contact".to_string()))
            .expect("contact sensor exists");
        assert_eq!(
            device.attributes.get(CONTACT),
            Some(&AttributeValue::Text("closed".to_string()))
        );
    }

    #[tokio::test]
    async fn lock_state_maps_and_commands_publish_correctly() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(Arc::clone(&broker)).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_sensor_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        // Apply initial lock state
        adapter
            .process_message(
                "zigbee2mqtt/front_door_lock",
                serde_json::json!({"lock_state": "locked", "battery": 72})
                    .to_string()
                    .as_bytes(),
                &registry,
            )
            .await
            .expect("lock state payload applies");

        let device = registry
            .get(&DeviceId("zigbee2mqtt:front_door_lock".to_string()))
            .expect("lock exists");
        assert_eq!(
            device.attributes.get(LOCK),
            Some(&AttributeValue::Text("locked".to_string()))
        );
        assert_eq!(
            device.attributes.get(BATTERY),
            Some(&AttributeValue::Integer(72))
        );

        // Send an unlock command; simulate the confirmation arriving
        let sender = adapter.event_sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send(ObservedDeviceUpdate {
                device_id: DeviceId("zigbee2mqtt:front_door_lock".to_string()),
                payload: serde_json::json!({"lock_state": "unlocked"}),
            });
        });

        assert!(adapter
            .command(
                &DeviceId("zigbee2mqtt:front_door_lock".to_string()),
                DeviceCommand {
                    capability: LOCK.to_string(),
                    action: "unlock".to_string(),
                    value: None,
                    transition_secs: None,
                },
                registry,
            )
            .await
            .expect("unlock command succeeds"));

        let published = broker.published.lock().await;
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "zigbee2mqtt/front_door_lock/set");
        assert_eq!(
            serde_json::from_slice::<Value>(&published[0].1).expect("published payload parses"),
            serde_json::json!({"state": "UNLOCK"})
        );
    }

    #[tokio::test]
    async fn cover_state_maps_position_tilt_and_commands_return_immediately() {
        let broker = Arc::new(FakeBroker::default());
        let adapter = build_test_adapter(Arc::clone(&broker)).await;
        let registry = DeviceRegistry::new(EventBus::new(16));

        adapter
            .process_message(
                "zigbee2mqtt/bridge/devices",
                bridge_sensor_payload().to_string().as_bytes(),
                &registry,
            )
            .await
            .expect("bridge devices message applies");

        adapter
            .process_message(
                "zigbee2mqtt/living_room_blind",
                serde_json::json!({"position": 45, "tilt": 30, "battery": 88})
                    .to_string()
                    .as_bytes(),
                &registry,
            )
            .await
            .expect("cover payload applies");

        let device = registry
            .get(&DeviceId("zigbee2mqtt:living_room_blind".to_string()))
            .expect("cover exists");
        assert_eq!(
            device.attributes.get(COVER_POSITION),
            Some(&AttributeValue::Integer(45))
        );
        assert_eq!(
            device.attributes.get(COVER_TILT),
            Some(&AttributeValue::Integer(30))
        );

        // Cover commands return immediately (no confirmation event needed)
        assert!(adapter
            .command(
                &DeviceId("zigbee2mqtt:living_room_blind".to_string()),
                DeviceCommand {
                    capability: COVER_POSITION.to_string(),
                    action: "set".to_string(),
                    value: Some(AttributeValue::Integer(75)),
                    transition_secs: None,
                },
                registry,
            )
            .await
            .expect("cover set position command succeeds"));

        let published = broker.published.lock().await;
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "zigbee2mqtt/living_room_blind/set");
        assert_eq!(
            serde_json::from_slice::<Value>(&published[0].1).expect("published payload parses"),
            serde_json::json!({"position": 75})
        );
    }
}
