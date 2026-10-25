//! Tool calls and their results, read out of chat-completions traffic.

use crate::logstore::{compress, LogRecord};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Call {
    pub call_id: String,
    pub tool: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolResult {
    pub call_id: String,
    pub tool: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub calls: Vec<Call>,
    pub results: Vec<ToolResult>,
}

/// Every tool call the model made in one response, buffered JSON or SSE.
///
/// A stream sends each call in pieces: `id` and `function.name` in an early
/// fragment, `arguments` spread over many. Fragments are keyed by
/// `(choice index, tool_call index)` and their arguments concatenated in
/// arrival order. Arguments stay the exact string the model produced, valid
/// JSON or not.
///
/// A missing or repeated id would make the call unmatchable at best and
/// mismatched at worst, so it is replaced by `inginx-<request_id>-<n>`,
/// which no client will ever send back.
pub fn calls_from_response(resp_body: &[u8], request_id: &str) -> Vec<Call> {
    let mut parts: BTreeMap<(u64, u64), Call> = BTreeMap::new();
    let mut absorb = |choice: u64, tc_index: u64, tc: &Value| {
        let entry = parts.entry((choice, tc_index)).or_default();
        if entry.call_id.is_empty() {
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                entry.call_id = id.to_string();
            }
        }
        let function = tc.get("function");
        if entry.tool.is_empty() {
            if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) {
                entry.tool = name.to_string();
            }
        }
        if let Some(args) = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
        {
            entry.arguments.push_str(args);
        }
    };
    let mut visit = |v: &Value, field: &str| {
        for (ci, choice) in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let choice_index = choice
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(ci as u64);
            let calls = choice
                .get(field)
                .and_then(|m| m.get("tool_calls"))
                .and_then(Value::as_array);
            for (ti, tc) in calls.into_iter().flatten().enumerate() {
                let tc_index = tc.get("index").and_then(Value::as_u64).unwrap_or(ti as u64);
                absorb(choice_index, tc_index, tc);
            }
        }
    };
    let text = std::str::from_utf8(resp_body).unwrap_or("");
    if text.trim_start().starts_with("data:") {
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(v) = serde_json::from_str::<Value>(payload.trim()) {
                visit(&v, "delta");
            }
        }
    } else if let Ok(v) = serde_json::from_slice::<Value>(resp_body) {
        visit(&v, "message");
    }

    let mut seen = HashSet::new();
    parts
        .into_values()
        .filter(|c| !c.tool.is_empty())
        .enumerate()
        .map(|(n, mut c)| {
            if c.call_id.is_empty() || !seen.insert(c.call_id.clone()) {
                c.call_id = format!("inginx-{request_id}-{n}");
            }
            c
        })
        .collect()
}

/// Every tool result the client sent back in one request.
///
/// A result counts only when the same request also carries the assistant
/// message that issued the call: that sibling names the tool, and its
/// presence is what ties the result to a call this conversation made rather
/// than to any call that happens to share an id.
pub fn results_from_request(req_body: &[u8]) -> Vec<ToolResult> {
    let Ok(v) = serde_json::from_slice::<Value>(req_body) else {
        return Vec::new();
    };
    let Some(messages) = v.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut issued: HashMap<&str, &str> = HashMap::new();
    for m in messages {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for tc in m
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = tc.get("id").and_then(Value::as_str);
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str);
            if let (Some(id), Some(name)) = (id, name) {
                issued.insert(id, name);
            }
        }
    }
    messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| {
            let id = m.get("tool_call_id").and_then(Value::as_str)?;
            let tool = issued.get(id)?;
            let content = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => String::new(),
                Some(other) => other.to_string(),
            };
            Some(ToolResult {
                call_id: id.to_string(),
                tool: tool.to_string(),
                content,
            })
        })
        .collect()
}

