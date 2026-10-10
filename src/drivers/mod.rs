//! One driver per engine behind a common interface.
//!
//! The workspace holds a [`Db`] (a shared [`Driver`]) and never talks to a
//! client library directly: sidebar objects, grid windows, the SQL editor,
//! saves and scripts all go through these calls. Every driver returns rows
//! as JSON objects in column order (numbers as numbers, everything else as
//! text), the shape `row_to_json` gives on Postgres.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;

use crate::db::{DbResult, EditSource, GridColumnMeta, IndexDef, ObjectTree, Stmt, WhereClause};
use crate::engine::{Caps, Dialect, Engine};
use crate::objects::{ObjKind, Script};

pub mod bigquery;
pub mod cassandra;
pub mod clickhouse;
pub mod d1;
pub mod duckdb;
pub mod dynamo;
pub mod edit_source;
pub mod http;
pub mod libsql;
pub mod mongo;
pub mod mssql;
pub mod mysql;
pub mod oracle;
pub mod pg;
pub mod redis;
pub mod sessions;
pub mod snowflake;
pub mod sqlite;
pub mod vertica;

pub type Fut<T> = BoxFuture<'static, DbResult<T>>;

/// A page of a table for the data grid.
#[derive(Clone, Debug)]
pub struct WindowReq {
    pub schema: String,
    pub table: String,
    pub filter: Option<WhereClause>,
    /// Trusted ORDER BY fragment (quoted identifiers + keywords).
    pub order_by: Option<String>,
    /// Put the engine's row key (see [`Dialect::row_key`]) first.
    pub with_key: bool,
    pub limit: i64,
    pub offset: i64,
}

