//! ClickHouse over its HTTP interface (port 8123 / 8443 with SSL). Rows come
//! back as `FORMAT JSON`; the catalog is `system.*`. Row edits are
//! mutations: the grid's UPDATE becomes `ALTER TABLE … UPDATE`.

use serde_json::Value;

use super::{Db, Driver, Fut, WindowReq, http};
use crate::db::{
    self, DbResult, GridColumnMeta, ObjectTree, SavedConnection, SslMode, Stmt, WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::ClickHouse;

#[derive(Clone)]
pub struct ClickHouse {
    url: String,
    user: String,
    password: String,
    database: String,
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let scheme = if conn.ssl == SslMode::Require || port == 8443 {
        "https"
    } else {
        "http"
    };
    let ch = ClickHouse {
        url: format!("{scheme}://{host}:{port}/"),
        user: if conn.user.is_empty() {
            "default".into()
        } else {
            conn.user.clone()
        },
        password,
        database: if conn.database.is_empty() {
            "default".into()
        } else {
            conn.database.clone()
        },
    };
    ch.query("SELECT 1 AS ok".into(), 1).await?;
    Ok(Db::new(ch))
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

/// Does the statement return rows (so `FORMAT JSON` can be appended)?
fn returns_rows(sql: &str) -> bool {
    let head = sql.split_whitespace().next().unwrap_or("").to_uppercase();
    matches!(
        head.as_str(),
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "EXISTS"
    )
}

impl ClickHouse {
    async fn post(&self, sql: String) -> DbResult<String> {
        let req = http::client()
            .post(&self.url)
            .query(&[
                ("database", self.database.as_str()),
                ("output_format_json_quote_64bit_integers", "0"),
                ("output_format_json_quote_denormals", "1"),
            ])
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql);
        http::send(req)
            .await
            .map_err(|e| e.replace("HTTP 500: ", "").replace("HTTP 400: ", ""))
    }

    /// Rows of a statement (objects in column order).
    async fn query(&self, sql: String, limit: i64) -> DbResult<Vec<Value>> {
        let sql = super::trim_sql(&sql);
        let this = self.clone();
        let label = sql.clone();
        db::run_logged(label, crate::console::Source::Data, async move {
            if !returns_rows(&sql) {
                this.post(sql).await?;
                return Ok(Vec::new());
            }
            let text = this.post(format!("{sql}\nFORMAT JSON")).await?;
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| format!("bad JSON from ClickHouse: {e}"))?;
            let mut rows: Vec<Value> = v["data"].as_array().cloned().unwrap_or_default();
            rows.truncate(limit.max(0) as usize);
            Ok(rows)
        })
        .await
    }

    fn rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone();
        Box::pin(async move { this.query(sql, limit).await })
    }
}

fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

/// `?` placeholders replaced by literals (ClickHouse's HTTP API binds by name).
pub fn inline(sql: &str, params: &[Option<String>], d: Dialect) -> String {
    let mut out = String::new();
    let mut it = params.iter();
    let mut quoted = false;
    for c in sql.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                out.push(c);
            }
            '?' if !quoted => match it.next() {
                Some(Some(v)) => out.push_str(&d.literal(v)),
                Some(None) | None => out.push_str("NULL"),
            },
            _ => out.push(c),
        }
    }
    out
}

/// `UPDATE t SET a = 1 WHERE …` → `ALTER TABLE t UPDATE a = 1 WHERE …`.
fn as_mutation(sql: &str) -> String {
    let t = sql.trim_start();
    if !t.to_uppercase().starts_with("UPDATE ") {
        return sql.to_string();
    }
    let rest = &t[7..];
    match rest.find(" SET ") {
        Some(i) => format!("ALTER TABLE {} UPDATE {}", &rest[..i], &rest[i + 5..]),
        None => sql.to_string(),
    }
}

