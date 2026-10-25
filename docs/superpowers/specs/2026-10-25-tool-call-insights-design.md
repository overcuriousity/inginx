# Tool-call insights — Design Spec

**Date:** 2026-10-25
**Status:** Draft, awaiting approval

## Why

The operator writes tools (function definitions that agents hand to models)
and runs agents against models behind inginx. Today the only way to see what
a model did with those tools is to open individual log entries and read raw
request/response bodies. They want to answer, at a glance:

> Which tools were called, by which client, on which model, how often — and
> what exactly went in and came out of each call?

The purpose is general inspection and judging the quality of their own tools
by looking at real calls. inginx is in a unique position for this: every
request and response already passes through it and is stored verbatim.

## Goals

1. **Counts:** number of tool calls, broken down by tool name × API key ×
   model, over a selectable time range, filterable by any of the three.
2. **Raw calls:** for any tool, a list of its individual calls, each showing
   the arguments exactly as the model emitted them, the result exactly as the
   client sent it back, the key, the model, the time, and a link to the logged
   request the call came from.

## Non-goals

- **No judging or scoring.** No error detection, retry-loop detection,
  schema validation or quality metrics. Raw data and counts only; the
  operator draws the conclusions.
- **No back-population is required.** Only traffic after the feature ships
  must be covered. (Builder may add a backfill if it falls out cheaply; it is
  not required.)
- **No parsing of text-embedded tool calls.** Only structured `tool_calls`
  are recognised. A backend that returns tool calls as plain text in
  `content` (llama-server without `--jinja`, or a model whose template lacks
  tool support) produces no tool-call rows. The operator accepted this.
- **No "offered but not called" statistics** (tools listed in `tools` that
  the model never picked).

## Anti-goals

- Must not add latency or work to the proxy hot path. Client-visible behaviour
  and bytes must not change.
- Must not break the log store's contract that statistics survive payload
  retention (see `logstore::prune` and `usage.rs`).

## Constraints

- Existing stack only: Rust, axum, rusqlite (SQLite, WAL), maud server-rendered
  pages, htmx. No new dependencies needed.
- Schema evolves via `CREATE TABLE IF NOT EXISTS` in `db.rs` `SCHEMA` (new
  tables) and `ADDED_COLUMNS` (new columns); there is no migration framework.
- Supported API surface: **OpenAI Chat Completions** (`…/chat/completions`),
  both buffered JSON and SSE-streamed responses.

## How it works

### Where extraction happens

In the log writer task (`logstore::spawn_writer` → `insert_record`), which
already receives the full request body and the full upstream response body
(`LogRecord.req_body` / `resp_body`; for streams, the captured raw SSE bytes)
and runs off the hot path. Extraction runs inside the same transaction that
inserts the `requests` row. Only records whose endpoint ends in
`/chat/completions` and whose status is 2xx are examined.

### Calls (from the response)

- **Buffered:** `choices[*].message.tool_calls[*]` → `id`,
  `function.name`, `function.arguments`.
- **Streamed:** concatenate `choices[*].delta.tool_calls[*]` fragments keyed
  by `(choice index, tool_call index)`: `id` and `function.name` arrive in an
  early fragment, `function.arguments` is the concatenation of every fragment's
  string, in order.
- Parallel calls (several in one response) each become their own row.
- `arguments` is stored as the exact string the model produced, even if it is
  not valid JSON. Pretty-printing is a display concern.
- A stream cut short (`client_disconnected`, stall) still yields whatever
  calls were assembled; arguments may be truncated. No special handling.
