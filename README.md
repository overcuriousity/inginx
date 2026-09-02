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

## Quickstart

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
```

Backends, models, keys, and retention are managed in the GUI and stored in
the database.

- With `admin_listen` set, the public listener serves **only** `/v1/*` and
  the admin GUI is reachable solely on the admin listener.
- inginx speaks plain HTTP; terminate TLS at your reverse proxy. Set
  `X-Forwarded-Proto: https` so WebAuthn derives the right origin (or set RP
  ID/origin under *Settings*).

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

## systemd

See `examples/inginx.service`:

```console
$ sudo cp target/release/inginx /usr/local/bin/
$ sudo cp examples/inginx.service /etc/systemd/system/
$ sudo systemctl enable --now inginx
$ journalctl -u inginx | grep setup    # first-run setup URL
```
