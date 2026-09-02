# inginx — Design Spec

**Date:** 2026-09-02
**Status:** Approved design, pre-implementation

## Purpose

inginx is a single-binary Rust reverse proxy for self-hosted AI inference. It
replaces LiteLLM in a setup where llama.cpp (behind llama-swap) serves
embeddings, chat/completions, TTS, STT, reranking, and (future) image
generation on a GPGPU backend. inginx exposes a 100% OpenAI-compatible API to
the internet, manages API keys through a web GUI, meters usage accurately, and
records every request/response verbatim with industry-standard export
(OpenTelemetry GenAI semantic conventions; see Vestigo issue #307).

Goals, in priority order:

1. Byte-level OpenAI API compatibility toward clients (plus `/v1/rerank`).
2. Low idle load, zero-ops deployment: one binary, one SQLite file, one TOML.
3. Accurate per-key / per-model usage tracking.
4. Full-fidelity request logging with retention and standard export.
5. An intuitive, beautiful admin GUI — server-rendered, no Node toolchain.

Non-goals (YAGNI): multi-instance clustering, per-key rate limiting/budgets,
TLS termination (delegated to the existing reverse proxy), prompt caching,
request transformation between vendor formats, per-key logging levels.

## Decisions made during brainstorming

| Topic | Decision |
|---|---|
| Storage | Embedded SQLite (WAL), payloads zstd-compressed in blobs |
| Token counting | Trust upstream `usage`; fallback to bundled Qwen3 tokenizer; record `usage_source` |
| Admin exposure | Internet-facing, password (argon2) + mandatory FIDO2/WebAuthn 2FA |
| API surface | Known routes fully handled; unknown `/v1/*` paths transparently proxied and logged |
| Logging policy | Always on, everything, global retention (max age and/or max DB size) |
| GUI stack | Server-rendered (maud) + htmx + vendored static assets; no Node anywhere |
| Architecture | Single axum/tokio process (Approach B from brainstorm) |

## Architecture

One `tokio`/`axum` process, one SQLite database, one config file. Five units:

### 1. Proxy core (`/v1/*`)

Middleware chain: bearer-key auth → model→backend resolution → forward via
`reqwest` with streaming passthrough. A tee wraps the response stream: bytes
flow to the client untouched while a copy accumulates for logging and usage
extraction.

### 2. Registry

Backends (base URL + optional upstream API key) and models. Models attach to a
backend, are discoverable from the backend's `/models` for bulk-add, and can
carry a public alias. Unknown `/v1/*` paths go to a configurable default
backend, logged raw.

### 3. Metering

- Extracts `usage` from response bodies (chat, completions, embeddings,
  rerank all report it on llama.cpp).
- Streaming chat: if the client did not set
  `stream_options.include_usage`, the proxy injects it upstream and
  **suppresses the resulting extra usage chunk** from the client stream, so
  the client sees a byte-for-byte compatible OpenAI stream while inginx gets
  exact counts.
- Fallback: Qwen3 tokenizer (HF `tokenizers` crate; `tokenizer.json` embedded
  zstd-compressed in the binary). Last resort for binary payloads: byte-based
  estimate.
- Every record stores `usage_source: upstream | estimated`.

### 4. Log store

Every request and response body stored verbatim (raw bytes; full SSE text for
streams), zstd-compressed, in SQLite blobs. A background pruner enforces
global retention: max age and/or max total DB size.

### 5. Admin GUI

Served from the same binary under `/admin`; optionally bindable to a separate
listener. `maud` templates + htmx + vendored static assets (htmx.min.js,
uPlot, hand-written CSS — dark-mode-first). Auth: argon2 password + mandatory
FIDO2/WebAuthn second factor (`webauthn-rs`), signed session cookies.

## Data model (SQLite)

- `backends(id, name, base_url, upstream_api_key, enabled)`
- `models(id, backend_id, upstream_name, alias, enabled)` — alias is the
  public name shown in `/v1/models`
- `api_keys(id, name, key_hash, key_prefix, created_at, revoked_at,
  expires_at)` — key format `igx_<base62 random>`, shown once at creation;
  only SHA-256 stored; prefix kept for display. Rotation = atomic
  revoke+create preserving model grants.
- `key_models(key_id, model_id)` + per-key "all models" flag
- `requests(id ULID, ts, key_id, model, endpoint, status, latency_ms,
  ttfb_ms, prompt_tokens, completion_tokens, usage_source, error,
  req_headers, resp_headers, req_body BLOB, resp_body BLOB)`
- `usage_hourly(key_id, model, hour, n_requests, prompt_tokens,
  completion_tokens, n_errors)` — rollup maintained in the same transaction
  as the request insert, so dashboards never scan payload rows
- `admin_credentials`, `webauthn_credentials`, `sessions`

`GET /v1/models` is filtered per key: consumers only see models their key can
use.

## Request flow

For `POST /v1/chat/completions`:

1. **Auth** — constant-time SHA-256 lookup of the bearer key.
   Revoked/expired/unknown → `401` in OpenAI's error JSON shape so client
   SDKs behave identically.
2. **Resolve** — minimal body parse (`model`, `stream`, `stream_options`
   only); alias → upstream model name + backend. Key lacks the model → `404
   model_not_found`, OpenAI shape.
3. **Forward** — body rewritten only when needed (alias mapping, usage
   injection); otherwise bytes pass through untouched. Client headers are not
   forwarded wholesale — only content-relevant headers; the upstream API key
   is attached server-side.
4. **Stream tee** — TTFB recorded at first upstream byte. On client
   disconnect mid-stream the upstream request is aborted; the partial
   response is still logged with a `client_disconnected` marker and estimated
   usage if the usage chunk never arrived.
5. **Persist** — log writes go through a bounded mpsc channel to a single
   writer task doing batched transactions; logging never adds latency to the
   hot path.

Other routes:

- **Embeddings / rerank** — `usage` present in body (llama.cpp serves
  Jina-style `/v1/rerank`); handled like non-streaming chat.
- **TTS** (`/v1/audio/speech`) — binary audio response logged as-is; usage
  estimated from input text.
- **STT** (`/v1/audio/transcriptions`) — multipart upload logged verbatim;
  retention bounds total size.
- **Images** (`/v1/images/generations`) — passthrough + raw logging, ready
  for future backends.
- **Unknown `/v1/*`** — default backend, raw logging, byte-based estimate.

Errors: upstream failures pass through with their status; a synthesized `502`
in OpenAI error shape when the backend is unreachable. Error responses are
logged like successes.

## Export

Two formats, available from the GUI (filtered by time range / key / model)
and from an authenticated admin API for scripting:

1. **OTel GenAI (OTLP/JSON, JSONL)** — one span per request following the
   GenAI semantic conventions (`gen_ai.system`, `gen_ai.request.model`,
   `gen_ai.usage.input_tokens` / `output_tokens`, timestamps, status), with
   prompt/completion content as span events. Trace/span IDs derived
   deterministically from the request ULID.
2. **Native JSONL** — one line per request with full decompressed payloads;
   the lossless format.

## GUI pages

1. **Dashboard** — requests/tokens over time, per-model and per-key
   breakdown, error rate, backend health (periodic `/models` ping).
2. **Keys** — create (name, model selection, optional expiry; key shown
   once), revoke, rotate; per-key usage sparkline.
3. **Backends** — add backend, "Import from /models" bulk-add with checkbox
   list, alias editing, enable/disable.
4. **Logs** — filterable table (key/model/status/time); detail view with
   pretty-printed JSON, raw toggle, SSE streams reassembled for readability,
   single-request copy/export.
5. **Settings** — retention policy, admin password change, passkey
   enrollment/management, export.

## Config & operations

- TOML config: listen addresses (public + optional separate admin bind), DB
  path, log level. Everything else (backends, keys, retention) lives in the
  DB, managed via the GUI.
- First run: a one-time setup URL/token is printed to stdout for creating the
  admin account and enrolling the passkey.
- Deployment: `cargo build --release`, single binary, systemd unit example in
  the README. Plain HTTP; TLS at the existing reverse proxy.

## Key crates

`axum`, `tokio`, `reqwest`, `rusqlite` (sync, accessed via the dedicated
writer task and `spawn_blocking` for reads), `maud`, `webauthn-rs`, `argon2`, `zstd`, `tokenizers`, `ulid`, `serde_json`.

## Testing

- **Unit:** usage extraction across all endpoint shapes; usage-chunk
  injection/suppression; key hashing/auth; retention pruning.
- **Integration:** mock OpenAI-compatible upstream (axum test server);
  streaming passthrough byte-fidelity; client disconnects; error passthrough.
- **Compatibility smoke:** request/response fixtures recorded from official
  OpenAI Python/JS client shapes.
