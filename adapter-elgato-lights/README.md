# adapter-elgato-lights

> **Install via HomeCmdr CLI** — this crate is designed to be added to a HomeCmdr workspace.
> From your `homecmdr-api` workspace root:
> ```bash
> homecmdr pull adapter-elgato-lights
> cargo build
> ```
> See [homecmdr-cli](https://github.com/homecmdr/homecmdr-cli) for installation instructions.

---

Elgato Light adapter for HomeCmdr.

## What It Does

This adapter polls the Elgato Light HTTP API and exposes one logical HomeCmdr light device per Elgato light index.

Example device IDs:

- `elgato_lights:light:0`
- `elgato_lights:light:1`

All devices are `DeviceKind::Light`.

## Canonical Capabilities Used

- `power`
- `state`
- `brightness`
- `color_temperature`

`color_temperature` is normalized to canonical `kelvin` values in the public API.

## Config

Example:

```toml
[adapters.elgato_lights]
enabled = true
base_url = "http://127.0.0.1:9123"
poll_interval_secs = 30
```

Validation:

- `base_url` must not be empty
- `poll_interval_secs >= 1`

## Command Support

This adapter supports canonical commands for:

- `power`: `on`, `off`, `toggle`
- `brightness`: `set`
- `color_temperature`: `set`

Vendor-specific narrowing:

- canonical `color_temperature` is accepted only in `kelvin`
- supported range is currently `2900..=7000`

## Implementation Notes

- stale upstream lights are removed from the registry
- `light_index` is stored in `metadata.vendor_specific`
- room assignment is preserved across refreshes
- post-command state is confirmed before the registry is updated

## Testing

Run:

```bash
cargo test -p adapter-elgato-lights
```

This crate includes tests for:

- polling normalization
- command application
- request translation
- vendor-specific command rejection
- adapter start and event flow