- **Missing or duplicate call ids.** Some backends/templates emit an empty
  `id`, or the same id twice in one response. Such calls are still recorded
  (and counted) under a synthesized id (e.g. `inginx-<request_id>-<n>`) that
  can never match a result. *(Claude's call.)*
- **Extraction must never cost the log record.** Any parse or insert failure
  in tool-call extraction is logged and skipped; the `requests` row and the
  usage rollup are written regardless. Parsing should happen before taking
  the DB connection, since the writer shares the one connection with key
  auth on the hot path.

### Results (from a later request)

Chat Completions is stateless: the client resends the conversation each turn,
and the tool's output arrives as a `{"role":"tool","tool_call_id":…,
"content":…}` message in a **later** request. When a chat-completions request
is recorded — **whatever its status**, see below — each `role: "tool"`
message in its `messages` is matched to a stored call and fills in the
result, the time it arrived, the request id it arrived in, and that
request's HTTP status.

Matching rule *(Claude's call, chosen to be safe against backends that reuse
ids like `call_0` every turn)*:

1. The same request must also contain the assistant message that issued the
   call: an `assistant` message whose `tool_calls` has an entry with that
   `id`. Its `function.name` gives the tool name. A `tool` message without
   such a sibling is ignored.
2. Candidate stored calls: same `key_id`, same call id, same tool name,
   result still empty, emitted at or before this request.
3. Of the candidates, the **newest** wins.

- Scoping to the key means call-id collisions across clients cannot
  cross-link.
- Because history is resent every turn, the same result appears in many
  later requests; once a call has a result it is no longer a candidate, so
  the first fill sticks. With unique ids (llama-server with `--jinja`
  generates random ones) this is exact; with reused ids it degrades to
  "most recent unanswered call of that tool", which is almost always right.
- **Failed result-bearing requests count.** A result is recorded even when
  the request carrying it got a 4xx/5xx (e.g. the tool output blew the
  context). Such a result is shown with the failing status beside it, since
  "my tool's output broke the next turn" is exactly what the operator wants
  to see. Call *extraction* from responses still requires 2xx.
  *(Claude's call.)*
- A `tool` message with no matching stored call (call made before the
  feature shipped, or through another route) is ignored.
- `content` may be a string or an array of content parts; store it verbatim
  as JSON text (string content stored as the plain string).
- A call that never gets a result shows as **"no result"** in the GUI.

### Storage

New table (names are the builder's call):

```
tool_calls(
  request_id   TEXT NOT NULL,   -- requests.id the call was emitted in
  call_id      TEXT NOT NULL,   -- tool_calls[].id from the model
  ts           INTEGER NOT NULL,
  key_id       INTEGER, model TEXT, tool TEXT NOT NULL,
  arguments    BLOB,            -- zstd, like req_body/resp_body
  result       BLOB,            -- zstd, NULL until matched
  result_ts    INTEGER, result_request_id TEXT, result_status INTEGER,
  PRIMARY KEY (request_id, call_id))
+ index on (key_id, call_id) for result matching
+ index on (tool, ts) for the detail page
```

Plus an hourly rollup mirroring `usage_hourly`:

```
tool_usage_hourly(key_id, model, tool, hour, n_calls, PRIMARY KEY(key_id, model, tool, hour))
```

**Retention:** `tool_calls` holds payload and is pruned alongside `requests`
(age cutoff by `ts`; size-based pruning deletes rows whose request was
deleted). `tool_usage_hourly` follows the usage-rollup retention
(`retention_usage_max_age_secs`), so counts outlive payloads exactly like
token statistics do. *(Builder's-call decision made by Claude.)*

### Admin GUI

A new **Tools** item in the top nav (`pages::layout`), between Logs and
Settings.

1. **`/admin/tools`** — overview. Range selector reusing `usage::RANGES`;
   filters for key and model. A table of tools sorted by call count, with
   columns for calls and a breakdown by key and by model (exact presentation
   is the builder's call; it must make "which tool, which key, which model,
   how often" readable in one view). Reads `tool_usage_hourly` only.
2. **`/admin/tools/{name}`** — one tool's calls, newest first, paginated like
   the logs page, same key/model filters. Each row: time, key, model, a short
   preview of arguments and result (or "no result"). Expanding a row (plain
   `<details>`) shows the full arguments and result, pretty-printed when they
   parse as JSON (reuse `logs::render_payload`-style rendering), and links to
   `/admin/logs/{request_id}` and, if present, `/admin/logs/{result_request_id}`.
   A result whose carrying request failed is marked with that status.
   A link whose request row has been pruned shows as plain text.

Visual style follows the existing pages; no new CSS framework, no JS beyond
htmx.

## Left to the builder

- Exact table/column/index names, Rust module layout (e.g. a new
  `src/toolcalls.rs` for extraction, `src/admin/tools.rs` for pages).
- Preview length, page size, tool-name URL encoding.
- Overview table layout.
- Whether to backfill from existing logs (optional, see non-goals).
- Tests: unit tests for buffered and streamed extraction (including parallel
  calls, fragmented arguments, missing/duplicate ids), result matching across
  two requests (including reused ids and a failed result-bearing request),
  retention, and a page-render smoke test like the existing admin tests.
