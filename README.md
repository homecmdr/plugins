# HomeCmdr Adapters

Official adapter monorepo for [HomeCmdr](https://github.com/homecmdr/homecmdr-api).

Adapters are Rust crates that integrate external devices and services into the HomeCmdr runtime. They are installed into a local `homecmdr-api` workspace using the [HomeCmdr CLI](https://github.com/homecmdr/homecmdr-cli).

## Available Adapters

| Name | Description |
|---|---|
| [adapter-elgato-lights](adapter-elgato-lights/) | Control Elgato Key Light and Key Light Air devices over the local network |
| [adapter-ollama](adapter-ollama/) | Service-style Lua access to a local Ollama LLM instance |
| [adapter-roku-tv](adapter-roku-tv/) | Power control for a Roku TV via the ECP HTTP API |
| [adapter-zigbee2mqtt](adapter-zigbee2mqtt/) | MQTT-backed adapter for Zigbee devices via Zigbee2MQTT |

## Installing an Adapter

From the root of your `homecmdr-api` workspace:

```bash
homecmdr pull adapter-elgato-lights
cargo build
```

See [homecmdr-cli](https://github.com/homecmdr/homecmdr-cli) for CLI installation.

## How Adapters Work

HomeCmdr adapters are **compile-time** Rust crates. They self-register with the runtime via the `inventory` crate and are linked into the final binary at build time. Pulling an adapter:

1. Downloads the adapter crate into your local `crates/` directory
2. Patches your workspace `Cargo.toml` to include it as a member
3. Patches `crates/adapters/Cargo.toml` to link it into the binary

You then run `cargo build` to produce a binary with the new adapter included.

## Registry Manifest

`adapters.toml` in this repo is the machine-readable registry consumed by the CLI. Format:

```toml
[[adapters]]
name = "adapter-example"
display_name = "Example"
description = "A short description."
path = "adapter-example"
version = "0.1.0"
```

## Building Your Own Adapter

See the [Adapter Authoring Guide](https://github.com/homecmdr/homecmdr-api/blob/main/config/docs/adapter_authoring_guide.md) in `homecmdr-api`.

To submit an official adapter, open a PR adding your adapter directory and a new entry in `adapters.toml`.
