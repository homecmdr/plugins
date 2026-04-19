# Zigbee2MQTT Adapter

> **Install via HomeCmdr CLI** — this crate is designed to be added to a HomeCmdr workspace.
> From your `homecmdr-api` workspace root:
> ```bash
> homecmdr pull adapter-zigbee2mqtt
> cargo build
> ```
> See [homecmdr-cli](https://github.com/homecmdr/homecmdr-cli) for installation instructions.

---

MQTT-backed adapter for Zigbee2MQTT devices.

## V1 Scope

Supported device classes:

- bulbs with `state`, `brightness`, `color`, `color_mode`, and `color_temp`
- smart plugs with `state`, `power`, `energy`, `energy_today`, `energy_yesterday`, `energy_month`, `voltage`, and `current`

Supported commands:

- bulbs: `power`, `brightness`, `color_xy`, `color_temperature`
- plugs: `power`

## Config

```toml
[adapters.zigbee2mqtt]
enabled = true
server = "mqtt://127.0.0.1:1883"
base_topic = "zigbee2mqtt"
client_id = "homecmdr-zigbee2mqtt"
keepalive_secs = 30
command_timeout_secs = 5
```

Optional credentials:

```toml
username = "mqtt-user"
password = "mqtt-password"
```
