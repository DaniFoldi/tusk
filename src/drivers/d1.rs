//! Cloudflare D1 over the Cloudflare API. A profile is an *account*: the
//! database field is optional — `resolve_database` picks one (most recently
//! used, else the first) when it is blank, and the control plane lists the
//! account's databases for the title-bar picker. Queries go to a database's
//! `/raw` endpoint (columns + rows, order kept); a save batch is one atomic
//! `batch` request. SQLite underneath: catalog queries are shared — but
//! sqlite_version() is blocked server-side, so `version` is static.

use std::sync::Arc;

use serde_json::{Value, json};

use super::sqlite::{self as lite, RowsFn};
use super::{Db, Driver, Fut, WindowReq, http};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, Stmt, WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::Sqlite;
const API: &str = "https://api.cloudflare.com/client/v4";

#[derive(Clone)]
pub struct Api {
    /// Account URL: `{root}/accounts/{account_id}`.
    base: String,
    token: String,
    /// The picked database's id. `None` until one is (account mode).
    database_id: Option<String>,
}

pub struct D1 {
    api: Api,
    rows: RowsFn,
}

/// What `database` on a profile should read before connecting:
/// - blank → the most recently used database that still exists, else the
///   first by name ("" when the account has none);
/// - a legacy profile's database id → that database's *name*;
/// - anything else → itself (a name; the driver maps it to its id).
pub async fn resolve_database(conn: &SavedConnection, token: &str) -> DbResult<String> {
    let api = account_api(conn, token);
    let db = conn.database.trim();
    if db.is_empty() {
        let dbs = api.list().await?;
        return Ok(crate::settings::recent_databases(&conn.name)
            .into_iter()
            .find(|recent| dbs.iter().any(|(name, _, _)| name == recent))
            .or_else(|| dbs.first().map(|(name, _, _)| name.clone()))
            .unwrap_or_default());
    }
    if uuid_shaped(db) {
        let v = http::send_json(api.account_get(&format!("d1/database/{db}"))).await?;
        check(&v)?;
        return Ok(v["result"]["name"].as_str().unwrap_or_default().to_string());
    }
    Ok(db.to_string())
}

/// `xxxxxxxx-xxxx-…`: a D1 database id, as older profiles stored it.
fn uuid_shaped(s: &str) -> bool {
    s.len() == 36
        && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        && s.matches('-').count() == 4
}

fn account_api(conn: &SavedConnection, token: &str) -> Api {
    let root = match conn.opt("endpoint") {
        "" => API.to_string(),
        e => e.trim_end_matches('/').to_string(),
    };
    Api {
        base: format!("{root}/accounts/{}", conn.opt("account_id")),
        token: token.to_string(),
        database_id: None,
    }
}

pub async fn connect(conn: &SavedConnection, token: String) -> DbResult<Db> {
    let api = account_api(conn, &token);
    let name = conn.database.trim().to_string();
    let api = if name.is_empty() {
        // Account mode: the list call proves the token + account and
        // leaves `database_id` unset until a database is picked.
        api.list().await?;
        api
    } else {
        let uuid = api
            .list()
            .await?
            .into_iter()
            .find(|(n, _, _)| n == &name)
            .map(|(_, u, _)| u)
            .ok_or_else(|| format!("No D1 database named \"{name}\" in this account"))?;
        let probe = api.with_database(uuid);
        probe.raw("SELECT 1".into(), Vec::new()).await?;
        probe
    };
    let a = api.clone();
    let rows: RowsFn = Arc::new(move |sql: String| {
        let a = a.clone();
        Box::pin(async move { a.rows(sql).await })
    });
    Ok(Db::new(D1 { api, rows }))
}

/// Cloudflare's envelope: `success` + `errors[].message`.
fn check(v: &Value) -> DbResult<()> {
    if v["success"] == Value::Bool(true) {
        return Ok(());
    }
    let msg = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Err(if msg.is_empty() {
        "D1 request failed".into()
    } else {
        msg
    })
}

