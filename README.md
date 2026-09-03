# inginx

A single-binary, OpenAI-compatible inference proxy for self-hosted AI backends
(llama.cpp, llama-swap, vLLM, …). Replaces heavier gateways like LiteLLM for
the common single-operator case:

- **100% OpenAI-compatible API** toward clients — chat, completions,
  embeddings, TTS, STT, images, plus Jina/Cohere-style `/v1/rerank`. Unknown
  `/v1/*` routes are transparently proxied to your default backend.
- **API key management** in a web GUI: create, scope to models, revoke,
  rotate. Keys are hashed (SHA-256); the plaintext is shown exactly once.
- **Exact usage metering**: inginx trusts the `usage` object your backend
  reports. For streams where the client didn't ask for usage, inginx injects
  `stream_options.include_usage` upstream and silently strips the extra chunk
  from the client stream — clients see a byte-compatible OpenAI stream,
  you get exact token counts. Fallback: a bundled Qwen3 tokenizer estimate
  (each request is marked `upstream` or `estimated`).
- **Full-fidelity request logging**: every request/response body stored
  verbatim (zstd-compressed) in SQLite, browsable in the GUI, with
  age/size-based retention and export as **native JSONL** or **OpenTelemetry
  GenAI (OTLP/JSON) JSONL**.
- **Admin GUI** (dark, server-rendered, no Node anywhere) protected by an
  argon2 password plus optional **FIDO2/WebAuthn passkey 2FA**.

Everything lives in one process, one SQLite file, one optional TOML file.

## Install

One line, x86_64 Linux, installs the binary to `/usr/local/bin` and sets up a
systemd service:

```console
$ curl -fsSL https://raw.githubusercontent.com/overcuriousity/inginx/main/install.sh | sudo bash
```

It downloads the latest tagged release (falling back to the rolling `nightly`
build of `main`), verifies its SHA-256, writes
`/etc/systemd/system/inginx.service`, and runs `systemctl enable --now inginx`.
Knobs:

```console
$ curl -fsSL .../install.sh | sudo INGINX_CHANNEL=nightly bash   # track main
$ curl -fsSL .../install.sh | sudo INGINX_VERSION=v0.1.0 bash    # pin a tag
$ curl -fsSL .../install.sh | sudo INGINX_NO_SERVICE=1 bash      # binary only
$ curl -fsSL .../install.sh | sudo INGINX_PREFIX=/opt/bin bash   # other prefix
```

Read the script before piping it into a root shell:
<https://github.com/overcuriousity/inginx/blob/main/install.sh>.

Then follow the setup URL:

```console
$ journalctl -u inginx | grep setup
```

## Quickstart (from source)

```console
$ cargo build --release
$ ./target/release/inginx
==> No admin account yet. Create one at: http://0.0.0.0:8080/admin/setup?t=<token>
```

1. Open the setup URL, create the admin account (and enroll a passkey under
   *Settings* — until you do, login is password-only).
2. *Backends* → add your backend, e.g. `http://10.0.0.5:9292/v1`
   (the OpenAI-compatible base **including** `/v1`), then **Import from
   /models** to bulk-add its models. Edit aliases as you like — the alias is
   the public model name.
3. *API Keys* → create a key, pick its models, copy the `igx_…` secret.
4. Point any OpenAI client at inginx:

```python
from openai import OpenAI
client = OpenAI(base_url="https://ai.example.com/v1", api_key="igx_...")
client.chat.completions.create(model="qwen3-8b", messages=[...])
```

## Configuration

`inginx.toml` (or the path in `INGINX_CONFIG`); all fields optional:

```toml
listen = "0.0.0.0:8080"          # public API + admin GUI
# admin_listen = "127.0.0.1:8081"  # optional: admin GUI on a separate listener
db_path = "inginx.db"
log_level = "info"

upstream_timeout_secs = 300      # wait for upstream response headers (0 = none)
upstream_stream_idle_secs = 120  # max silence inside a streaming response
upstream_retry_on_5xx = true     # retry once when the backend answers 502/503
api_auth_max_failures = 20       # failed /v1 keys from one IP before lockout
api_auth_lockout_secs = 300      # base lockout, doubles while it keeps failing
```

Backends, models, keys, and retention are managed in the GUI and stored in
the database.

- With `admin_listen` set, the public listener serves **only** `/v1/*` and
  the admin GUI is reachable solely on the admin listener.
- inginx speaks plain HTTP; terminate TLS at your reverse proxy. Set
  `X-Forwarded-Proto: https` so WebAuthn derives the right origin (or set RP
  ID/origin under *Settings*).
- A backend that accepts a request and then goes quiet is answered with a
  `504 upstream_timeout` after `upstream_timeout_secs`, and the attempt is
  logged. Without that ceiling such a request hangs until the client gives up
  and never reaches the log at all. `upstream_retry_on_5xx` covers the other
  half of a model swapper's behaviour: a 502/503 that means "still starting"
  is retried once, before any byte has reached the client.
- Upstream failures that are not OpenAI-shaped (llama-swap answers a failed
  model start with plain text) are wrapped in `{"error": {...}}` on the way
  out, keeping the backend's own wording as the message. The untouched body
  stays in the request log.
- Failed `/v1` authentications are logged and rate-limited per source IP, on
  the same doubling lockout as the admin login.

### Voice maps

Backends name TTS voices their own way — Kokoro wants `af_alloy`, Piper wants
a language code — while every stock OpenAI client sends `alloy`. Each model
carries an optional voice map, edited in *Backends* beside its alias:

```
alloy=af_alloy, echo=am_echo, fable=bm_fable, onyx=am_onyx, nova=af_nova, shimmer=af_sky
```

The `voice` field of a JSON request to that model is rewritten before it is
forwarded. A voice with no entry is passed through untouched.

## Retention & export

*Settings* → retention prunes request logs hourly by max age and/or a total
database size cap. Export (GUI or `GET /admin/export?format=native|otel&from=&to=&key=&model=`,
admin session required) produces:

- `native` — one JSON object per request with full decompressed payloads.
- `otel` — one OTLP/JSON `resourceSpans` line per request following the
  OpenTelemetry GenAI semantic conventions (`gen_ai.request.model`,
  `gen_ai.usage.input_tokens`, prompt/completion span events, deterministic
  trace IDs).

## Notes

- STT/TTS and other binary payloads are logged verbatim; retention keeps
  growth in check.
- A request body of up to 512 MB is accepted (audio uploads).
- The admin GUI uses vendored htmx + uPlot; the Qwen3 tokenizer is embedded
  in the binary. `cargo build` needs no network beyond crates.io.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT), at your option.

## systemd

The install script writes the unit for you. To do it by hand, see
`examples/inginx.service`:

```console
$ sudo cp target/release/inginx /usr/local/bin/
$ sudo cp examples/inginx.service /etc/systemd/system/
$ sudo systemctl enable --now inginx
$ journalctl -u inginx | grep setup    # first-run setup URL
```