/// Writes one request's calls and fills in the results it carries.
///
/// Each result goes to the newest still-open call with the same key, id and
/// tool that an *earlier* request emitted — a request cannot answer its own
/// response. Results are taken last message first: when a backend reuses ids
/// (`call_0` every turn), the latest tool message belongs to the latest call,
/// and the resent older ones then find their calls already answered.
pub fn store(c: &rusqlite::Connection, rec: &LogRecord, x: &Extracted) -> anyhow::Result<()> {
    let hour = rec.ts - rec.ts % 3600;
    for call in &x.calls {
        c.execute(
            "INSERT OR IGNORE INTO tool_calls (request_id, call_id, ts, key_id, model, tool, arguments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                rec.id,
                call.call_id,
                rec.ts,
                rec.key_id,
                rec.model,
                call.tool,
                compress(call.arguments.as_bytes()),
            ],
        )?;
        c.execute(
            "INSERT INTO tool_usage_hourly (key_id, model, tool, hour, n_calls)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(key_id, model, tool, hour) DO UPDATE SET n_calls = n_calls + 1",
            rusqlite::params![
                rec.key_id.unwrap_or(-1),
                rec.model.as_deref().unwrap_or(""),
                call.tool,
                hour,
            ],
        )?;
    }
    for result in x.results.iter().rev() {
        c.execute(
            "UPDATE tool_calls SET result = ?1, result_ts = ?2, result_request_id = ?3, result_status = ?4
             WHERE rowid = (
               SELECT rowid FROM tool_calls
               WHERE key_id IS ?5 AND call_id = ?6 AND tool = ?7 AND result IS NULL
                 AND ts <= ?2 AND request_id != ?3
               ORDER BY ts DESC, rowid DESC LIMIT 1)",
            rusqlite::params![
                compress(result.content.as_bytes()),
                rec.ts,
                rec.id,
                rec.status,
                rec.key_id,
                result.call_id,
                result.tool,
            ],
        )?;
    }
    Ok(())
}

