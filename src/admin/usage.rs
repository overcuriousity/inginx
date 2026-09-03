//! Usage statistics over the hourly rollup.
//!
//! Every request adds a row to `usage_hourly` keyed by (key, model, hour), so
//! every question the dashboard and the key pages ask — tokens per hour, per
//! day, per week, split by key or by model — is a `GROUP BY` over one small
//! table. The rollup is written by `logstore::insert_record`; nothing here
//! touches the request bodies, which is why a year of statistics survives a
//! retention policy that keeps only a week of payloads.

use crate::db::{now, Db};
use anyhow::Result;

/// A window of time and the bucket the series is drawn at. Hours are the
/// storage granularity, so every bucket is a whole number of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// The `range=` value in a URL.
    pub key: &'static str,
    /// What the selector calls it.
    pub label: &'static str,
    /// How far back the window reaches, in seconds. `None` is all of history.
    pub span_secs: Option<i64>,
    /// Bucket width in seconds: 3600 hourly, 86 400 daily, 604 800 weekly.
    pub bucket_secs: i64,
}

/// The selector, in the order it is rendered. A year is bucketed by week
/// rather than by day because 365 points of daily data is a smear at chart
/// width, and by month would put an unequal number of days in each column.
pub const RANGES: [Range; 6] = [
    Range {
        key: "24h",
        label: "24 hours",
        span_secs: Some(86_400),
        bucket_secs: 3600,
    },
    Range {
        key: "7d",
        label: "7 days",
        span_secs: Some(7 * 86_400),
        bucket_secs: 3600,
    },
    Range {
        key: "30d",
        label: "30 days",
        span_secs: Some(30 * 86_400),
        bucket_secs: 86_400,
    },
    Range {
        key: "90d",
        label: "90 days",
        span_secs: Some(90 * 86_400),
        bucket_secs: 86_400,
    },
    Range {
        key: "1y",
        label: "1 year",
        span_secs: Some(365 * 86_400),
        bucket_secs: 7 * 86_400,
    },
    Range {
        key: "all",
        label: "All time",
        span_secs: None,
        bucket_secs: 7 * 86_400,
    },
];

/// The default window: long enough to show a working week, short enough to
/// still be hourly.
pub const DEFAULT_RANGE: &str = "7d";

pub fn range(key: Option<&str>) -> Range {
    let key = key.unwrap_or(DEFAULT_RANGE);
    RANGES
        .iter()
        .find(|r| r.key == key)
        .copied()
        .unwrap_or(RANGES[1])
}

impl Range {
    /// The first bucket boundary in the window. `all` starts at the oldest
    /// hour on record, so an empty database yields an empty series rather
    /// than a sweep from 1970.
    pub async fn since(&self, db: &Db) -> Result<i64> {
        let start = match self.span_secs {
            Some(span) => now() - span,
            None => {
                let oldest: Option<i64> = db
                    .call(|c| {
                        Ok(c.query_row("SELECT MIN(hour) FROM usage_hourly", [], |r| r.get(0))?)
                    })
                    .await?;
                match oldest {
                    Some(h) => h,
                    None => now(),
                }
            }
        };
        Ok(start - start.rem_euclid(self.bucket_secs))
    }
}

/// What a series or a table is narrowed to. Both are optional and combine:
/// one key, one model, or one key's use of one model.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub key_id: Option<i64>,
    pub model: Option<String>,
}

impl Filter {
    /// The `AND …` tail of a WHERE clause. `prefix` qualifies the columns for
    /// statements that join (`"u."`), and is empty for those that do not —
    /// only the column names take it, never the parameter names.
    fn sql(&self, prefix: &str) -> String {
        let mut s = String::new();
        if self.key_id.is_some() {
            s.push_str(&format!(" AND {prefix}key_id = :key_id"));
        }
        if self.model.is_some() {
            s.push_str(&format!(" AND {prefix}model = :model"));
        }
        s
    }

