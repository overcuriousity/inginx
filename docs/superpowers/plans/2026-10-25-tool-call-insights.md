# Tool-call Insights Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record every structured tool call passing through `/chat/completions` (arguments from the response, result from a later request) and show counts by tool × key × model plus the raw calls in a new admin **Tools** page.

**Architecture:** A pure parsing module (`src/toolcalls.rs`) turns a `LogRecord`'s bodies into calls and results. The log writer runs it *before* taking the DB connection and stores the outcome inside `insert_record`'s transaction under a savepoint, so a failure never costs the log row. Two new tables: `tool_calls` (payload, pruned with `requests`) and `tool_usage_hourly` (rollup, pruned with usage). A new `src/admin/tools.rs` renders an overview and a per-tool call list.

**Tech Stack:** Rust 2021, axum 0.8, rusqlite 0.40 (SQLite WAL), maud 0.26, htmx, serde_json (with `preserve_order`), zstd.

**Spec:** `docs/superpowers/specs/2026-10-25-tool-call-insights-design.md`

## Global Constraints

- No new dependencies.
- Proxy hot path and client-visible bytes unchanged: nothing in `src/proxy.rs` changes.
- Only endpoints ending in `/chat/completions`. Calls are extracted only from 2xx responses; results are read from requests of **any** status.
- `arguments` and `result` stored verbatim (zstd-compressed like `req_body`), never re-serialized.
- Tool-call extraction or storage failure must never lose the `requests` row or the `usage_hourly` update.
- Synthesized id for a missing/empty/duplicate call id: `inginx-<request_id>-<n>` (`n` = 0-based position of the call in the response).
- Result matching: same request must contain an `assistant` message whose `tool_calls` has that id (name taken from there); candidates are same `key_id`, same `call_id`, same tool, `result IS NULL`, `ts <=` the request's ts; newest wins.
- Schema changes only via `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS` in `db.rs` `SCHEMA`.
- New nav item "Tools" between Logs and Settings; key `"tools"`.
- Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Streamed tool call whose `id`/`name` arrive only in the first fragment and arguments split across many events, interleaved for two parallel calls** → both calls reassembled with correct arguments (Task 1 test `stream_parallel_interleaved`).
2. **`tool` message `content` as an array of content parts, or `null`** → stored as its JSON text, or the empty string for null, without error (Task 1 test `result_content_shapes`).
3. **A tool name containing `/`, spaces or non-ASCII (`mcp__fs/read file`)** → detail URL round-trips and the page finds the calls (Task 5 test `tool_page_odd_name`).
4. **Request body that is not JSON, or JSON whose `messages` is not an array** → no results, no panic, log row written (Task 2 test `garbage_bodies_still_log`).
5. **Result arrives for a call whose request row was already pruned by size** → `tool_calls` row is gone too, so the result is ignored silently (Task 3 test `size_prune_drops_orphans`).

---

### Task 1: Parsing module

**Files:**
- Create: `src/toolcalls.rs`
- Modify: `src/lib.rs` (add `pub mod toolcalls;`)
- Test: inline `#[cfg(test)] mod tests` in `src/toolcalls.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Default)]
  pub struct Call { pub call_id: String, pub tool: String, pub arguments: String }
  #[derive(Debug, Clone, PartialEq, Default)]
  pub struct ToolResult { pub call_id: String, pub tool: String, pub content: String }
  #[derive(Debug, Default)]
  pub struct Extracted { pub calls: Vec<Call>, pub results: Vec<ToolResult> }

  pub fn calls_from_response(resp_body: &[u8], request_id: &str) -> Vec<Call>;
  pub fn results_from_request(req_body: &[u8]) -> Vec<ToolResult>;
  /// Applies the endpoint/status rules from Global Constraints.
  pub fn extract(rec: &crate::logstore::LogRecord) -> Extracted;
  ```

