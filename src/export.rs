use crate::db::Db;
use anyhow::Result;
use base64::Engine;
use serde_json::{json, Value};

#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub key_id: Option<i64>,
    pub model: Option<String>,
}

struct Row {
    id: String,
    ts: i64,
    key_id: Option<i64>,
    model: Option<String>,
    endpoint: String,
    status: u16,
    latency_ms: u32,
    ttfb_ms: Option<u32>,
    prompt_tokens: u32,
    completion_tokens: u32,
    usage_source: String,
    error: Option<String>,
    req_headers: String,
    resp_headers: String,
    req_body: Vec<u8>,
    resp_body: Vec<u8>,
}

async fn fetch(db: &Db, filter: &Filter) -> Result<Vec<Row>> {
    let f = filter.clone();
    db.call(move |c| {
        let mut sql = String::from(
            "SELECT id, ts, key_id, model, endpoint, status, latency_ms, ttfb_ms,
                    prompt_tokens, completion_tokens, usage_source, error,
                    req_headers, resp_headers, req_body, resp_body
             FROM requests WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(from) = f.from {
            sql.push_str(" AND ts >= ?");
            params.push(Box::new(from));
        }
        if let Some(to) = f.to {
            sql.push_str(" AND ts <= ?");
            params.push(Box::new(to));
        }
        if let Some(kid) = f.key_id {
            sql.push_str(" AND key_id = ?");
            params.push(Box::new(kid));
        }
        if let Some(model) = f.model.clone() {
            sql.push_str(" AND model = ?");
            params.push(Box::new(model));
        }
        sql.push_str(" ORDER BY ts ASC");
        let mut stmt = c.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(Row {
                id: r.get(0)?,
                ts: r.get(1)?,
                key_id: r.get(2)?,
                model: r.get(3)?,
                endpoint: r.get(4)?,
                status: r.get(5)?,
                latency_ms: r.get(6)?,
                ttfb_ms: r.get(7)?,
                prompt_tokens: r.get(8)?,
                completion_tokens: r.get(9)?,
                usage_source: r.get(10)?,
                error: r.get(11)?,
                req_headers: r.get::<_, Option<String>>(12)?.unwrap_or_default(),
                resp_headers: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                req_body: r.get::<_, Option<Vec<u8>>>(14)?.unwrap_or_default(),
                resp_body: r.get::<_, Option<Vec<u8>>>(15)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

fn decompress(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(data).unwrap_or_else(|_| data.to_vec())
}

fn body_value(body: &[u8]) -> Value {
    match std::str::from_utf8(body) {
        Ok(s) => json!(s),
        Err(_) => json!({
            "encoding": "base64",
            "data": base64::engine::general_purpose::STANDARD.encode(body),
        }),
    }
}

/// One JSON object per line with full decompressed payloads — the lossless format.
pub async fn native_jsonl(db: &Db, filter: &Filter) -> Result<String> {
    let rows = fetch(db, filter).await?;
    let mut out = String::new();
    for r in rows {
        let line = json!({
            "id": r.id,
            "ts": r.ts,
            "key_id": r.key_id,
            "model": r.model,
            "endpoint": r.endpoint,
            "status": r.status,
            "latency_ms": r.latency_ms,
            "ttfb_ms": r.ttfb_ms,
            "prompt_tokens": r.prompt_tokens,
            "completion_tokens": r.completion_tokens,
            "usage_source": r.usage_source,
            "error": r.error,
            "req_headers": serde_json::from_str::<Value>(&r.req_headers).unwrap_or(Value::Null),
            "resp_headers": serde_json::from_str::<Value>(&r.resp_headers).unwrap_or(Value::Null),
            "req_body": body_value(&decompress(&r.req_body)),
            "resp_body": body_value(&decompress(&r.resp_body)),
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    Ok(out)
}

/// Trace/span ids derived deterministically from the request ULID.
fn trace_span_ids(ulid: &str) -> (String, String) {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(ulid.as_bytes());
    (hex::encode(&digest[..16]), hex::encode(&digest[16..24]))
}

fn otel_attr(key: &str, value: Value) -> Value {
    let v = match value {
        Value::String(s) => json!({"stringValue": s}),
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            json!({"intValue": n.to_string()})
        }
        other => json!({"stringValue": other.to_string()}),
    };
    json!({"key": key, "value": v})
}

/// One OTLP/JSON `ResourceSpans`-wrapped span per line, following the OTel
/// GenAI semantic conventions, with prompt/completion payloads as span events.
pub async fn otel_jsonl(db: &Db, filter: &Filter) -> Result<String> {
    let rows = fetch(db, filter).await?;
    let mut out = String::new();
    for r in rows {
        let (trace_id, span_id) = trace_span_ids(&r.id);
        let start_ns = r.ts as i128 * 1_000_000_000;
        let end_ns = start_ns + r.latency_ms as i128 * 1_000_000;
        let verb = r
            .endpoint
            .rsplit('/')
            .next()
            .unwrap_or("request")
            .to_string();
        let name = match &r.model {
            Some(m) => format!("{verb} {m}"),
            None => verb.clone(),
        };
        let mut attributes = vec![
            otel_attr("gen_ai.system", json!("inginx")),
            otel_attr("gen_ai.operation.name", json!(verb)),
            otel_attr("gen_ai.usage.input_tokens", json!(r.prompt_tokens)),
            otel_attr("gen_ai.usage.output_tokens", json!(r.completion_tokens)),
            otel_attr("http.response.status_code", json!(r.status)),
            otel_attr("url.path", json!(r.endpoint)),
        ];
        if let Some(m) = &r.model {
            attributes.push(otel_attr("gen_ai.request.model", json!(m)));
        }
        if let Some(kid) = r.key_id {
            attributes.push(otel_attr("inginx.api_key_id", json!(kid)));
        }
        attributes.push(otel_attr("inginx.usage_source", json!(r.usage_source)));
        let events = vec![
            json!({
                "timeUnixNano": start_ns.to_string(),
                "name": "gen_ai.content.prompt",
                "attributes": [otel_attr("gen_ai.prompt", body_value(&decompress(&r.req_body)))],
            }),
            json!({
                "timeUnixNano": end_ns.to_string(),
                "name": "gen_ai.content.completion",
                "attributes": [otel_attr("gen_ai.completion", body_value(&decompress(&r.resp_body)))],
            }),
        ];
        let status = if r.error.is_some() || r.status >= 400 {
            json!({"code": "STATUS_CODE_ERROR", "message": r.error.clone().unwrap_or_default()})
        } else {
            json!({"code": "STATUS_CODE_OK"})
        };
        let line = json!({
            "resourceSpans": [{
                "resource": {"attributes": [otel_attr("service.name", json!("inginx"))]},
                "scopeSpans": [{
                    "scope": {"name": "inginx"},
                    "spans": [{
                        "traceId": trace_id,
                        "spanId": span_id,
                        "name": name,
                        "kind": 2,
                        "startTimeUnixNano": start_ns.to_string(),
                        "endTimeUnixNano": end_ns.to_string(),
                        "attributes": attributes,
                        "events": events,
                        "status": status,
                    }],
                }],
            }],
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testdb;
    use crate::logstore::LogRecord;
    use crate::metering::Usage;

    async fn seed(db: &Db, ts: i64, model: &str) {
        let rec = LogRecord {
            id: ulid::Ulid::new().to_string(),
            ts,
            key_id: Some(1),
            model: Some(model.into()),
            endpoint: "/v1/chat/completions".into(),
            status: 200,
            latency_ms: 42,
            ttfb_ms: Some(7),
            usage: Usage { prompt_tokens: 5, completion_tokens: 9 },
            usage_source: "upstream",
            error: None,
            req_headers: "{}".into(),
            resp_headers: "{}".into(),
            req_body: br#"{"messages":[{"content":"hello"}]}"#.to_vec(),
            resp_body: br#"{"choices":[]}"#.to_vec(),
        };
        let w = crate::logstore::spawn_writer(db.clone());
        w.send(rec);
        for _ in 0..100 {
            let n: i64 = db
                .call(|c| Ok(c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?))
                .await
                .unwrap();
            if n > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn native_jsonl_filters_and_roundtrips() {
        let db = testdb().await;
        seed(&db, 1000, "in-filter").await;
        seed(&db, 999_999, "out-of-filter").await;
        let out = native_jsonl(
            &db,
            &Filter { from: Some(0), to: Some(2000), ..Default::default() },
        )
        .await
        .unwrap();
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["model"], "in-filter");
        assert_eq!(v["req_body"], r#"{"messages":[{"content":"hello"}]}"#);
        assert_eq!(v["prompt_tokens"], 5);
    }

    #[tokio::test]
    async fn otel_jsonl_span_shape() {
        let db = testdb().await;
        seed(&db, 1000, "m").await;
        let out = otel_jsonl(&db, &Filter::default()).await.unwrap();
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        let span = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        let trace_id = span["traceId"].as_str().unwrap();
        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        let attrs = span["attributes"].as_array().unwrap();
        let input = attrs
            .iter()
            .find(|a| a["key"] == "gen_ai.usage.input_tokens")
            .unwrap();
        assert_eq!(input["value"]["intValue"], "5");
        assert_eq!(span["events"].as_array().unwrap().len(), 2);
        assert_eq!(span["startTimeUnixNano"], "1000000000000");
    }
}