    /// The named parameters matching `sql()`. Owned and `Send`, because they
    /// are carried into the blocking database closure.
    fn bind(&self, since: i64) -> Vec<(&'static str, Box<dyn rusqlite::ToSql + Send>)> {
        let mut v: Vec<(&'static str, Box<dyn rusqlite::ToSql + Send>)> =
            vec![(":since", Box::new(since))];
        if let Some(k) = self.key_id {
            v.push((":key_id", Box::new(k)));
        }
        if let Some(m) = &self.model {
            v.push((":model", Box::new(m.clone())));
        }
        v
    }
}

/// A time series, one array per measure — the shape uPlot wants, and the
/// shape `/admin/api/stats` returns.
#[derive(Debug, Default, serde::Serialize)]
pub struct Series {
    /// Bucket start, unix seconds.
    pub buckets: Vec<i64>,
    pub requests: Vec<i64>,
    pub prompt_tokens: Vec<i64>,
    pub completion_tokens: Vec<i64>,
    pub errors: Vec<i64>,
    /// Echoed back so a client knows what it is drawing.
    pub bucket_secs: i64,
}

/// Buckets with no traffic are emitted as zeroes rather than skipped: a gap in
/// a line chart reads as missing data, and "nobody called" is not missing.
pub async fn series(db: &Db, r: Range, filter: &Filter) -> Result<Series> {
    let since = r.since(db).await?;
    let bucket = r.bucket_secs;
    let where_extra = filter.sql("");
    let binds = filter.bind(since);
    let sql = format!(
        "SELECT hour - (hour % :bucket) AS b, SUM(n_requests), SUM(prompt_tokens),
                SUM(completion_tokens), SUM(n_errors)
         FROM usage_hourly WHERE hour >= :since{where_extra}
         GROUP BY b ORDER BY b"
    );
    let rows: Vec<(i64, i64, i64, i64, i64)> = db
        .call(move |c| {
            let mut stmt = c.prepare(&sql)?;
            let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = vec![(":bucket", &bucket)];
            for (name, value) in binds.iter() {
                params.push((name, value.as_ref()));
            }
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;

    let map: std::collections::HashMap<i64, (i64, i64, i64, i64)> = rows
        .into_iter()
        .map(|(b, r, p, c, e)| (b, (r, p, c, e)))
        .collect();
    let mut s = Series {
        bucket_secs: bucket,
        ..Default::default()
    };
    let end = now();
    let mut b = since;
    while b <= end {
        let (r, p, c, e) = map.get(&b).copied().unwrap_or_default();
        s.buckets.push(b);
        s.requests.push(r);
        s.prompt_tokens.push(p);
        s.completion_tokens.push(c);
        s.errors.push(e);
        b += bucket;
    }
    Ok(s)
}

/// One row of a leaderboard: a model, or a key, with its totals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Row {
    /// Model alias or key name.
    pub name: String,
    /// Key id, when the row is a key — the link target.
    pub id: Option<i64>,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub errors: i64,
}

impl Row {
    pub fn total_tokens(&self) -> i64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// What a breakdown groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Model,
    Key,
}

/// Totals per model or per key over the window, busiest first. `limit` of 0
/// means every row.
pub async fn breakdown(db: &Db, r: Range, filter: &Filter, by: By, limit: i64) -> Result<Vec<Row>> {
    let since = r.since(db).await?;
    let binds = filter.bind(since);
    let select = match by {
        By::Model => {
            "SELECT COALESCE(NULLIF(model, ''), '(none)'), NULL, SUM(n_requests),
                    SUM(prompt_tokens), SUM(completion_tokens), SUM(n_errors)
             FROM usage_hourly u"
        }
        By::Key => {
            // `key_id = -1` is the rollup's placeholder for traffic that
            // carried no valid key — a rejected request. It has no page to
            // link to, which `id` being NULL says.
            "SELECT COALESCE(k.name, CASE WHEN u.key_id < 0 THEN '(no key)' ELSE '(deleted key)' END),
                    CASE WHEN u.key_id < 0 THEN NULL ELSE u.key_id END, SUM(u.n_requests),
                    SUM(u.prompt_tokens), SUM(u.completion_tokens), SUM(u.n_errors)
             FROM usage_hourly u LEFT JOIN api_keys k ON k.id = u.key_id"
        }
    };
    // The key breakdown joins, so its columns need the table alias.
    let where_extra = match by {
        By::Model => filter.sql(""),
        By::Key => filter.sql("u."),
    };
    let group = match by {
        By::Model => "GROUP BY model",
        By::Key => "GROUP BY u.key_id",
    };
    let hour_col = match by {
        By::Model => "hour",
        By::Key => "u.hour",
    };
    let limit_clause = if limit > 0 {
        format!(" LIMIT {limit}")
    } else {
        String::new()
    };
    let sql = format!(
        "{select} WHERE {hour_col} >= :since{where_extra} {group} ORDER BY 3 DESC{limit_clause}"
    );
    db.call(move |c| {
        let mut stmt = c.prepare(&sql)?;
        let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
        for (name, value) in binds.iter() {
            params.push((name, value.as_ref()));
        }
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok(Row {
                name: r.get(0)?,
                id: r.get(1)?,
                requests: r.get(2)?,
                prompt_tokens: r.get(3)?,
                completion_tokens: r.get(4)?,
                errors: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

/// The single-number totals above a chart.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Totals {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub errors: i64,
}

impl Totals {
    pub fn total_tokens(&self) -> i64 {
        self.prompt_tokens + self.completion_tokens
    }
}

pub async fn totals(db: &Db, r: Range, filter: &Filter) -> Result<Totals> {
    let since = r.since(db).await?;
    let where_extra = filter.sql("");
    let binds = filter.bind(since);
    let sql = format!(
        "SELECT COALESCE(SUM(n_requests), 0), COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0), COALESCE(SUM(n_errors), 0)
         FROM usage_hourly WHERE hour >= :since{where_extra}"
    );
    db.call(move |c| {
        let mut stmt = c.prepare(&sql)?;
        let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
        for (name, value) in binds.iter() {
            params.push((name, value.as_ref()));
        }
        Ok(stmt.query_row(params.as_slice(), |r| {
            Ok(Totals {
                requests: r.get(0)?,
                prompt_tokens: r.get(1)?,
                completion_tokens: r.get(2)?,
                errors: r.get(3)?,
            })
        })?)
    })
    .await
}

/// Per-key totals for the keys table, all of history, in one query rather
/// than one per row.
pub async fn totals_by_key(db: &Db) -> Result<std::collections::HashMap<i64, Totals>> {
    db.call(|c| {
        let mut stmt = c.prepare(
            "SELECT key_id, SUM(n_requests), SUM(prompt_tokens), SUM(completion_tokens),
                    SUM(n_errors)
             FROM usage_hourly GROUP BY key_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Totals {
                    requests: r.get(1)?,
                    prompt_tokens: r.get(2)?,
                    completion_tokens: r.get(3)?,
                    errors: r.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?)
    })
    .await
}

/// Thousands separators, because a token count is read for its magnitude and
/// `1483920` does not give it up at a glance.
pub fn fmt_count(n: i64) -> String {
    let neg = n < 0;
    let digits = n.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testdb;

    async fn seed(db: &Db, key_id: i64, model: &str, hour: i64, prompt: i64, completion: i64) {
        let model = model.to_string();
        db.call(move |c| {
            c.execute(
                "INSERT INTO usage_hourly (key_id, model, hour, n_requests, prompt_tokens,
                                           completion_tokens, n_errors)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, 0)
                 ON CONFLICT(key_id, model, hour) DO UPDATE SET
                    n_requests = n_requests + 1,
                    prompt_tokens = prompt_tokens + excluded.prompt_tokens,
                    completion_tokens = completion_tokens + excluded.completion_tokens",
                rusqlite::params![key_id, model, hour, prompt, completion],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    fn this_hour() -> i64 {
        now() - now() % 3600
    }

    #[tokio::test]
    async fn series_fills_empty_buckets() {
        let db = testdb().await;
        seed(&db, 1, "m", this_hour(), 10, 5).await;
        let s = series(&db, range(Some("24h")), &Filter::default())
            .await
            .unwrap();
        assert_eq!(s.bucket_secs, 3600);
        // 24 hours of hourly buckets, whether or not anything happened in them.
        assert!(s.buckets.len() >= 24, "got {}", s.buckets.len());
        assert_eq!(s.prompt_tokens.iter().sum::<i64>(), 10);
        assert_eq!(s.completion_tokens.iter().sum::<i64>(), 5);
        assert_eq!(s.requests.iter().sum::<i64>(), 1);
    }

    #[tokio::test]
    async fn daily_buckets_merge_hours() {
        let db = testdb().await;
        let h = this_hour();
        seed(&db, 1, "m", h, 10, 0).await;
        seed(&db, 1, "m", h - 3600, 7, 0).await;
        let s = series(&db, range(Some("30d")), &Filter::default())
            .await
            .unwrap();
        assert_eq!(s.bucket_secs, 86_400);
        assert_eq!(s.prompt_tokens.iter().sum::<i64>(), 17);
    }

    #[tokio::test]
    async fn filters_narrow_to_one_key_and_model() {
        let db = testdb().await;
        let h = this_hour();
        seed(&db, 1, "a", h, 10, 1).await;
        seed(&db, 2, "a", h, 100, 1).await;
        seed(&db, 1, "b", h, 1000, 1).await;

        let only_key = Filter {
            key_id: Some(1),
            model: None,
        };
        let t = totals(&db, range(Some("24h")), &only_key).await.unwrap();
        assert_eq!(t.prompt_tokens, 1010);

        let key_and_model = Filter {
            key_id: Some(1),
            model: Some("a".into()),
        };
        let t = totals(&db, range(Some("24h")), &key_and_model)
            .await
            .unwrap();
        assert_eq!(t.prompt_tokens, 10);
    }

    #[tokio::test]
    async fn breakdown_by_model_and_by_key() {
        let db = testdb().await;
        let h = this_hour();
        seed(&db, 1, "a", h, 10, 1).await;
        seed(&db, 1, "a", h, 10, 1).await;
        seed(&db, 1, "b", h, 1, 1).await;

        let rows = breakdown(&db, range(Some("24h")), &Filter::default(), By::Model, 0)
            .await
            .unwrap();
        assert_eq!(rows[0].name, "a");
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].total_tokens(), 22);

        let per_key = breakdown(
            &db,
            range(Some("24h")),
            &Filter {
                key_id: Some(1),
                model: None,
            },
            By::Key,
            0,
        )
        .await
        .unwrap();
        assert_eq!(per_key.len(), 1);
        assert_eq!(per_key[0].requests, 3);
    }

    #[tokio::test]
    async fn totals_by_key_covers_every_key() {
        let db = testdb().await;
        let h = this_hour();
        seed(&db, 1, "a", h, 10, 1).await;
        seed(&db, 2, "a", h, 3, 1).await;
        let map = totals_by_key(&db).await.unwrap();
        assert_eq!(map[&1].total_tokens(), 11);
        assert_eq!(map[&2].total_tokens(), 4);
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_483_920), "1 483 920");
    }
}
