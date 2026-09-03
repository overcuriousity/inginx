use crate::db::{now, Db};
use anyhow::Result;
use rand::Rng;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub all_models: bool,
}

const BASE62: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Keys are minted as `sk_…`. The scheme carries no meaning to inginx — a key
/// is authenticated by the hash of the whole string — but enough clients
/// hard-code a check for OpenAI's prefix that anything else is a support
/// burden. Keys issued before this (`igx_…`) keep working: nothing here
/// parses the prefix.
pub const KEY_PREFIX: &str = "sk_";

/// How many characters of the random body are stored in the clear, for
/// telling keys apart in the GUI.
const PREFIX_CHARS: usize = 8;

fn random_key() -> String {
    let mut rng = rand::rng();
    let body: String = (0..40)
        .map(|_| BASE62[rng.random_range(0..BASE62.len())] as char)
        .collect();
    format!("{KEY_PREFIX}{body}")
}

/// How a stored prefix is shown. New rows hold the whole issued prefix
/// (`sk_AbCd1234`); rows written before keys carried their scheme hold only
/// the eight body characters, and those were all `igx_`.
pub fn display_prefix(stored: &str) -> String {
    if stored.contains('_') {
        format!("{stored}…")
    } else {
        format!("igx_{stored}…")
    }
}

pub fn hash_key(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

pub async fn create(
    db: &Db,
    name: &str,
    all_models: bool,
    model_ids: &[i64],
    expires_at: Option<i64>,
) -> Result<String> {
    let plain = random_key();
    let hash = hash_key(&plain);
    // Stored with its scheme, so the GUI can show a key exactly as it was
    // issued — including the older `igx_` keys, whose rows hold the bare
    // eight characters and are rendered by `display_prefix`.
    let prefix: String = plain[..KEY_PREFIX.len() + PREFIX_CHARS].to_string();
    let name = name.to_string();
    let model_ids = model_ids.to_vec();
    db.call(move |c| {
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO api_keys (name, key_hash, key_prefix, all_models, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![name, hash, prefix, all_models as i64, now(), expires_at],
        )?;
        let key_id = tx.last_insert_rowid();
        for mid in &model_ids {
            tx.execute(
                "INSERT OR IGNORE INTO key_models (key_id, model_id) VALUES (?1, ?2)",
                rusqlite::params![key_id, mid],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(plain)
}

pub async fn authenticate(db: &Db, bearer: &str) -> Result<Option<ApiKey>> {
    let hash = hash_key(bearer);
    db.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT id, name, all_models FROM api_keys
             WHERE key_hash = ?1 AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > ?2)",
        )?;
        let key = stmt
            .query_row(rusqlite::params![hash, now()], |r| {
                Ok(ApiKey {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    all_models: r.get::<_, i64>(2)? != 0,
                })
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(key)
    })
    .await
}

pub async fn revoke(db: &Db, id: i64) -> Result<()> {
    db.call(move |c| {
        c.execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            rusqlite::params![now(), id],
        )?;
        Ok(())
    })
    .await
}

/// Revokes the old key and creates a replacement with the same name and grants.
pub async fn rotate(db: &Db, id: i64) -> Result<String> {
    let plain = random_key();
    let hash = hash_key(&plain);
    // Stored with its scheme, so the GUI can show a key exactly as it was
    // issued — including the older `igx_` keys, whose rows hold the bare
    // eight characters and are rendered by `display_prefix`.
    let prefix: String = plain[..KEY_PREFIX.len() + PREFIX_CHARS].to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        let (name, all_models, expires_at): (String, i64, Option<i64>) = tx.query_row(
            "SELECT name, all_models, expires_at FROM api_keys WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        tx.execute(
            "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2",
            rusqlite::params![now(), id],
        )?;
        tx.execute(
            "INSERT INTO api_keys (name, key_hash, key_prefix, all_models, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![name, hash, prefix, all_models, now(), expires_at],
        )?;
        let new_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO key_models (key_id, model_id)
             SELECT ?1, model_id FROM key_models WHERE key_id = ?2",
            rusqlite::params![new_id, id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(plain)
}

pub async fn key_allows_model(db: &Db, key: &ApiKey, model_id: i64) -> Result<bool> {
    if key.all_models {
        return Ok(true);
    }
    let key_id = key.id;
    db.call(move |c| {
        let n: i64 = c.query_row(
            "SELECT count(*) FROM key_models WHERE key_id = ?1 AND model_id = ?2",
            rusqlite::params![key_id, model_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    })
    .await
}

#[cfg(test)]
pub async fn testdb() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir); // keep tempdir alive for test duration
    db
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_authenticate() {
        let db = testdb().await;
        let plain = create(&db, "ci", true, &[], None).await.unwrap();
        assert!(plain.starts_with("sk_"), "{plain}");
        assert_eq!(plain.len(), KEY_PREFIX.len() + 40);
        // The stored prefix keeps its scheme, so the GUI shows the key the
        // way it was issued.
        let stored: String = db
            .call(|c| Ok(c.query_row("SELECT key_prefix FROM api_keys", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert!(plain.starts_with(&stored), "{stored} vs {plain}");
        assert_eq!(display_prefix(&stored), format!("{stored}…"));
        // A key from before the scheme was stored still renders correctly.
        assert_eq!(display_prefix("AbCd1234"), "igx_AbCd1234…");
        let k = authenticate(&db, &plain).await.unwrap().unwrap();
        assert_eq!(k.name, "ci");
        assert!(k.all_models);
        assert!(authenticate(&db, "sk_wrong").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoked_and_expired_fail() {
        let db = testdb().await;
        let plain = create(&db, "a", true, &[], None).await.unwrap();
        let k = authenticate(&db, &plain).await.unwrap().unwrap();
        revoke(&db, k.id).await.unwrap();
        assert!(authenticate(&db, &plain).await.unwrap().is_none());
        let expired = create(&db, "b", true, &[], Some(now() - 10)).await.unwrap();
        assert!(authenticate(&db, &expired).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rotate_preserves_grants() {
        let db = testdb().await;
        let old = create(&db, "r", false, &[], None).await.unwrap();
        let k = authenticate(&db, &old).await.unwrap().unwrap();
        let newp = rotate(&db, k.id).await.unwrap();
        assert!(authenticate(&db, &old).await.unwrap().is_none());
        let nk = authenticate(&db, &newp).await.unwrap().unwrap();
        assert_eq!(nk.name, "r");
        assert!(!nk.all_models);
    }
}
