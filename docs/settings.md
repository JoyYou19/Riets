# Server settings — `CorelamoSettings.toml`

One server-wide settings file at `<data-root>/CorelamoSettings.toml`. It is a **flat TOML map of string keys to string values** (all values are quoted strings).

On first start, if the file doesn't exist, the server writes the defaults. CLI flags (`--host`, `--port`, …) fill in any missing keys.

## Precedence

By default (and `--overwrite_config false`): **config file > CLI flag > hardcoded default**.

With `--overwrite_config true`: **CLI flag > config file > default**, and the CLI values are written back to the file. `--overwrite_config` is a CLI-only flag (never persisted).

## Full default example

```toml
root-path = "/var/lib/corelamo"
name = "corelamo"
host = "0.0.0.0"
port = "6006"
format = "json"
pretty = "true"
auth = "true"
max_payload_size = "1024"
max_request_timeout = "240"
embedding-model = ""
embedding-device = "auto"
embedding-runtime = "bge-m3"
embedding-instances = "1"
embedding-batch-size = "64"
embedding-max-tokens = "512"
embedding-max-chars = "4096"
```

## Keys

| Key | Default | Meaning |
|---|---|---|
| `root-path` | `/var/lib/corelamo` | Root directory for all databases and config. |
| `name` | `corelamo` | Instance name. |
| `host` | `0.0.0.0` | Host address to bind the HTTP server to. |
| `port` | `6006` | HTTP port. |
| `format` | `json` | Default request/response/document format. |
| `pretty` | `true` | Pretty-print JSON responses. |
| `auth` | `true` | Enable/disable authentication entirely. |
| `max_payload_size` | `1024` | Max request payload size, in **MB**. |
| `max_request_timeout` | `240` | Max time before responding, in **seconds**. |
| `embedding-model` | `bge-m3` | Embedding model for `$vector` fields, shared by all databases. |
| `embedding-device` | `auto` | `auto`, `cpu`, `cuda`, `mps`, … |
| `embedding-runtime` | `` | Path to the ONNX runtime library; empty = auto-install to `<root>/runtime`. |
| `embedding-instances` | `1` | Number of model instances. |
| `embedding-batch-size` | `64` | Documents per embedding call. |
| `embedding-max-tokens` | `512` | Max tokens per embedded text. |
| `embedding-max-chars` | `4096` | Max characters per embedded text. |

## CLI flags

Every key (except `root-path` special-casing) is also a `--key value` flag, e.g. `--port 6007 --auth false`. Run `corelamo-runtime --help` for the full list. Flags do not change the file unless `--overwrite_config true`.

## Related files in the data root

- `CorelamoSettings.toml` — this file.
- `CorelamoDictionary.toml` — the default synonym dictionary (see `dictionary.md`).
- `models/` — downloaded embedding models.
- `runtime/` — the auto-installed embedding runtime.
- `databases/<db>/` — one directory per database (its `policy.json`, `config.toml`, `dictionary.toml`, …).
