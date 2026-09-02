use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

fn usage_from_value(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    if !u.is_object() {
        return None;
    }
    let prompt = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(Value::as_u64);
    let completion = u
        .get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(Value::as_u64);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    Some(Usage {
        prompt_tokens: prompt.unwrap_or(0) as u32,
        completion_tokens: completion.unwrap_or(0) as u32,
    })
}

pub fn extract_usage_json(body: &[u8]) -> Option<Usage> {
    let v: Value = serde_json::from_slice(body).ok()?;
    usage_from_value(&v)
}

/// Scans SSE `data:` payloads from the end for a chunk carrying `usage`.
pub fn extract_usage_sse(raw: &[u8]) -> Option<Usage> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.lines().rev() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(payload) {
            if let Some(u) = usage_from_value(&v) {
                return Some(u);
            }
        }
    }
    None
}

/// Rewrites `model` to the upstream name and, for streaming requests, injects
/// `stream_options.include_usage` when the client did not ask for it.
/// Returns the (possibly rewritten) body and whether usage was injected.
pub fn prepare_upstream_body(body: &[u8], upstream_model: Option<&str>) -> (Vec<u8>, bool) {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return (body.to_vec(), false);
    };
    if !v.is_object() {
        return (body.to_vec(), false);
    }
    let mut changed = false;
    if let Some(up) = upstream_model {
        if v.get("model").and_then(Value::as_str) != Some(up) {
            v["model"] = Value::String(up.to_string());
            changed = true;
        }
    }
    let mut injected = false;
    if v.get("stream").and_then(Value::as_bool) == Some(true) {
        let already = v
            .get("stream_options")
            .and_then(|o| o.get("include_usage"))
            .and_then(Value::as_bool)
            == Some(true);
        if !already {
            v["stream_options"]["include_usage"] = Value::Bool(true);
            injected = true;
            changed = true;
        }
    }
    if changed {
        (serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()), injected)
    } else {
        (body.to_vec(), injected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_from_chat_json() {
        let b = br#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34}}"#;
        let u = extract_usage_json(b).unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens), (12, 34));
    }

    #[test]
    fn usage_from_embeddings_json() {
        let b = br#"{"data":[],"usage":{"prompt_tokens":7,"total_tokens":7}}"#;
        assert_eq!(extract_usage_json(b).unwrap().prompt_tokens, 7);
    }

    #[test]
    fn usage_absent() {
        assert!(extract_usage_json(br#"{"choices":[]}"#).is_none());
        assert!(extract_usage_json(b"not json").is_none());
    }

    #[test]
    fn usage_from_sse_final_chunk() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
        let u = extract_usage_sse(raw).unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens), (5, 9));
    }

    #[test]
    fn sse_without_usage() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(extract_usage_sse(raw).is_none());
    }

    #[test]
    fn inject_include_usage_and_rewrite_model() {
        let b = br#"{"model":"qwen3-8b","stream":true,"messages":[]}"#;
        let (out, injected) = prepare_upstream_body(b, Some("qwen3-8b-q4"));
        assert!(injected);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "qwen3-8b-q4");
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn no_injection_when_client_asked() {
        let b = br#"{"model":"m","stream":true,"stream_options":{"include_usage":true}}"#;
        let (_, injected) = prepare_upstream_body(b, None);
        assert!(!injected);
    }

    #[test]
    fn non_stream_untouched() {
        let b = br#"{"model":"m","messages":[]}"#;
        let (out, injected) = prepare_upstream_body(b, None);
        assert_eq!(out, b.to_vec());
        assert!(!injected);
    }

    #[test]
    fn non_json_untouched() {
        let b = b"\x00\x01binary";
        let (out, injected) = prepare_upstream_body(b, Some("x"));
        assert_eq!(out, b.to_vec());
        assert!(!injected);
    }
}