- [ ] **Step 1: Write the failing tests**

  - `buffered_single_call`: body `{"choices":[{"message":{"tool_calls":[{"id":"c1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Bonn\"}"}}]}}]}` → `[Call{call_id:"c1", tool:"get_weather", arguments:"{\"city\":\"Bonn\"}"}]`.
  - `buffered_parallel_calls`: two entries in `tool_calls` → two `Call`s in order.
  - `stream_parallel_interleaved`: SSE with events (each `data: {...}\n\n`, ending `data: [DONE]`): idx0 `{id:"a",function:{name:"read",arguments:""}}`, idx1 `{id:"b",function:{name:"write",arguments:"{\"p"}}`, idx0 `{function:{arguments:"{\"f\":1"}}`, idx1 `{function:{arguments:"\":2}"}}`, idx0 `{function:{arguments:"}"}}` (all under `choices[0].delta.tool_calls`) → `[Call{"a","read","{\"f\":1}"}, Call{"b","write","{\"p\":2}"}]`.
  - `invalid_json_arguments_kept_verbatim`: `arguments:"{oops"` → stored as `"{oops"`.
  - `missing_and_duplicate_ids_synthesized`: request_id `"R"`, calls with ids `""`, `"x"`, `"x"` → ids `"inginx-R-0"`, `"x"`, `"inginx-R-2"`.
  - `no_tool_calls`: plain content response, empty body, non-JSON → `vec![]`.
  - `results_need_assistant_sibling`: messages `[assistant{tool_calls:[{id:"c1",function:{name:"t"}}]}, tool{tool_call_id:"c1",content:"ok"}, tool{tool_call_id:"zz",content:"orphan"}]` → `[ToolResult{"c1","t","ok"}]`.
  - `result_content_shapes`: content `[{"type":"text","text":"hi"}]` → content is that array's compact JSON text; content `null` → `""`.
  - `extract_rules`: `LogRecord` with endpoint `/v1/chat/completions`, status 200 → calls and results; status 400 → no calls but results; endpoint `/v1/embeddings` → both empty.

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test toolcalls::`
  Expected: compile error (module missing).

- [ ] **Step 3: Implement the module**

  Parse with `serde_json::Value`. SSE detection: body (as UTF-8) trimmed starts with `data:` (same rule as `admin::logs::render_payload`); split events on `data:` lines, skip `[DONE]`. Reassembly keys on `(choice index, tool_call index)` in a `BTreeMap` so output order is choice then index; first non-empty `id`/`name` wins, `arguments` concatenates. Synthesize ids after assembly. Result `content`: `Value::String(s)` → `s`; `Null`/missing → `""`; anything else → `v.to_string()`.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test toolcalls::`
  Expected: all pass.

- [ ] **Step 5: Commit**

  ```bash
  git add src/toolcalls.rs src/lib.rs
  git commit -m "feat(toolcalls): parse tool calls and results from chat traffic"
  ```

### Task 2: Schema and storage in the log writer

**Files:**
- Modify: `src/db.rs` (`SCHEMA`)
- Modify: `src/logstore.rs` (`insert_record`, `spawn_writer`, tests)
- Modify: `src/toolcalls.rs` (add `store`)

**Interfaces:**
- Consumes: `toolcalls::{extract, Extracted}` from Task 1.
- Produces:
  - Tables:
    ```sql
    CREATE TABLE IF NOT EXISTS tool_calls (
      request_id TEXT NOT NULL, call_id TEXT NOT NULL, ts INTEGER NOT NULL,
      key_id INTEGER, model TEXT, tool TEXT NOT NULL,
      arguments BLOB, result BLOB, result_ts INTEGER,
      result_request_id TEXT, result_status INTEGER,
      PRIMARY KEY (request_id, call_id));
    CREATE INDEX IF NOT EXISTS idx_tool_calls_match ON tool_calls(key_id, call_id);
    CREATE INDEX IF NOT EXISTS idx_tool_calls_tool ON tool_calls(tool, ts);
    CREATE TABLE IF NOT EXISTS tool_usage_hourly (
      key_id INTEGER NOT NULL, model TEXT NOT NULL, tool TEXT NOT NULL, hour INTEGER NOT NULL,
      n_calls INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (key_id, model, tool, hour));
    ```
    `tool_usage_hourly` uses the same sentinels as `usage_hourly`: `key_id` `-1` and `model` `""` for none.
  - `pub fn store(c: &rusqlite::Connection, rec: &LogRecord, x: &Extracted) -> anyhow::Result<()>` in `toolcalls.rs`: inserts calls (`INSERT OR IGNORE`), bumps the rollup, then fills results with the matching rule from Global Constraints (one `UPDATE … WHERE rowid = (SELECT rowid … ORDER BY ts DESC LIMIT 1)` per result).
  - `logstore::insert_record(c: &mut Connection, rec: &LogRecord, tools: &Extracted) -> Result<()>`: after the existing two statements, opens `tx.savepoint()`, runs `store`; on `Err` logs `tracing::warn!` and drops the savepoint (rollback), then commits the outer transaction.
  - `spawn_writer`: computes `let tools = toolcalls::extract(&rec);` **before** `db.call`.