/// What one logged request contributes. Only chat completions carry tool
/// calls. Calls are taken from successful responses only, but results are
/// read from any request: a tool output that made the next turn fail is the
/// most interesting result of all.
pub fn extract(rec: &LogRecord) -> Extracted {
    if !rec.endpoint.ends_with("/chat/completions") {
        return Extracted::default();
    }
    let calls = if (200..300).contains(&rec.status) {
        calls_from_response(&rec.resp_body, &rec.id)
    } else {
        Vec::new()
    };
    Extracted {
        calls,
        results: results_from_request(&rec.req_body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::Usage;
    use serde_json::json;

    fn call(id: &str, tool: &str, args: &str) -> Call {
        Call {
            call_id: id.into(),
            tool: tool.into(),
            arguments: args.into(),
        }
    }

    fn sse(events: &[serde_json::Value]) -> Vec<u8> {
        let mut out = String::new();
        for e in events {
            out.push_str(&format!("data: {e}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        out.into_bytes()
    }

    fn delta(index: u64, tc: serde_json::Value) -> serde_json::Value {
        let mut tc = tc;
        tc["index"] = json!(index);
        json!({"choices":[{"index":0,"delta":{"tool_calls":[tc]}}]})
    }

    #[test]
    fn buffered_single_call() {
        let body = br#"{"choices":[{"message":{"tool_calls":[{"id":"c1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Bonn\"}"}}]}}]}"#;
        assert_eq!(
            calls_from_response(body, "R"),
            vec![call("c1", "get_weather", r#"{"city":"Bonn"}"#)]
        );
    }

    #[test]
    fn buffered_parallel_calls() {
        let body = json!({"choices":[{"message":{"tool_calls":[
            {"id":"a","function":{"name":"read","arguments":"{}"}},
            {"id":"b","function":{"name":"write","arguments":"{\"x\":1}"}}
        ]}}]})
        .to_string();
        assert_eq!(
            calls_from_response(body.as_bytes(), "R"),
            vec![call("a", "read", "{}"), call("b", "write", r#"{"x":1}"#)]
        );
    }

    #[test]
    fn stream_parallel_interleaved() {
        let body = sse(&[
            delta(
                0,
                json!({"id":"a","function":{"name":"read","arguments":""}}),
            ),
            delta(
                1,
                json!({"id":"b","function":{"name":"write","arguments":"{\"p"}}),
            ),
            delta(0, json!({"function":{"arguments":"{\"f\":1"}})),
            delta(1, json!({"function":{"arguments":"\":2}"}})),
            delta(0, json!({"function":{"arguments":"}"}})),
        ]);
        assert_eq!(
            calls_from_response(&body, "R"),
            vec![
                call("a", "read", r#"{"f":1}"#),
                call("b", "write", r#"{"p":2}"#)
            ]
        );
    }

    #[test]
    fn invalid_json_arguments_kept_verbatim() {
        let body = json!({"choices":[{"message":{"tool_calls":[
            {"id":"a","function":{"name":"t","arguments":"{oops"}}
        ]}}]})
        .to_string();
        assert_eq!(
            calls_from_response(body.as_bytes(), "R")[0].arguments,
            "{oops"
        );
    }

    #[test]
    fn missing_and_duplicate_ids_synthesized() {
        let body = json!({"choices":[{"message":{"tool_calls":[
            {"id":"","function":{"name":"t","arguments":"{}"}},
            {"id":"x","function":{"name":"t","arguments":"{}"}},
            {"id":"x","function":{"name":"t","arguments":"{}"}}
        ]}}]})
        .to_string();
        let ids: Vec<String> = calls_from_response(body.as_bytes(), "R")
            .into_iter()
            .map(|c| c.call_id)
            .collect();
        assert_eq!(ids, vec!["inginx-R-0", "x", "inginx-R-2"]);
    }

    #[test]
    fn no_tool_calls() {
        let plain = br#"{"choices":[{"message":{"content":"hello"}}]}"#;
        assert!(calls_from_response(plain, "R").is_empty());
        assert!(calls_from_response(b"", "R").is_empty());
        assert!(calls_from_response(b"not json", "R").is_empty());
        let text_stream = sse(&[json!({"choices":[{"delta":{"content":"hi"}}]})]);
        assert!(calls_from_response(&text_stream, "R").is_empty());
    }

    #[test]
    fn results_need_assistant_sibling() {
        let body = json!({"messages":[
            {"role":"user","content":"go"},
            {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"t","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"ok"},
            {"role":"tool","tool_call_id":"zz","content":"orphan"}
        ]})
        .to_string();
        assert_eq!(
            results_from_request(body.as_bytes()),
            vec![ToolResult {
                call_id: "c1".into(),
                tool: "t".into(),
                content: "ok".into()
            }]
        );
    }

    #[test]
    fn result_content_shapes() {
        let body = json!({"messages":[
            {"role":"assistant","tool_calls":[
                {"id":"a","function":{"name":"t","arguments":"{}"}},
                {"id":"b","function":{"name":"t","arguments":"{}"}}
            ]},
            {"role":"tool","tool_call_id":"a","content":[{"type":"text","text":"hi"}]},
            {"role":"tool","tool_call_id":"b","content":null}
        ]})
        .to_string();
        let r = results_from_request(body.as_bytes());
        assert_eq!(r[0].content, r#"[{"type":"text","text":"hi"}]"#);
        assert_eq!(r[1].content, "");
        assert!(results_from_request(b"not json").is_empty());
        assert!(results_from_request(br#"{"messages":5}"#).is_empty());
    }

    fn rec(endpoint: &str, status: u16) -> LogRecord {
        LogRecord {
            id: "R".into(),
            ts: 0,
            key_id: Some(1),
            model: Some("m".into()),
            endpoint: endpoint.into(),
            status,
            latency_ms: 0,
            ttfb_ms: None,
            usage: Usage::default(),
            usage_source: "upstream",
            error: None,
            req_headers: "{}".into(),
            resp_headers: "{}".into(),
            req_body: json!({"messages":[
                {"role":"assistant","tool_calls":[{"id":"old","function":{"name":"t","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"old","content":"r"}
            ]})
            .to_string()
            .into_bytes(),
            resp_body: json!({"choices":[{"message":{"tool_calls":[
                {"id":"new","function":{"name":"t","arguments":"{}"}}
            ]}}]})
            .to_string()
            .into_bytes(),
        }
    }

    #[test]
    fn extract_rules() {
        let ok = extract(&rec("/v1/chat/completions", 200));
        assert_eq!((ok.calls.len(), ok.results.len()), (1, 1));
        let failed = extract(&rec("/v1/chat/completions", 400));
        assert_eq!((failed.calls.len(), failed.results.len()), (0, 1));
        let other = extract(&rec("/v1/embeddings", 200));
        assert_eq!((other.calls.len(), other.results.len()), (0, 0));
    }
}
