# adapter-roku-tv

> **Install via HomeCmdr CLI** — this crate is designed to be added to a HomeCmdr workspace.
> From your `homecmdr-api` workspace root:
> ```bash
> homecmdr pull adapter-roku-tv
> cargo build
> ```
> See [homecmdr-cli](https://github.com/homecmdr/homecmdr-cli) for installation instructions.

---

Roku TV adapter for HomeCmdr.

## What It Does

This adapter exposes one Roku TV as a single `DeviceKind::Switch` device.

Current device ID:

- `roku_tv:tv`

It uses the Roku ECP HTTP API on port `8060`.

## Canonical Capabilities Used

- `power`
- `state`

Current command support:

- `power:on`
- `power:off`
- `power:toggle`

## Config

Example:

```toml
[adapters.roku_tv]
enabled = true
ip_address = "192.168.1.114"
poll_interval_secs = 30
```

Validation:

- `ip_address` must not be empty
- `poll_interval_secs >= 1`

## Vendor Mapping

Polling:

- `GET http://<ip>:8060/query/device-info`

Commands:

- `power:on` -> `POST /keypress/PowerOn`
- `power:off` -> `POST /keypress/PowerOff`
- `power:toggle` -> `POST /keypress/Power`

## Implementation Notes

- current implementation uses a static configured IP address
- `power_mode`, `friendly_name`, and `model_name` are preserved in `metadata.vendor_specific`
- room assignment is preserved across refreshes
- the adapter re-reads device info after commands and updates canonical state

## Testing

Run:

```bash
cargo test -p adapter-roku-tv
```

This crate includes tests for:

- power-state polling normalization
- command request translation
- config validation
- device-info XML parsing