impl Driver for ClickHouse {
    fn engine(&self) -> Engine {
        Engine::ClickHouse
    }
    fn default_schema(&self) -> Option<String> {
        Some(self.database.clone())
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let f = self.rows("SELECT version() AS v".into(), 1);
        Box::pin(async move {
            Ok(format!(
                "ClickHouse {}",
                f.await?.first().map(|r| s(&r["v"])).unwrap_or_default()
            ))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.rows(
            "SELECT name FROM system.databases
              WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name"
                .into(),
            10_000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["name"])).collect()) })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let f = self.rows(
            format!(
                "SELECT name, engine FROM system.tables WHERE database = {} AND NOT is_temporary ORDER BY name",
                D.literal(&schema)
            ),
            100_000,
        );
        Box::pin(async move {
            let mut tree = ObjectTree::default();
            for r in f.await? {
                match s(&r["engine"]).as_str() {
                    "View" => tree.views.push(s(&r["name"])),
                    "MaterializedView" => tree.matviews.push(s(&r["name"])),
                    _ => tree.tables.push(s(&r["name"])),
                }
            }
            Ok(tree)
        })
    }

    /// On-disk bytes per table (system.tables.total_bytes): real for the
    /// MergeTree family, 0 for Memory / Log / views.
    fn table_sizes(&self, schema: String) -> Fut<Vec<(String, u64)>> {
        let f = self.rows(
            format!(
                "SELECT name, total_bytes FROM system.tables
                  WHERE database = {} AND engine NOT IN ('View', 'MaterializedView')
                  ORDER BY name",
                D.literal(&schema)
            ),
            100_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| {
                    (
                        s(&r["name"]),
                        r["total_bytes"]
                            .as_f64()
                            .map(|v| if v.is_finite() && v > 0. { v as u64 } else { 0 })
                            .unwrap_or(0),
                    )
                })
                .collect())
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let f = self.rows(
            format!(
                "SELECT name, type, default_expression AS dflt, is_in_primary_key AS pk, comment
                   FROM system.columns WHERE database = {} AND table = {} ORDER BY position",
                D.literal(&schema),
                D.literal(&table)
            ),
            10_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| {
                    let ty = s(&r["type"]);
                    let inner = ty
                        .strip_prefix("Nullable(")
                        .and_then(|t| t.strip_suffix(')'))
                        .unwrap_or(&ty)
                        .to_string();
                    let enum_values = if inner.starts_with("Enum") {
                        inner
                            .split('\'')
                            .skip(1)
                            .step_by(2)
                            .map(str::to_string)
                            .collect()
                    } else {
                        Vec::new()
                    };
                    GridColumnMeta {
                        name: s(&r["name"]),
                        pg_type: super::short_type(&inner),
                        nullable: ty.starts_with("Nullable("),
                        default: Some(s(&r["dflt"])).filter(|d| !d.is_empty()),
                        comment: Some(s(&r["comment"])).filter(|d| !d.is_empty()),
                        is_pk: r["pk"].as_i64() == Some(1) || r["pk"] == Value::Bool(true),
                        foreign_key: None,
                        enum_values,
                        sql_type: ty,
                    }
                })
                .collect())
        })
    }
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let f = self.rows(
            format!(
                "SELECT count() AS n FROM {} {}",
                D.qualified(&schema, &table, true),
                where_sql(&filter)
            ),
            1,
        );
        Box::pin(async move { Ok(f.await?.first().and_then(|r| r["n"].as_i64()).unwrap_or(0)) })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let sql = format!(
            "SELECT * FROM {} {} {} LIMIT {} OFFSET {}",
            D.qualified(&req.schema, &req.table, true),
            where_sql(&req.filter),
            req.order_by.as_deref().unwrap_or(""),
            req.limit.min(i64::MAX / 2),
            req.offset
        );
        self.rows(sql, req.limit)
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.rows(sql, limit)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let this = self.clone();
        Box::pin(async move {
            let text = this
                .post(format!("{}\nLIMIT 0\nFORMAT JSON", super::trim_sql(&sql)))
                .await?;
            let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            Ok(v["meta"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| s(&m["name"]))
                .collect())
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                this.post(super::trim_sql(&sql)).await?;
                Ok(0)
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone();
        Box::pin(async move {
            // No transactions: statements run one after another.
            let mut n = 0;
            for st in stmts {
                let sql = as_mutation(&inline(&st.sql, &st.params, D));
                let started = std::time::Instant::now();
                match this.post(sql.clone()).await {
                    Ok(_) => {
                        crate::console::record(&sql, started, crate::console::Source::Data, None)
                    }
                    Err(e) => {
                        crate::console::record(
                            &sql,
                            started,
                            crate::console::Source::Data,
                            Some(&e),
                        );
                        return Err(format!("{e}\n  in: {sql}"));
                    }
                }
                n += 1;
            }
            Ok(n)
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let this = self.clone();
        let cols = self.columns(schema, name);
        Box::pin(async move {
            match which {
                Script::Create => {
                    let r = this.query(format!("SHOW CREATE TABLE {target}"), 1).await?;
                    Ok(r.first()
                        .and_then(|r| r.as_object())
                        .and_then(|o| o.values().next())
                        .map(s)
                        .unwrap_or_default())
                }
                Script::Drop => Ok(format!(
                    "DROP {} {target};",
                    if kind == ObjKind::View {
                        "VIEW"
                    } else {
                        "TABLE"
                    }
                )),
                w => Ok(super::dml_script(D, w, &target, &cols.await?)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::{Dialect, Engine};

    #[test]
    fn mutations_and_params() {
        assert_eq!(
            super::as_mutation("UPDATE `shop`.`t` SET `a` = 'x' WHERE `id` = 1"),
            "ALTER TABLE `shop`.`t` UPDATE `a` = 'x' WHERE `id` = 1"
        );
        assert_eq!(
            super::inline(
                "a = ? AND b = '?' AND c = ?",
                &[Some("it's".into()), None],
                Dialect::ClickHouse
            ),
            "a = 'it''s' AND b = '?' AND c = NULL"
        );
    }

    #[test]
    fn live_clickhouse() {
        if live::reachable(38123) {
            live::exercise(
                live::conn(Engine::ClickHouse, 38123, "default", "shop"),
                "tusk",
                "customers",
                "email",
            );
        }
    }
}
