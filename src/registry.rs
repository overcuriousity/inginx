use crate::db::Db;
use crate::keys::ApiKey;
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Backend {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub upstream_api_key: Option<String>,
    pub enabled: bool,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: i64,
    pub backend_id: i64,
    pub upstream_name: String,
    pub alias: String,
    pub enabled: bool,
    /// Optional JSON object rewriting the `voice` field of an audio request
    /// before it is forwarded — `{"alloy": "af_alloy"}`. Backends name their
    /// voices whatever they like (Kokoro's `af_*`, Piper's language codes),
    /// while every stock OpenAI client sends `alloy`; this is where the two
    /// are reconciled. See `proxy::apply_voice_map`.
    pub voice_map: Option<String>,
    /// Unix seconds. Reported as `created` on `/v1/models`, which some clients
    /// require to be present. Rows written before this column existed carry 0.
    pub created_at: i64,
}

fn backend_from_row(r: &rusqlite::Row) -> rusqlite::Result<Backend> {
    Ok(Backend {
        id: r.get(0)?,
        name: r.get(1)?,
        base_url: r.get(2)?,
        upstream_api_key: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
        is_default: r.get::<_, i64>(5)? != 0,
    })
}

const BACKEND_COLS: &str = "id, name, base_url, upstream_api_key, enabled, is_default";

fn model_from_row(r: &rusqlite::Row) -> rusqlite::Result<Model> {
    Ok(Model {
        id: r.get(0)?,
        backend_id: r.get(1)?,
        upstream_name: r.get(2)?,
        alias: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
        voice_map: r.get(5)?,
        created_at: r.get(6)?,
    })
}

const MODEL_COLS: &str = "id, backend_id, upstream_name, alias, enabled, voice_map, created_at";

pub async fn add_backend(
    db: &Db,
    name: &str,
    base_url: &str,
    upstream_api_key: Option<String>,
    is_default: bool,
) -> Result<i64> {
    let name = name.to_string();
    let base_url = base_url.trim_end_matches('/').to_string();
    // The admin who sets a backend is trusted — the proxy is meant to reach
    // internal GPU hosts, so we cannot block private addresses without
    // defeating the point. But a `file://` or a URL carrying credentials is
    // never a legitimate OpenAI base, and refusing them costs nothing.
    let lower = base_url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        anyhow::bail!("base_url must be an http(s) URL");
    }
    if base_url
        .split_once("//")
        .and_then(|(_, rest)| rest.split('/').next())
        .map(|authority| authority.contains('@'))
        .unwrap_or(false)
    {
        anyhow::bail!("base_url must not embed credentials");
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        if is_default {
            tx.execute("UPDATE backends SET is_default = 0", [])?;
        }
        tx.execute(
            "INSERT INTO backends (name, base_url, upstream_api_key, is_default) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![name, base_url, upstream_api_key, is_default as i64],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    })
    .await
}

pub async fn add_model(db: &Db, backend_id: i64, upstream_name: &str, alias: &str) -> Result<i64> {
    let upstream_name = upstream_name.to_string();
    let alias = alias.to_string();
    db.call(move |c| {
        c.execute(
            "INSERT INTO models (backend_id, upstream_name, alias, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![backend_id, upstream_name, alias, crate::db::now()],
        )?;
        Ok(c.last_insert_rowid())
    })
    .await
}

pub async fn resolve(db: &Db, alias: &str) -> Result<Option<(Model, Backend)>> {
    let alias = alias.to_string();
    db.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT m.id, m.backend_id, m.upstream_name, m.alias, m.enabled,
                    m.voice_map, m.created_at,
                    b.id, b.name, b.base_url, b.upstream_api_key, b.enabled, b.is_default
             FROM models m JOIN backends b ON b.id = m.backend_id
             WHERE m.alias = ?1 AND m.enabled = 1 AND b.enabled = 1",
        )?;
        let row = stmt
            .query_row([alias], |r| {
                Ok((
                    Model {
                        id: r.get(0)?,
                        backend_id: r.get(1)?,
                        upstream_name: r.get(2)?,
                        alias: r.get(3)?,
                        enabled: r.get::<_, i64>(4)? != 0,
                        voice_map: r.get(5)?,
                        created_at: r.get(6)?,
                    },
                    Backend {
                        id: r.get(7)?,
                        name: r.get(8)?,
                        base_url: r.get(9)?,
                        upstream_api_key: r.get(10)?,
                        enabled: r.get::<_, i64>(11)? != 0,
                        is_default: r.get::<_, i64>(12)? != 0,
                    },
                ))
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(row)
    })
    .await
}

pub async fn default_backend(db: &Db) -> Result<Option<Backend>> {
    db.call(move |c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {BACKEND_COLS} FROM backends WHERE enabled = 1 ORDER BY is_default DESC, id ASC LIMIT 1"
        ))?;
        let b = stmt.query_row([], backend_from_row).map(Some).or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
        Ok(b)
    })
    .await
}