- [ ] **Step 1: Write the failing tests** (in `logstore.rs` tests; helper `chat_rec(id, ts, key, status, req_json, resp_json) -> LogRecord`)

  - `call_then_result_is_matched`: request A (200) responds with call `c1`/`t`; request B (200, later ts) sends `[assistant{tool_calls c1 t}, tool c1 "out"]` → one `tool_calls` row, `result` decompresses to `"out"`, `result_request_id = B`, `result_status = 200`; `tool_usage_hourly.n_calls = 1`.
  - `resent_history_does_not_overwrite`: after the above, request C resends the same history with tool content `"changed"` → result still `"out"`, `result_request_id` still B.
  - `reused_ids_pick_newest_open_call`: A emits `call_0`/`t`, B sends result for `call_0` and emits a new `call_0`/`t`, C sends both results (history + new, contents `"first"`, `"second"`) → A's call result `"first"` (from B), B's call result `"second"` (from C).
  - `other_key_cannot_match`: call by key 1, result in a request by key 2 → result stays NULL.
  - `failed_request_result_recorded`: result-bearing request has status 400 → result filled, `result_status = 400`.
  - `garbage_bodies_still_log`: req body `b"not json"` and `{"messages":5}`, endpoint chat completions → `requests` has the row, `usage_hourly` counted, no `tool_calls` rows.
  - `store_failure_keeps_log_row`: drop table `tool_calls` before insert of a call-bearing record → `requests` row exists, `usage_hourly` counted.
  - Update existing `insert_record` calls in tests to pass `&Extracted::default()`.

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test logstore::`
  Expected: compile error / failures.

- [ ] **Step 3: Implement schema, `store`, `insert_record`, `spawn_writer` changes** as specified in Interfaces.

- [ ] **Step 4: Run all tests**

  Run: `cargo test`
  Expected: all pass (existing 95 plus new).

- [ ] **Step 5: Commit**

  ```bash
  git add src/db.rs src/logstore.rs src/toolcalls.rs
  git commit -m "feat(toolcalls): store tool calls and match their results"
  ```

### Task 3: Retention

**Files:**
- Modify: `src/logstore.rs` (`prune`, tests)

**Interfaces:**
- Consumes: tables from Task 2. `prune` signature unchanged.

- [ ] **Step 1: Write the failing tests**

  - `age_prune_covers_tool_calls`: tool call rows at `now()-30d` and `now()`, `prune(Some(7d), None, None)` → one `tool_calls` row left; `tool_usage_hourly` keeps both hours. Then `prune(None, None, Some(7d))` → one rollup row left.
  - `size_prune_drops_orphans`: insert a call-bearing record, delete its `requests` row via the size loop (call `prune` with `max_db_bytes = Some(1)`) → `tool_calls` empty; a later result for that call id changes nothing and does not error.

- [ ] **Step 2: Run to verify failure** — `cargo test logstore::` → the two new tests fail.

- [ ] **Step 3: Implement**

  Age branch: also `DELETE FROM tool_calls WHERE ts < ?1`. Usage branch: also `DELETE FROM tool_usage_hourly WHERE hour < ?1`. Size loop: after each batch delete, `DELETE FROM tool_calls WHERE request_id NOT IN (SELECT id FROM requests)` before the vacuum.

- [ ] **Step 4: Run** — `cargo test` → all pass.

- [ ] **Step 5: Commit**

  ```bash
  git add src/logstore.rs
  git commit -m "feat(toolcalls): prune tool calls with request retention"
  ```

### Task 4: Query layer

**Files:**
- Create: `src/admin/tools.rs` (queries now, pages in Task 5)
- Modify: `src/admin/mod.rs` (`pub mod tools;`)
- Modify: `src/admin/usage.rs` (make `Filter::sql` and `Filter::bind` `pub(crate)`)

**Interfaces:**
- Consumes: `usage::{Range, Filter}`, `Range::since`.
- Produces:
  ```rust
  pub struct ToolRow { pub tool: String, pub calls: i64,
                       pub by_key: Vec<(String, i64)>, pub by_model: Vec<(String, i64)> }
  pub async fn overview(db: &Db, r: Range, f: &Filter) -> Result<Vec<ToolRow>>;
  pub struct CallRow { pub request_id: String, pub call_id: String, pub ts: i64,
                       pub key_name: Option<String>, pub model: Option<String>,
                       pub arguments: String, pub result: Option<String>,
                       pub result_request_id: Option<String>, pub result_status: Option<u16>,
                       pub request_logged: bool, pub result_logged: bool }
  pub const CALLS_PAGE: i64 = 50;
  pub async fn calls(db: &Db, tool: &str, f: &Filter, page: i64) -> Result<Vec<CallRow>>;
  ```
  `overview` reads `tool_usage_hourly` only, sorted by `calls` desc; `by_key`/`by_model` busiest first, key names joined from `api_keys` (missing → `"(deleted)"`, `-1` → `"(none)"`). `calls` reads `tool_calls` newest first, decompresses blobs (lossy UTF-8), and sets `request_logged`/`result_logged` via `EXISTS` on `requests`.

- [ ] **Step 1: Write the failing tests** (seed via `logstore::insert_record` with `toolcalls::extract`)

  - `overview_counts_by_tool_key_model`: 3 calls of `a` (key 1 model m1 ×2, key 2 model m2 ×1), 1 call of `b` → rows `[a:3, b:1]`, `a.by_key` sums to 3, `a.by_model == [("m1",2),("m2",1)]`.
  - `overview_respects_filter`: `Filter{key_id:Some(2)}` → only `a:1`.
  - `calls_newest_first_with_result`: two calls of `a`, one answered → newest first, `result` set only on the answered one, `request_logged == true`.
  - `calls_pagination`: 51 calls → page 0 has 50, page 1 has 1.

- [ ] **Step 2: Run** — `cargo test admin::tools::` → fails.
- [ ] **Step 3: Implement** per Interfaces.
- [ ] **Step 4: Run** — `cargo test` → all pass.
- [ ] **Step 5: Commit**

  ```bash
  git add src/admin/tools.rs src/admin/mod.rs src/admin/usage.rs
  git commit -m "feat(admin): tool-call statistics queries"
  ```

### Task 5: Tools pages, routes and nav

**Files:**
- Modify: `src/admin/tools.rs` (handlers)
- Modify: `src/admin/mod.rs` (routes, tests)
- Modify: `src/admin/pages.rs` (`layout` nav)
- Modify: `README.md` (one bullet under the feature list)

**Interfaces:**
- Consumes: Task 4 queries; `pages::{layout, fmt_ts, err500, key_rows}`; `registry::list_models`; `usage::{range, RANGES}`; `logs::render_payload`.
- Produces: routes `GET /admin/tools` → `tools_page(State, Query<ToolsQuery>)`, `GET /admin/tools/{name}` → `tool_detail(State, Path<String>, Query<ToolsQuery>)`, where `ToolsQuery { range: Option<String>, key: Option<i64>, model: Option<String>, page: Option<i64> }`.

- [ ] **Step 1: Write the failing tests** (in `admin/mod.rs` tests, using `logged_in` / `get_page`)

  - `tools_page_lists_tools`: seed one call of `get_weather` → `/admin/tools` is 200, contains `get_weather` and a link to `/admin/tools/get_weather`; nav contains `href="/admin/tools"`.
  - `tool_page_shows_raw_call`: seed call with arguments `{"city":"Bonn"}` and a matched result `sunny` → detail page contains `Bonn`, `sunny`, and `/admin/logs/<request id>`.
  - `tool_page_no_result`: unanswered call → page contains `no result`.
  - `tool_page_odd_name`: tool `mcp__fs/read file` → overview link is the URL-encoded name (`urlencoding::encode`) and fetching it returns 200 containing the call.
  - `tools_page_empty`: no data → 200 with an empty-state message.

- [ ] **Step 2: Run** — `cargo test admin::` → fails.

- [ ] **Step 3: Implement**

  Overview: filter form like `logs_page` (range select over `RANGES`, key, model), one card with a `table class="grid"`: columns Tool, Calls, By key, By model (the breakdown columns as compact `name count` lists). Detail: same filters plus pagination ("← Newer"/"Older →" like logs); one row per call with time, key, model, 120-char previews; a `<details>` per row with arguments and result via `render_payload` (download URL may be the log's `/admin/logs/{id}/download`), links to both log entries when `*_logged`, plain id text otherwise, a `badge-warning` with the status when `result_status >= 400`, and the text `no result` when unanswered. Nav: insert `("/admin/tools", "Tools", "tools")` between Logs and Settings. README: one bullet describing tool-call insights.

- [ ] **Step 4: Run** — `cargo test` and `cargo clippy --all-targets` → all pass, no new warnings.

- [ ] **Step 5: Commit**

  ```bash
  git add src/admin/tools.rs src/admin/mod.rs src/admin/pages.rs README.md
  git commit -m "feat(admin): Tools page with per-tool raw calls"
  ```
