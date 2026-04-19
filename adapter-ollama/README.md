# adapter-ollama

> **Install via HomeCmdr CLI** — this crate is designed to be added to a HomeCmdr workspace.
> From your `homecmdr-api` workspace root:
> ```bash
> homecmdr pull adapter-ollama
> cargo build
> ```
> See [homecmdr-cli](https://github.com/homecmdr/homecmdr-cli) for installation instructions.

---

`adapter-ollama` adds service-style Lua access to a local Ollama instance.

## Config

Example:

```toml
[adapters.ollama]
enabled = true
base_url = "http://127.0.0.1:11434"
model = "llava"
```

Fields:

- `enabled`: enable the adapter
- `base_url`: Ollama HTTP base URL
- `model`: default model to use for invoke targets when the payload omits `model`

## Lua Invoke Targets

This adapter currently owns these invoke targets:

- `ollama:generate`
- `ollama:vision`
- `ollama:chat`
- `ollama:embeddings`
- `ollama:tags`
- `ollama:ps`
- `ollama:show`
- `ollama:version`

Vision example:

```lua
local result = ctx:invoke("ollama:vision", {
  prompt = "Reply only true or false. Are clothes on the clothesline?",
  image_base64 = snapshot_base64,
})

if result.boolean == true then
  -- take action
end
```

Chat example:

```lua
local result = ctx:invoke("ollama:chat", {
  messages = {
    {
      role = "system",
      content = "Be concise.",
    },
    {
      role = "user",
      content = "Summarize the weather in one sentence.",
    },
  },
})

local reply = result.message.content
```

Embeddings example:

```lua
local result = ctx:invoke("ollama:embeddings", {
  input = {
    "front door open",
    "back door open",
  },
})
```

## Target Summaries

`ollama:generate`

- required: `prompt`
- optional: `model`, `suffix`, `system`, `template`, `format`, `options`, `keep_alive`, `raw`, `images`

`ollama:vision`

- required: `prompt`, `image_base64`
- optional: `model`, `system`, `template`, `format`, `options`, `keep_alive`, `raw`

`ollama:chat`

- required: `messages`
- optional: `model`, `format`, `options`, `keep_alive`, `tools`

`messages` is a Lua list of message tables with fields like `role`, `content`, `images`, `tool_calls`, and `tool_name`.

`ollama:embeddings`

- required: `input`
- optional: `model`, `keep_alive`, `options`, `truncate`

`input` can be a string or a Lua list of strings.

`ollama:tags`

- no payload required

`ollama:ps`

- no payload required

`ollama:show`

- optional: `model`, `verbose`

`ollama:version`

- no payload required

## Response Notes

- `generate` and `vision` return `response`, `boolean`, `done`, and timing fields when present
- `chat` returns `message`, `done`, and timing fields when present
- `embeddings` returns nested numeric arrays in `embeddings`
- `tags` and `ps` return `models`
- `show` returns the raw JSON object converted into Lua tables/lists
- `version` returns `version`