pub async fn list_backends(db: &Db) -> Result<Vec<Backend>> {
    db.call(move |c| {
        let mut stmt = c.prepare(&format!("SELECT {BACKEND_COLS} FROM backends ORDER BY id"))?;
        let rows = stmt.query_map([], backend_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

pub async fn list_models(db: &Db) -> Result<Vec<Model>> {
    db.call(move |c| {
        let mut stmt = c.prepare(&format!("SELECT {MODEL_COLS} FROM models ORDER BY alias"))?;
        let rows = stmt.query_map([], model_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

pub async fn models_for_key(db: &Db, key: &ApiKey) -> Result<Vec<Model>> {
    let all = key.all_models;
    let key_id = key.id;
    db.call(move |c| {
        let sql = if all {
            format!(
                "SELECT {MODEL_COLS} FROM models m
                 WHERE enabled = 1 AND EXISTS
                   (SELECT 1 FROM backends b WHERE b.id = m.backend_id AND b.enabled = 1)
                 ORDER BY alias"
            )
        } else {
            "SELECT m.id, m.backend_id, m.upstream_name, m.alias, m.enabled,
                    m.voice_map, m.created_at FROM models m
             JOIN key_models km ON km.model_id = m.id AND km.key_id = ?1
             JOIN backends b ON b.id = m.backend_id AND b.enabled = 1
             WHERE m.enabled = 1 ORDER BY m.alias"
                .to_string()
        };
        let mut stmt = c.prepare(&sql)?;
        let rows = if all {
            stmt.query_map([], model_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map([key_id], model_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    })
    .await
}

pub async fn set_model_enabled(db: &Db, model_id: i64, enabled: bool) -> Result<()> {
    db.call(move |c| {
        c.execute(
            "UPDATE models SET enabled = ?1 WHERE id = ?2",
            rusqlite::params![enabled as i64, model_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_model_alias(db: &Db, model_id: i64, alias: &str) -> Result<()> {
    let alias = alias.to_string();
    db.call(move |c| {
        c.execute(
            "UPDATE models SET alias = ?1 WHERE id = ?2",
            rusqlite::params![alias, model_id],
        )?;
        Ok(())
    })
    .await
}

/// Stores (or clears, with `None`) the voice rewrite map for one model. The
/// caller is responsible for handing over a JSON object; `parse_voice_map`
/// turns the admin form's `alloy=af_alloy, nova=af_nova` into one.
pub async fn set_model_voice_map(db: &Db, model_id: i64, map: Option<String>) -> Result<()> {
    db.call(move |c| {
        c.execute(
            "UPDATE models SET voice_map = ?1 WHERE id = ?2",
            rusqlite::params![map, model_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn make_default(db: &Db, backend_id: i64) -> Result<()> {
    db.call(move |c| {
        let tx = c.transaction()?;
        tx.execute("UPDATE backends SET is_default = 0", [])?;
        tx.execute(
            "UPDATE backends SET is_default = 1 WHERE id = ?1",
            [backend_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Fetches `{base_url}/models`, inserts models not yet present (alias = upstream
/// name), and returns the newly added upstream names.
pub async fn import_models(db: &Db, backend_id: i64) -> Result<Vec<String>> {
    let backend = db
        .call(move |c| {
            Ok(c.query_row(
                &format!("SELECT {BACKEND_COLS} FROM backends WHERE id = ?1"),
                [backend_id],
                backend_from_row,
            )?)
        })
        .await?;

    let mut req = reqwest::Client::new().get(format!("{}/models", backend.base_url));
    if let Some(k) = &backend.upstream_api_key {
        req = req.bearer_auth(k);
    }
    let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
    let ids: Vec<String> = body["data"]
        .as_array()
        .context("upstream /models response has no data array")?
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect();

    db.call(move |c| {
        let tx = c.transaction()?;
        let mut added = Vec::new();
        for id in ids {
            let n = tx.execute(
                "INSERT OR IGNORE INTO models (backend_id, upstream_name, alias, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![backend_id, id, id, crate::db::now()],
            )?;
            if n > 0 {
                added.push(id);
            }
        }
        tx.commit()?;
        Ok(added)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testdb;

    #[tokio::test]
    async fn resolve_alias() {
        let db = testdb().await;
        let b = add_backend(&db, "gpu1", "http://127.0.0.1:9999", None, true)
            .await
            .unwrap();
        add_model(&db, b, "qwen3-8b-q4", "qwen3-8b").await.unwrap();
        let (m, be) = resolve(&db, "qwen3-8b").await.unwrap().unwrap();
        assert_eq!(m.upstream_name, "qwen3-8b-q4");
        assert_eq!(be.name, "gpu1");
        assert!(resolve(&db, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn disabled_model_does_not_resolve() {
        let db = testdb().await;
        let b = add_backend(&db, "gpu1", "http://x", None, true)
            .await
            .unwrap();
        let mid = add_model(&db, b, "m", "m").await.unwrap();
        db.call(move |c| {
            c.execute("UPDATE models SET enabled = 0 WHERE id = ?1", [mid])?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(resolve(&db, "m").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn models_for_key_respects_grants() {
        let db = testdb().await;
        let b = add_backend(&db, "gpu1", "http://x", None, true)
            .await
            .unwrap();
        let a_id = add_model(&db, b, "a", "a").await.unwrap();
        add_model(&db, b, "b", "b").await.unwrap();
        let plain_all = crate::keys::create(&db, "all", true, &[], None)
            .await
            .unwrap();
        let plain_one = crate::keys::create(&db, "one", false, &[a_id], None)
            .await
            .unwrap();
        let k_all = crate::keys::authenticate(&db, &plain_all)
            .await
            .unwrap()
            .unwrap();
        let k_one = crate::keys::authenticate(&db, &plain_one)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(models_for_key(&db, &k_all).await.unwrap().len(), 2);
        let one = models_for_key(&db, &k_one).await.unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].alias, "a");
    }

    #[tokio::test]
    async fn import_models_dedups() {
        let db = testdb().await;
        // mock upstream serving /models
        let app = axum::Router::new().route(
            "/models",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"data":[{"id":"qwen3-8b"},{"id":"bge-m3"}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let b = add_backend(&db, "gpu1", &format!("http://{addr}"), None, true)
            .await
            .unwrap();
        let mut added = import_models(&db, b).await.unwrap();
        added.sort();
        assert_eq!(added, vec!["bge-m3".to_string(), "qwen3-8b".to_string()]);
        assert!(import_models(&db, b).await.unwrap().is_empty());
    }
}