fn params(p: &[Option<String>]) -> Vec<Value> {
    p.iter()
        .map(|v| v.clone().map(Value::String).unwrap_or(Value::Null))
        .collect()
}

impl Api {
    /// The picked database's REST URL.
    fn db_url(&self) -> DbResult<String> {
        let id = self
            .database_id
            .as_deref()
            .ok_or("No D1 database picked — choose one from the database menu")?;
        Ok(format!("{}/d1/database/{id}", self.base))
    }

    fn with_database(mut self, id: String) -> Self {
        self.database_id = Some(id);
        self
    }

    /// A GET against the account (control plane).
    fn account_get(&self, path: &str) -> reqwest::RequestBuilder {
        http::client()
            .get(format!("{}/{path}", self.base))
            .bearer_auth(&self.token)
    }

    /// A POST against the picked database's REST subtree.
    fn db_post(&self, path: &str, body: Value) -> DbResult<reqwest::RequestBuilder> {
        Ok(http::client()
            .post(format!("{}/{}", self.db_url()?, path))
            .bearer_auth(&self.token)
            .json(&body))
    }

    /// The account's databases — `(name, uuid, file_size)` by name. Pages
    /// of 100, capped at 1000 (an account may hold tens of thousands).
    async fn list(&self) -> DbResult<Vec<(String, String, u64)>> {
        let mut out = Vec::new();
        for page in 1..=10 {
            let v =
                http::send_json(self.account_get(&format!("d1/database?page={page}&per_page=100")))
                    .await?;
            check(&v)?;
            let batch = v["result"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|d| {
                    (
                        d["name"].as_str().unwrap_or_default().to_string(),
                        d["uuid"].as_str().unwrap_or_default().to_string(),
                        d["file_size"].as_u64().unwrap_or(0),
                    )
                })
                .collect::<Vec<_>>();
            let done = batch.len() < 100;
            out.extend(batch);
            if done {
                break;
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// One statement: (columns, rows, changes).
    async fn raw(
        &self,
        sql: String,
        p: Vec<Option<String>>,
    ) -> DbResult<(Vec<String>, Vec<Vec<Value>>, u64)> {
        let v = http::send_json(self.db_post("raw", json!({"sql": sql, "params": params(&p)}))?)
            .await?;
        check(&v)?;
        let r = v["result"]
            .as_array()
            .and_then(|a| a.last())
            .cloned()
            .unwrap_or_default();
        let res = &r["results"];
        let cols = res["columns"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| c.as_str().unwrap_or("").to_string())
            .collect();
        let rows = res["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r.as_array().cloned().unwrap_or_default())
            .collect();
        Ok((cols, rows, r["meta"]["changes"].as_u64().unwrap_or(0)))
    }

    async fn rows(&self, sql: String) -> DbResult<Vec<Value>> {
        let sql = super::trim_sql(&sql);
        let this = self.clone();
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            let (cols, rows, _) = this.raw(sql, Vec::new()).await?;
            Ok(super::objects_from(&cols, rows))
        })
        .await
    }

    /// Statements as one atomic batch.
    async fn batch(&self, stmts: &[Stmt]) -> DbResult<u64> {
        let batch: Vec<Value> = stmts
            .iter()
            .map(|s| json!({"sql": s.sql, "params": params(&s.params)}))
            .collect();
        let v = http::send_json(self.db_post("query", json!({ "batch": batch }))?).await?;
        check(&v)?;
        Ok(v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r["meta"]["changes"].as_u64().unwrap_or(0))
            .sum())
    }
}

impl Driver for D1 {
    fn engine(&self) -> Engine {
        Engine::CloudflareD1
    }
    fn default_schema(&self) -> Option<String> {
        Some("main".into())
    }
    fn version(&self) -> Fut<String> {
        // D1 rejects sqlite_version() with "not authorized to use
        // function", so the engine version isn't reachable through SQL.
        // Report the service, the way BigQuery does.
        Box::pin(async { Ok("Cloudflare D1".into()) })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let api = self.api.clone();
        Box::pin(async move { Ok(api.list().await?.into_iter().map(|(n, _, _)| n).collect()) })
    }
    /// Bytes per database from the account list (`file_size`; 0 while a
    /// database has never been queried — those report nothing).
    fn database_sizes(&self) -> Fut<Vec<(String, u64)>> {
        let api = self.api.clone();
        Box::pin(async move {
            Ok(api
                .list()
                .await?
                .into_iter()
                .filter(|(_, _, size)| *size > 0)
                .map(|(n, _, size)| (n, size))
                .collect())
        })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        Box::pin(async { Ok(vec!["main".to_string()]) })
    }
    fn objects(&self, _schema: String) -> Fut<ObjectTree> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::objects(&rows).await })
    }
    fn columns(&self, _schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::columns(&rows, &table).await })
    }
    fn count(&self, _schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let rows = self.rows.clone();
        Box::pin(async move {
            let r = rows(format!(
                "SELECT COUNT(*) AS n FROM {} {}",
                D.quote(&table),
                lite::where_sql(&filter)
            ))
            .await?;
            Ok(r.first().and_then(|r| r["n"].as_i64()).unwrap_or(0))
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        (self.rows)(lite::window_sql(&req))
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let f = (self.rows)(sql);
        Box::pin(async move {
            let mut r = f.await?;
            r.truncate(limit.max(0) as usize);
            Ok(r)
        })
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let api = self.api.clone();
        Box::pin(async move {
            Ok(api
                .raw(
                    format!("SELECT * FROM ({}) LIMIT 0", super::trim_sql(&sql)),
                    Vec::new(),
                )
                .await?
                .0)
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let api = self.api.clone();
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                let stmts: Vec<Stmt> = db::split_statements(&sql)
                    .into_iter()
                    .map(Stmt::plain)
                    .collect();
                api.batch(&stmts).await
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let api = self.api.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            let label = stmts
                .iter()
                .map(|s| s.sql.as_str())
                .collect::<Vec<_>>()
                .join(";\n");
            let r = api.batch(&stmts).await;
            crate::console::record(
                &label,
                started,
                crate::console::Source::Data,
                r.as_ref().err().map(String::as_str),
            );
            r
        })
    }
    fn script(&self, kind: ObjKind, _schema: String, name: String, which: Script) -> Fut<String> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::script(&rows, kind, &name, which).await })
    }
    fn triggers(&self, _schema: String, table: String) -> Fut<Vec<Value>> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::triggers(&rows, &table).await })
    }
    fn indexes(&self, _schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let rows = self.rows.clone();
        Box::pin(async move { lite::indexes(&rows, &table).await })
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    /// Against a local stand-in for the D1 API (scratch `d1_mock.py`, SQLite
    /// underneath). The mock must also serve the account-level database
    /// list: `GET /accounts/{acct}/d1/database` — connect resolves the
    /// profile's database name to its id through it.
    #[test]
    fn live_d1_mock() {
        if !live::reachable(38787) {
            return;
        }
        let mut c = live::conn(Engine::CloudflareD1, 0, "", "db-1");
        c.options.insert("account_id".into(), "acct".into());
        c.options
            .insert("endpoint".into(), "http://127.0.0.1:38787".into());
        let rt = crate::db::runtime();
        let db = rt.block_on(super::connect(&c, "tok".into())).unwrap();
        rt.block_on(db.driver().exec(
            "DROP TABLE IF EXISTS people; CREATE TABLE people (id INTEGER PRIMARY KEY, email TEXT);
             INSERT INTO people (email) VALUES ('a@x'), ('b@x');"
                .into(),
        ))
        .unwrap();
        live::exercise(c, "tok", "people", "email");
    }

    #[test]
    fn uuid_shaped_only_matches_ids() {
        let f = super::uuid_shaped;
        assert!(f("6a1e6b2f-5c0f-4d9e-9f6c-2d8e1a4b7c9d"));
        assert!(!f("db-1"));
        assert!(!f("6a1e6b2f-5c0f-4d9e-9f6c-2d8e1a4b7c9"));
        assert!(!f(""));
    }
}
