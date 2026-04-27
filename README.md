# HomeCmdr Plugins

Official WASM plugin registry for [HomeCmdr](https://github.com/homecmdr/homecmdr-api).

## Available plugins

| Plugin | Description | Poll interval |
|--------|-------------|--------------|
| [elgato-lights](#elgato-lights) | Elgato Key Light / Key Light Air | 30 s |
| [ollama](#ollama) | Ollama local LLM service | 60 s |
| [roku-tv](#roku-tv) | Roku streaming devices and TVs | 15 s |
| [zigbee2mqtt](#zigbee2mqtt) | Zigbee2MQTT device state | 30 s |

---

## Installing a plugin

```bash
homecmdr plugin add <name>
```

This fetches the `.wasm` binary and `.plugin.toml` manifest from this repo,
prompts you for any required config values, appends the adapter config block
to your `config/default.toml`, and restarts the HomeCmdr service if it is
running.

Examples:

```bash
homecmdr plugin add elgato-lights
homecmdr plugin add ollama
homecmdr plugin add roku-tv
homecmdr plugin add zigbee2mqtt
```

### Listing installed plugins

```bash
homecmdr plugin list
```

### Removing a plugin

```bash
homecmdr plugin remove <name>
```

---

## Plugin reference

### elgato-lights

Controls [Elgato Key Light](https://www.elgato.com/key-light) and Key Light Air
devices via the [Elgato Control Center](https://www.elgato.com/downloads)
companion app HTTP API (default port 9123).

**Config**

| Key | Default | Required | Description |
|-----|---------|----------|-------------|
| `base_url` | `http://127.0.0.1:9123` | no | URL of the Control Center companion app |

**Devices** — one `light` device per physical light, indexed `0, 1, ...`:

| Device ID | Kind | Attributes |
|-----------|------|------------|
| `elgato_lights:light:0` | `light` | `power` (bool), `brightness` (0–100), `color_temperature` (kelvin) |

**Commands** — `power` (on/off/toggle), `brightness` (set), `color_temperature` (set).

> **Note:** The Elgato Control Center API requires PUT for state changes.
> `host_http` currently only supports GET, so commands are acknowledged but not
> applied.  This will be resolved when `host_http::put` is added to the WIT
> interface.

---

### ollama

Polls a local [Ollama](https://ollama.ai) instance (`GET /api/tags`) for service
health and available model metadata.

**Config**

| Key | Default | Required | Description |
|-----|---------|----------|-------------|
| `base_url` | `http://127.0.0.1:11434` | no | URL of the Ollama server |

**Devices**

| Device ID | Kind | Attributes |
|-----------|------|------------|
| `ollama:service` | `virtual` | `power` (bool), `custom.ollama.model_count` |
| `ollama:model:<name>` | `virtual` | `custom.ollama.model_name`, `custom.ollama.size_bytes`, `custom.ollama.family`, `custom.ollama.parameter_size`, `custom.ollama.quantization_level` |

**Commands** — `generate` (prompt a model).

> **Note:** Generate commands require POST to `/api/generate`.  Commands are
> acknowledged but not forwarded until `host_http::post` is available.

---

### roku-tv

Polls [Roku](https://www.roku.com) streaming devices and Roku TVs via the
[External Control Protocol](https://developer.roku.com/docs/developer-program/debugging/external-control-api.md)
(ECP, port 8060).

**Config**

| Key | Default | Required | Description |
|-----|---------|----------|-------------|
| `base_url` | — | **yes** | URL of the Roku device, e.g. `http://192.168.1.100:8060` |

**Devices**

| Device ID | Kind | Attributes |
|-----------|------|------------|
| `roku_tv:tv` | `virtual` | `power` (bool), `custom.roku_tv.model_name`, `custom.roku_tv.serial_number`, `custom.roku_tv.software_version`, `custom.roku_tv.active_app`, `custom.roku_tv.active_app_id`, `custom.roku_tv.power_mode` |

**Commands** — remote control keys (power, home, select, volume, etc.).

> **Note:** ECP keypress commands require POST to `/keypress/{key}`.  Commands
> are acknowledged but not forwarded until `host_http::post` is available.

---

### zigbee2mqtt

Polls [Zigbee2MQTT](https://www.zigbee2mqtt.io) for device state via the
built-in frontend HTTP API (`GET /api/devices`).  Requires Zigbee2MQTT 1.36+
with the web frontend enabled (the default).

**Config**

| Key | Default | Required | Description |
|-----|---------|----------|-------------|
| `base_url` | `http://127.0.0.1:8080` | no | URL of the Zigbee2MQTT frontend |

**Devices** — one device per Zigbee end-device or router (Coordinator skipped).

Device kind is inferred from the `exposes` definition: `light` → `light`,
`switch` → `switch`, otherwise `sensor`.

Standard state fields are mapped to canonical HomeCmdr attribute keys:

| Zigbee2MQTT field | HomeCmdr attribute | Notes |
|-------------------|--------------------|-------|
| `state` | `power` (bool) | `"ON"` → `true` |
| `brightness` | `brightness` (0–100) | Normalised from Zigbee 0–254 |
| `color_temp` | `color_temperature` (kelvin) | Converted from mireds |
| `temperature` | `temperature_outdoor` (measurement) | |
| `humidity` | `humidity` (measurement) | |
| `pressure` | `pressure` (measurement) | |
| `battery` | `custom.zigbee2mqtt.battery` | |
| everything else | `custom.zigbee2mqtt.<field>` | |

**Commands** — device state mutations (turn on/off, set brightness, etc.).

> **Note:** State changes require POST to the Zigbee2MQTT API (or MQTT).
> Commands are acknowledged but not forwarded until `host_http::post` is
> available.

---

## Developing a plugin

### Starter templates

Two ready-to-copy templates live in this repository:

| Directory | Use when… |
|-----------|-----------|
| [`template-wasm/`](template-wasm/) | Your plugin polls an HTTP API on a fixed interval and/or handles simple commands. Compiled to WebAssembly; sandboxed; easiest to deploy. |
| [`template-ipc/`](template-ipc/) | Your plugin needs persistent connections (MQTT, WebSocket, serial, etc.) or event-driven I/O. Compiled to a native binary; full OS access. |

**Quick start (WASM):**

```bash
cp -r template-wasm plugin-my-plugin
cd plugin-my-plugin
# Edit Cargo.toml, my_plugin.plugin.toml, and src/lib.rs
# (search for TODO comments — every placeholder is marked)
cargo build --release
cp target/wasm32-wasip2/release/plugin_my_plugin_wasm.wasm \
   <homecmdr-api>/config/plugins/my_plugin.wasm
cp my_plugin.plugin.toml \
   <homecmdr-api>/config/plugins/my_plugin.plugin.toml
```

**Quick start (IPC):**

```bash
cp -r template-ipc plugin-my-plugin
cd plugin-my-plugin
# Edit Cargo.toml, my_plugin.plugin.toml, and src/main.rs
cargo build --release
cp target/release/my-plugin-adapter \
   <homecmdr-api>/config/plugins/my-plugin-adapter
cp my_plugin.plugin.toml \
   <homecmdr-api>/config/plugins/my_plugin.plugin.toml
```

See the [Plugin Authoring Guide](https://github.com/homecmdr/homecmdr-api/blob/main/config/docs/plugin_authoring_guide.md)
in the API repo for the full workflow: WIT interface, Cargo.toml setup, build
instructions, and deployment.

The WIT interface used by all plugins:

```wit
package homecmdr:plugin@0.1.0;

world adapter {
    import host-http;   // get: func(url: string) -> result<string, string>
    import host-log;    // log: func(level: string, message: string)
    export plugin;      // name, init, poll, command
}
```

### Building a plugin binary

```bash
cd plugin-<name>
cargo build --release
# Output: target/wasm32-wasip2/release/plugin_<name>_wasm.wasm
```

Pre-built binaries are committed directly to this repo alongside their
`.plugin.toml` manifests.  The CLI downloads both files when you run
`homecmdr plugin add`.

> **CI note:** Automated builds that compile and commit the `.wasm` binaries
> are not yet configured.  Until GitHub Actions workflows are added, binaries
> must be built locally and committed manually.

---

## Registry format

`plugins.toml` is the machine-readable registry consumed by the CLI:

```toml
[[plugins]]
name         = "plugin-elgato-lights"   # canonical name (plugin-* prefix)
path         = "plugin-elgato-lights"   # directory containing .wasm + .plugin.toml
display_name = "Elgato Key Light"
description  = "One-line summary"
version      = "0.1.0"
```