pub trait Driver: Send + Sync + 'static {
    fn engine(&self) -> Engine;

    fn caps(&self) -> Caps {
        self.engine().caps()
    }

    fn dialect(&self) -> Dialect {
        self.engine().dialect()
    }

    fn version(&self) -> Fut<String>;
    fn databases(&self) -> Fut<Vec<String>>;
    fn schemas(&self) -> Fut<Vec<String>>;
    fn objects(&self, schema: String) -> Fut<ObjectTree>;
    /// Bytes on disk per table of a schema, for the sidebar. Only the
    /// engines with a cheap catalog answer implement it; the default is
    /// none.
    fn table_sizes(&self, _schema: String) -> Fut<Vec<(String, u64)>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>>;
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64>;
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>>;
    /// A statement that returns rows, capped at `limit`.
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>>;
    /// The agent path (run_sql in Read-only mode): a server-enforced
    /// READ ONLY transaction where the protocol has one. The default is
    /// the plain path — for those engines the statement classifier stays
    /// the only gate (see mcp_bridge::agent_may_run).
    fn query_rows_readonly(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.query_rows(sql, limit)
    }
    /// Column names of a statement's result (for empty results).
    fn query_columns(&self, sql: String) -> Fut<Vec<String>>;
    /// A statement without rows: affected count.
    fn exec(&self, sql: String) -> Fut<u64>;
    /// Parameterised statements, in one transaction where the engine has them.
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64>;
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String>;

    /// Can a SELECT's result be edited in place (one table, key present)?
    /// Whether a query's result can be edited in place (and how its columns
    /// map to a table). `schema`: the workspace's current one, for
    /// unqualified names; `result`: the result's column names.
    fn edit_source(
        &self,
        sql: String,
        schema: String,
        result: Vec<String>,
    ) -> Fut<Result<EditSource, String>> {
        if self.dialect() == Dialect::NoSql && self.engine() != Engine::DynamoDb {
            return Box::pin(async {
                Ok(Err("read-only: command results can't be edited".to_string()))
            });
        }
        edit_source::generic(|s, t| self.columns(s, t), &sql, schema, result)
    }
    fn user_types(&self, _schema: String) -> Fut<Vec<String>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn triggers(&self, _schema: String, _table: String) -> Fut<Vec<Value>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn indexes(&self, _schema: String, _table: String) -> Fut<Vec<IndexDef>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    /// The Postgres pool, for Postgres-only features (backup, roles, …).
    fn pg(&self) -> Option<&sqlx::PgPool> {
        None
    }
    /// The schema the sidebar opens on (else `public`, else the first).
    fn default_schema(&self) -> Option<String> {
        None
    }
    /// Does [`Driver::window`] honour `with_key` (a row-key column first)?
    /// Server sessions for the process list; the first column is the id
    /// [`Driver::signal_session`] takes.
    fn sessions(&self) -> Fut<Vec<Value>> {
        match sessions::list_sql(self.engine()) {
            Some(sql) => self.query_rows(sql.into(), 5_000),
            None => {
                let msg = format!("{} has no process list", self.engine().label());
                Box::pin(async move { Err(msg) })
            }
        }
    }
    /// Cancel session `id`'s running statement, or end the session (`kill`).
    fn signal_session(&self, id: String, kill: bool) -> Fut<()> {
        match sessions::signal_sql(self.engine(), &id, kill) {
            Ok(sql) => {
                let f = self.exec(sql);
                Box::pin(async move { f.await.map(|_| ()) })
            }
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
    fn row_key(&self) -> bool {
        self.dialect().row_key().is_some()
    }
}

/// Per-table (column, type) lists a driver keeps for typed literals.
pub type Shapes =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<(String, String)>>>>;

/// The live connection the workspace shares.
#[derive(Clone)]
pub struct Db(pub Arc<dyn Driver>);

impl Db {
    pub fn new(d: impl Driver) -> Self {
        Self(Arc::new(d))
    }
    pub fn engine(&self) -> Engine {
        self.0.engine()
    }
    pub fn caps(&self) -> Caps {
        self.0.caps()
    }
    pub fn dialect(&self) -> Dialect {
        self.0.dialect()
    }
    pub fn driver(&self) -> &dyn Driver {
        &*self.0
    }
    pub fn pg(&self) -> Option<&sqlx::PgPool> {
        self.0.pg()
    }
    pub fn row_key(&self) -> bool {
        self.0.row_key()
    }
    pub fn quote(&self, name: &str) -> String {
        self.dialect().quote(name)
    }
    /// `schema.table`, or just the table on schema-less engines.
    pub fn qualified(&self, schema: &str, table: &str) -> String {
        self.dialect().qualified(schema, table, self.caps().schemas)
    }
}

/// Rows of differing shape (documents) as one table: every key seen, in
/// first-seen order, missing ones null — result grids place cells by position.
pub fn uniform_rows(rows: Vec<Value>) -> Vec<Value> {
    let mut keys: Vec<String> = Vec::new();
    for r in &rows {
        for k in r.as_object().into_iter().flat_map(|o| o.keys()) {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    rows.into_iter()
        .map(|r| {
            Value::Object(
                keys.iter()
                    .map(|k| (k.clone(), r.get(k).cloned().unwrap_or(Value::Null)))
                    .collect(),
            )
        })
        .collect()
}

/// JSON objects from column names and row values, in order.
pub fn objects_from(columns: &[String], rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter()
        .map(|r| {
            let mut m = serde_json::Map::new();
            for (i, v) in r.into_iter().enumerate() {
                let key = match columns.get(i) {
                    Some(c) if !m.contains_key(c) => c.clone(),
                    Some(c) => format!("{c}_{i}"),
                    None => format!("column{}", i + 1),
                };
                m.insert(key, v);
            }
            Value::Object(m)
        })
        .collect()
}

/// A type name as the grid's short Postgres-style name (`int4`, `bool`,
/// `float8`, `json`, `timestamptz`, …): the render / editor policy keys.
pub fn short_type(sql_type: &str) -> String {
    let t = sql_type.trim().to_lowercase();
    let base = t
        .split(['(', ' '])
        .next()
        .unwrap_or("")
        .trim_start_matches("unsigned")
        .to_string();
    let base = base
        .trim_start_matches("nullable")
        .trim_matches(|c| c == '(' || c == ')')
        .to_string();
    match base.as_str() {
        "int" | "integer" | "int4" | "mediumint" | "int32" | "uint32" | "uint16" | "int16" => {
            "int4".into()
        }
        "bigint" | "int8" | "int64" | "uint64" | "long" | "number" if !t.contains(',') => {
            "int8".into()
        }
        "smallint" | "int2" | "tinyint" | "int8_t" | "uint8" => {
            if t == "tinyint(1)" {
                "bool".into()
            } else {
                "int2".into()
            }
        }
        "bool" | "boolean" | "bit" => "bool".into(),
        "float" | "double" | "real" | "float4" | "float8" | "float32" | "float64"
        | "binary_float" | "binary_double" => "float8".into(),
        "numeric" | "decimal" | "dec" | "money" | "smallmoney" | "bignumeric" | "decimal32"
        | "decimal64" | "decimal128" => "numeric".into(),
        "json" | "jsonb" | "variant" | "object" | "array" | "map" | "document" => "json".into(),
        "date" | "date32" => "date".into(),
        "timestamptz" | "datetimeoffset" => "timestamptz".into(),
        "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "datetime64"
        | "timestamp_ntz" | "timestamp_ltz" | "timestamp_tz" => {
            if t.contains("with time zone") || t.contains("_tz") || t.contains("ltz") {
                "timestamptz".into()
            } else {
                "timestamp".into()
            }
        }
        "uuid" | "uniqueidentifier" => "uuid".into(),
        "bytea" | "blob" | "binary" | "varbinary" | "longblob" | "mediumblob" | "tinyblob"
        | "raw" | "bytes" => "bytea".into(),
        "text" | "longtext" | "mediumtext" | "tinytext" | "clob" | "nclob" | "ntext" | "string" => {
            "text".into()
        }
        "varchar" | "character" | "char" | "nvarchar" | "nchar" | "varchar2" | "nvarchar2"
        | "fixedstring" => "varchar".into(),
        other => other.to_string(),
    }
}

/// `sql` without a trailing `;` (drivers wrap / page statements).
pub fn trim_sql(sql: &str) -> String {
    sql.trim().trim_end_matches(';').trim().to_string()
}

/// Bytes shown as `\x…` hex (the Postgres `bytea` text form).
pub fn hex(bytes: &[u8]) -> Value {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    Value::String(s)
}

/// A minimal CREATE TABLE from column metadata (engines without a native
/// "show create" statement).
pub fn create_table_from(d: Dialect, qualified: &str, cols: &[GridColumnMeta]) -> String {
    let mut lines: Vec<String> = cols
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", d.quote(&c.name), c.sql_type);
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            if let Some(def) = &c.default {
                l.push_str(&format!(" DEFAULT {def}"));
            }
            l
        })
        .collect();
    let pk: Vec<String> = cols
        .iter()
        .filter(|c| c.is_pk)
        .map(|c| d.quote(&c.name))
        .collect();
    if !pk.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"))
}

/// Generic DML templates (Select / Insert / Update / Delete / Drop /
/// Truncate) from column metadata; `Create` is the driver's own.
pub fn dml_script(d: Dialect, which: Script, qualified: &str, cols: &[GridColumnMeta]) -> String {
    match which {
        Script::Drop => format!("DROP TABLE {qualified};"),
        Script::Truncate => format!("TRUNCATE TABLE {qualified};"),
        _ => crate::objects::dml_template_with(d, which, qualified, cols),
    }
}

/// A grid save statement taken apart, for engines that don't run SQL
/// (Redis, MongoDB, DynamoDB) or bind by type (Cassandra). The grid writes
/// these in a fixed shape: `"quoted"` identifiers and `?` parameters.
#[derive(Clone, Debug, PartialEq)]
pub enum GridOp {
    Insert {
        schema: Option<String>,
        table: String,
        values: Vec<(String, Option<String>)>,
    },
    Update {
        schema: Option<String>,
        table: String,
        set: Vec<(String, Option<String>)>,
        key: Vec<(String, Option<String>)>,
    },
    Delete {
        schema: Option<String>,
        table: String,
        key: Vec<(String, Option<String>)>,
    },
}

/// Parse a grid statement (see [`GridOp`]); `None` for anything else.
pub fn parse_grid_stmt(stmt: &Stmt) -> Option<GridOp> {
    let sql = stmt.sql.trim();
    let mut params = stmt.params.iter().cloned();
    // `"a"."b"` → last part; returns (name, rest).
    fn ident(s: &str) -> Option<(String, &str)> {
        let (mut parts, rest) = path(s)?;
        Some((parts.pop()?, rest))
    }
    // `"a"."b"` → (["a", "b"], rest).
    fn path(s: &str) -> Option<(Vec<String>, &str)> {
        let mut s = s.trim_start();
        let mut parts = Vec::new();
        loop {
            // `"name"` or (BigQuery / MySQL style) `` `name` ``.
            let q = s.chars().next().filter(|c| *c == '"' || *c == '`')?;
            let rest = &s[1..];
            let mut out = String::new();
            let mut chars = rest.char_indices();
            let end = loop {
                let (i, c) = chars.next()?;
                if c == q {
                    if rest[i + 1..].starts_with(q) {
                        out.push(q);
                        chars.next();
                        continue;
                    }
                    break i + 1;
                }
                out.push(c);
            };
            s = &rest[end..];
            parts.push(out);
            match s.strip_prefix('.') {
                Some(r) => s = r,
                None => return Some((parts, s)),
            }
        }
    }
    // `"a" = ?, "b" = ?` (sep `,` or `AND`).
    fn pairs(
        mut s: &str,
        sep: &str,
        params: &mut dyn Iterator<Item = Option<String>>,
    ) -> Option<Vec<(String, Option<String>)>> {
        let mut out = Vec::new();
        loop {
            let (name, rest) = ident(s)?;
            let rest = rest.trim_start().strip_prefix('=')?.trim_start();
            let rest = rest.strip_prefix('?')?;
            out.push((name, params.next()?));
            let rest = rest.trim_start();
            match rest.strip_prefix(sep) {
                Some(r) => s = r,
                None => return rest.is_empty().then_some(out),
            }
        }
    }
    // Target table (+ schema when qualified).
    fn target(s: &str) -> Option<(Option<String>, String, &str)> {
        let (mut parts, rest) = path(s)?;
        let table = parts.pop()?;
        Some((parts.pop(), table, rest))
    }
    let upper = sql.to_uppercase();
    if let Some(rest) = upper.starts_with("DELETE FROM ").then(|| &sql[12..]) {
        let (schema, table, rest) = target(rest)?;
        let pred = rest.trim_start().strip_prefix("WHERE ")?;
        return Some(GridOp::Delete {
            schema,
            table,
            key: pairs(pred, "AND ", &mut params)?,
        });
    }
    if let Some(rest) = upper.starts_with("INSERT INTO ").then(|| &sql[12..]) {
        let (schema, table, rest) = target(rest)?;
        let rest = rest.trim_start();
        if rest.to_uppercase().starts_with("DEFAULT VALUES") || rest.starts_with("() VALUES") {
            return Some(GridOp::Insert {
                schema,
                table,
                values: Vec::new(),
            });
        }
        let mut cols = Vec::new();
        let mut r = rest.strip_prefix('(')?;
        loop {
            let (c, rest) = ident(r)?;
            cols.push(c);
            let rest = rest.trim_start();
            if let Some(x) = rest.strip_prefix(',') {
                r = x;
            } else {
                rest.strip_prefix(')')?;
                break;
            }
        }
        let values = cols
            .into_iter()
            .map(|c| (c, params.next().flatten()))
            .collect();
        return Some(GridOp::Insert {
            schema,
            table,
            values,
        });
    }
    if let Some(rest) = upper.starts_with("UPDATE ").then(|| &sql[7..]) {
        let (schema, table, rest) = target(rest)?;
        let rest = rest.trim_start().strip_prefix("SET ")?;
        let at = rest.find(" WHERE ")?;
        let set = pairs(&rest[..at], ",", &mut params)?;
        let key = pairs(&rest[at + 7..], "AND ", &mut params)?;
        return Some(GridOp::Update {
            schema,
            table,
            set,
            key,
        });
    }
    None
}

/// Open a connection for a saved profile.
pub async fn connect(
    conn: &crate::db::SavedConnection,
    host: String,
    port: u16,
    secret: String,
) -> DbResult<Db> {
    match conn.engine {
        Engine::Vertica => vertica::connect(conn, host, port, secret).await,
        e if e.pg_wire() => pg::connect(conn, host, port, secret).await,
        Engine::MySql | Engine::MariaDb => mysql::connect(conn, host, port, secret).await,
        Engine::Sqlite => sqlite::connect(conn).await,
        Engine::DuckDb => duckdb::connect(conn).await,
        Engine::LibSql => libsql::connect(conn, secret).await,
        Engine::CloudflareD1 => d1::connect(conn, secret).await,
        Engine::MsSql => mssql::connect(conn, host, port, secret).await,
        Engine::Oracle => oracle::connect(conn, host, port, secret).await,
        Engine::ClickHouse => clickhouse::connect(conn, host, port, secret).await,
        Engine::Snowflake => snowflake::connect(conn, secret).await,
        Engine::BigQuery => bigquery::connect(conn, secret).await,
        Engine::Redis | Engine::Valkey => redis::connect(conn, host, port, secret).await,
        Engine::MongoDb => mongo::connect(conn, host, port, secret).await,
        Engine::Cassandra => cassandra::connect(conn, host, port, secret).await,
        Engine::DynamoDb => dynamo::connect(conn, secret).await,
        _ => Err("unknown engine".into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn grid_statements_parse() {
        use super::{GridOp, parse_grid_stmt};
        use crate::db::Stmt;
        let st = |sql: &str, p: &[Option<&str>]| Stmt {
            sql: sql.into(),
            params: p.iter().map(|v| v.map(str::to_string)).collect(),
        };
        assert_eq!(
            parse_grid_stmt(&st(
                r#"UPDATE "ks"."t" SET "a" = ?, "b""x" = ? WHERE "id" = ? AND "k" = ?"#,
                &[Some("1"), None, Some("7"), Some("z")]
            )),
            Some(GridOp::Update {
                schema: Some("ks".into()),
                table: "t".into(),
                set: vec![("a".into(), Some("1".into())), ("b\"x".into(), None)],
                key: vec![
                    ("id".into(), Some("7".into())),
                    ("k".into(), Some("z".into()))
                ],
            })
        );
        assert_eq!(
            parse_grid_stmt(&st(r#"DELETE FROM "t" WHERE "_id" = ?"#, &[Some("abc")])),
            Some(GridOp::Delete {
                schema: None,
                table: "t".into(),
                key: vec![("_id".into(), Some("abc".into()))]
            })
        );
        assert_eq!(
            parse_grid_stmt(&st(
                r#"INSERT INTO "t" ("a", "b") VALUES (?, ?)"#,
                &[Some("1"), None]
            )),
            Some(GridOp::Insert {
                schema: None,
                table: "t".into(),
                values: vec![("a".into(), Some("1".into())), ("b".into(), None)]
            })
        );
        assert_eq!(
            parse_grid_stmt(&st("DELETE FROM `ds`.`t` WHERE `id` = ?", &[Some("1")])),
            Some(GridOp::Delete {
                schema: Some("ds".into()),
                table: "t".into(),
                key: vec![("id".into(), Some("1".into()))]
            })
        );
        assert_eq!(
            parse_grid_stmt(&st(r#"INSERT INTO "t" DEFAULT VALUES"#, &[])),
            Some(GridOp::Insert {
                schema: None,
                table: "t".into(),
                values: vec![]
            })
        );
    }

    #[test]
    fn objects_keep_order_and_dedupe() {
        let rows = super::objects_from(
            &["b".into(), "a".into(), "b".into()],
            vec![vec![json!(1), json!("x"), json!(null)]],
        );
        let keys: Vec<&String> = rows[0].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["b", "a", "b_2"]);
        assert_eq!(super::hex(&[0xde, 0xad]), json!("\\xdead"));
    }
}

/// Live checks against local servers (docker); each engine's test skips
/// when its server isn't reachable.
#[cfg(test)]
pub(crate) mod live {
    use super::*;
    use crate::db::SavedConnection;

    pub fn reachable(port: u16) -> bool {
        std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            std::time::Duration::from_millis(300),
        )
        .is_ok()
    }

    pub fn conn(engine: Engine, port: u16, user: &str, database: &str) -> SavedConnection {
        let mut c = crate::db::dev_default();
        c.engine = engine;
        c.name = format!("test-{}", engine.label());
        c.host = "127.0.0.1".into();
        c.port = port;
        c.user = user.into();
        c.database = database.into();
        c.ssl = crate::db::SslMode::Disable;
        c
    }

    /// Everything the workspace does with a table, end to end.
    /// `table` needs an integer primary key and a text column `text_col`.
    pub fn exercise(c: SavedConnection, secret: &str, table: &str, text_col: &str) {
        let rt = crate::db::runtime();
        let db = rt
            .block_on(super::connect(
                &c,
                c.host.clone(),
                c.port,
                secret.to_string(),
            ))
            .unwrap_or_else(|e| panic!("{} connect: {e}", c.engine.label()));
        let d = db.driver();
        let v = rt.block_on(d.version()).expect("version");
        assert!(!v.is_empty());
        let schemas = rt.block_on(d.schemas()).expect("schemas");
        assert!(!schemas.is_empty(), "schemas");
        let schema = d
            .default_schema()
            .filter(|s| schemas.contains(s))
            .unwrap_or_else(|| schemas[0].clone());
        let tree = rt.block_on(d.objects(schema.clone())).expect("objects");
        assert!(
            tree.tables.iter().any(|t| t.eq_ignore_ascii_case(table)),
            "{table} not in {:?}",
            tree.tables
        );
        let cols = rt
            .block_on(d.columns(schema.clone(), table.into()))
            .expect("columns");
        assert!(!cols.is_empty(), "columns");
        let pk = cols
            .iter()
            .find(|c| c.is_pk)
            .unwrap_or_else(|| panic!("no pk in {cols:?}"))
            .clone();
        let n = rt
            .block_on(d.count(schema.clone(), table.into(), None))
            .expect("count");
        assert!(n >= 1, "count {n}");
        let rows = rt
            .block_on(d.window(WindowReq {
                schema: schema.clone(),
                table: table.into(),
                filter: None,
                order_by: Some(format!("ORDER BY {}", db.quote(&pk.name))),
                with_key: false,
                limit: 2,
                offset: 0,
            }))
            .expect("window");
        assert_eq!(rows.len() as i64, n.min(2), "window rows");
        let first = rows[0].as_object().expect("row object");
        assert!(
            first.keys().any(|k| k.eq_ignore_ascii_case(&pk.name)),
            "row keys {:?}",
            first.keys()
        );
        // A text filter in this dialect.
        let ix = cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(text_col))
            .expect("text column");
        let spec = crate::filter::FilterSpec {
            enabled: true,
            column: Some(ix),
            op: crate::filter::FilterOp::Contains,
            value: "@".into(),
        };
        let w = crate::filter::build_where_for(&[spec], &cols, db.dialect());
        rt.block_on(d.count(schema.clone(), table.into(), w))
            .expect("filtered count");
        // Write the pk row's text value back unchanged, the way the grid saves.
        let pk_val = first
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&pk.name))
            .map(|(_, v)| crate::grid::cell_text(v))
            .unwrap();
        let txt = first
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(text_col))
            .map(|(_, v)| crate::grid::cell_text(v))
            .unwrap();
        let dl = db.dialect();
        let col_meta = &cols[ix];
        let stmt = Stmt {
            sql: format!(
                "UPDATE {} SET {} = {} WHERE {} = {}",
                db.qualified(&schema, table),
                dl.quote(&col_meta.name),
                dl.param(1, Some(&col_meta.sql_type)),
                dl.quote(&pk.name),
                dl.param(2, Some(&pk.sql_type))
            ),
            params: vec![Some(txt), Some(pk_val)],
        };
        let affected = rt.block_on(d.batch(vec![stmt])).expect("batch update");
        assert!(affected <= 1, "affected {affected}");
        let ddl = rt
            .block_on(d.script(ObjKind::Table, schema.clone(), table.into(), Script::Create))
            .expect("ddl");
        assert!(ddl.to_uppercase().contains("CREATE"), "ddl: {ddl}");
        let select = rt
            .block_on(d.script(ObjKind::Table, schema.clone(), table.into(), Script::Select))
            .expect("select");
        let r = rt
            .block_on(d.query_rows(select, 5))
            .expect("select template runs");
        assert!(!r.is_empty());
    }
}
