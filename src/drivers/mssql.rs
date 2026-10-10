//! Microsoft SQL Server / Azure SQL (tiberius, TDS in pure Rust). One
//! connection behind an async mutex; parameters are `@P1…`, saves run in
//! `BEGIN TRAN … COMMIT`.

use std::sync::Arc;

use serde_json::Value;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, SslMode, Stmt,
    WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

const D: Dialect = Dialect::MsSql;
type Conn = Client<Compat<TcpStream>>;

pub struct MsSql {
    conn: Arc<Mutex<Conn>>,
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let mut cfg = Config::new();
    cfg.host(&host);
    cfg.port(port);
    cfg.authentication(AuthMethod::sql_server(&conn.user, &password));
    if !conn.database.is_empty() {
        cfg.database(&conn.database);
    }
    match conn.ssl {
        SslMode::Disable => cfg.encryption(EncryptionLevel::NotSupported),
        SslMode::Prefer => {
            cfg.encryption(EncryptionLevel::On);
            cfg.trust_cert();
        }
        SslMode::Require => cfg.encryption(EncryptionLevel::Required),
    }
    let client = db::run_db(async move {
        let tcp = TcpStream::connect(cfg.get_addr())
            .await
            .map_err(|e| format!("Can't reach {host}:{port}: {e}"))?;
        tcp.set_nodelay(true).map_err(|e| e.to_string())?;
        Client::connect(cfg, tcp.compat_write()).await.map_err(err)
    })
    .await?;
    Ok(Db::new(MsSql {
        conn: Arc::new(Mutex::new(client)),
    }))
}

fn err(e: tiberius::error::Error) -> String {
    match &e {
        tiberius::error::Error::Server(t) => format!("[{}] {}", t.code(), t.message()),
        _ => e.to_string(),
    }
}

fn cell(data: &ColumnData<'static>) -> Value {
    use ColumnData as C;
    let text = |o: Option<String>| o.map(Value::String).unwrap_or(Value::Null);
    match data {
        C::U8(v) => v.map(Value::from).unwrap_or(Value::Null),
        C::I16(v) => v.map(Value::from).unwrap_or(Value::Null),
        C::I32(v) => v.map(Value::from).unwrap_or(Value::Null),
        C::I64(v) => v.map(Value::from).unwrap_or(Value::Null),
        C::F32(v) => v.map(|f| Value::from(f as f64)).unwrap_or(Value::Null),
        C::F64(v) => v.map(Value::from).unwrap_or(Value::Null),
        C::Bit(v) => v.map(Value::Bool).unwrap_or(Value::Null),
        C::String(v) => text(v.as_ref().map(|s| s.to_string())),
        C::Guid(v) => text(v.map(|g| g.to_string())),
        C::Binary(v) => v.as_ref().map(|b| super::hex(b)).unwrap_or(Value::Null),
        C::Numeric(v) => text(v.map(|n| n.to_string())),
        C::Xml(v) => text(v.as_ref().map(|x| x.to_string())),
        C::Date(_) => text(
            chrono::NaiveDate::from_sql(data)
                .ok()
                .flatten()
                .map(|d| d.to_string()),
        ),
        C::Time(_) => text(
            chrono::NaiveTime::from_sql(data)
                .ok()
                .flatten()
                .map(|d| d.to_string()),
        ),
        C::DateTimeOffset(_) => text(
            chrono::DateTime::<chrono::FixedOffset>::from_sql(data)
                .ok()
                .flatten()
                .map(|d| d.to_rfc3339()),
        ),
        C::DateTime(_) | C::SmallDateTime(_) | C::DateTime2(_) => text(
            chrono::NaiveDateTime::from_sql(data)
                .ok()
                .flatten()
                .map(|d| d.to_string()),
        ),
    }
}

impl MsSql {
    /// Rows of the first result set of `sql` (objects in column order).
    fn rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let conn = self.conn.clone();
        let sql = super::trim_sql(&sql);
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                let mut c = conn.lock().await;
                let results = c
                    .simple_query(sql)
                    .await
                    .map_err(err)?
                    .into_results()
                    .await
                    .map_err(err)?;
                let mut out = Vec::new();
                // The last result set with columns (a batch may start with SET … statements).
                if let Some(rows) = results.into_iter().rev().find(|r| !r.is_empty()) {
                    for row in rows.into_iter().take(limit.max(0) as usize) {
                        let names: Vec<String> =
                            row.columns().iter().map(|c| c.name().to_string()).collect();
                        let vals: Vec<Value> = row.cells().map(|(_, d)| cell(d)).collect();
                        out.extend(super::objects_from(&names, vec![vals]));
                    }
                }
                Ok(out)
            })
            .await
        })
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

impl Driver for MsSql {
    fn engine(&self) -> Engine {
        Engine::MsSql
    }
    fn default_schema(&self) -> Option<String> {
        Some("dbo".into())
    }
    fn version(&self) -> Fut<String> {
        let f = self.rows("SELECT @@VERSION AS v".into(), 1);
        Box::pin(async move {
            Ok(f.await?
                .first()
                .map(|r| s(&r["v"]))
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .to_string())
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        let f = self.rows(
            "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name".into(),
            10_000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["name"])).collect()) })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let f = self.rows(
            "SELECT name FROM sys.schemas
              WHERE name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') AND name NOT LIKE 'db[_]%'
              ORDER BY CASE WHEN name = 'dbo' THEN 0 ELSE 1 END, name"
                .into(),
            10_000,
        );
        Box::pin(async move { Ok(f.await?.iter().map(|r| s(&r["name"])).collect()) })
    }
    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let t = self.rows(
            format!(
                "SELECT TABLE_NAME AS n, TABLE_TYPE AS k FROM INFORMATION_SCHEMA.TABLES
                  WHERE TABLE_SCHEMA = {} ORDER BY TABLE_NAME",
                D.literal(&schema)
            ),
            100_000,
        );
        let f = self.rows(
            format!(
                "SELECT ROUTINE_NAME AS n FROM INFORMATION_SCHEMA.ROUTINES WHERE ROUTINE_SCHEMA = {} ORDER BY 1",
                D.literal(&schema)
            ),
            100_000,
        );
        Box::pin(async move {
            let mut tree = ObjectTree::default();
            for r in t.await? {
                if s(&r["k"]) == "VIEW" {
                    tree.views.push(s(&r["n"]));
                } else {
                    tree.tables.push(s(&r["n"]));
                }
            }
            tree.functions = f
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| s(&r["n"]))
                .collect();
            Ok(tree)
        })
    }

    /// Reserved pages × 8 KB per table (heap + indexes), rowstore figures.
    fn table_sizes(&self, schema: String) -> Fut<Vec<(String, u64)>> {
        let f = self.rows(
            format!(
                "SELECT t.name AS n, COALESCE(SUM(CAST(ps.reserved_page_count AS BIGINT)), 0) * 8192 AS b
                   FROM sys.tables t
                   JOIN sys.schemas sch ON sch.schema_id = t.schema_id
                   LEFT JOIN sys.dm_db_partition_stats ps ON ps.object_id = t.object_id
                  WHERE sch.name = {}
                  GROUP BY t.name ORDER BY t.name",
                D.literal(&schema)
            ),
            100_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| {
                    let b = r["b"].as_f64().map(|v| v.max(0.) as u64).unwrap_or(0);
                    (s(&r["n"]), b)
                })
                .collect())
        })
    }
    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let f = self.rows(
            format!(
                "SELECT c.COLUMN_NAME AS name, c.DATA_TYPE AS dt, c.CHARACTER_MAXIMUM_LENGTH AS len,
                        c.NUMERIC_PRECISION AS prec, c.NUMERIC_SCALE AS scale,
                        c.IS_NULLABLE AS nullable, c.COLUMN_DEFAULT AS dflt,
                        CASE WHEN EXISTS (
                            SELECT 1 FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc
                              JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k
                                ON k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND k.TABLE_SCHEMA = tc.TABLE_SCHEMA
                             WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY' AND tc.TABLE_SCHEMA = c.TABLE_SCHEMA
                               AND tc.TABLE_NAME = c.TABLE_NAME AND k.COLUMN_NAME = c.COLUMN_NAME)
                        THEN 1 ELSE 0 END AS pk
                   FROM INFORMATION_SCHEMA.COLUMNS c
                  WHERE c.TABLE_SCHEMA = {} AND c.TABLE_NAME = {}
                  ORDER BY c.ORDINAL_POSITION",
                D.literal(&schema),
                D.literal(&table)
            ),
            10_000,
        );
        Box::pin(async move {
            Ok(f.await?
                .iter()
                .map(|r| {
                    let dt = s(&r["dt"]);
                    let sql_type = match (r["len"].as_i64(), dt.as_str()) {
                        (Some(-1), _) => format!("{dt}(max)"),
                        (Some(n), _)
                            if !matches!(dt.as_str(), "text" | "ntext" | "image" | "xml") =>
                        {
                            format!("{dt}({n})")
                        }
                        (_, "decimal" | "numeric") => format!("{dt}({},{})", r["prec"], r["scale"]),
                        _ => dt.clone(),
                    };
                    GridColumnMeta {
                        name: s(&r["name"]),
                        pg_type: super::short_type(&dt),
                        sql_type,
                        nullable: s(&r["nullable"]) == "YES",
                        default: r["dflt"].as_str().map(str::to_string),
                        comment: None,
                        is_pk: r["pk"].as_i64() == Some(1),
                        foreign_key: None,
                        enum_values: Vec::new(),
                    }
                })
                .collect())
        })
    }
    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let f = self.rows(
            format!(
                "SELECT COUNT_BIG(*) AS n FROM {} {}",
                D.qualified(&schema, &table, true),
                where_sql(&filter)
            ),
            1,
        );
        Box::pin(async move { Ok(f.await?.first().and_then(|r| r["n"].as_i64()).unwrap_or(0)) })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let sql = format!(
            "SELECT * FROM {} {} {}",
            D.qualified(&req.schema, &req.table, true),
            where_sql(&req.filter),
            D.page(
                req.order_by.as_deref().unwrap_or(""),
                &req.limit.min(i64::from(i32::MAX)).to_string(),
                &req.offset.to_string()
            )
        );
        self.rows(sql, req.limit)
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        self.rows(sql, limit)
    }
    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let conn = self.conn.clone();
        let sql = super::trim_sql(&sql);
        Box::pin(async move {
            db::run_db(async move {
                let mut c = conn.lock().await;
                let mut stream = c.simple_query(sql).await.map_err(err)?;
                let cols = stream.columns().await.map_err(err)?.unwrap_or_default();
                Ok(cols.iter().map(|c| c.name().to_string()).collect())
            })
            .await
        })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let conn = self.conn.clone();
        Box::pin(async move {
            let label = sql.clone();
            db::run_logged(label, crate::console::Source::Data, async move {
                let mut c = conn.lock().await;
                let r = c.execute(sql, &[]).await.map_err(err)?;
                Ok(r.total())
            })
            .await
        })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let conn = self.conn.clone();
        Box::pin(async move {
            db::run_db(async move {
                let mut c = conn.lock().await;
                c.simple_query("BEGIN TRANSACTION")
                    .await
                    .map_err(err)?
                    .into_results()
                    .await
                    .map_err(err)?;
                let mut affected = 0;
                for st in &stmts {
                    let started = std::time::Instant::now();
                    let params: Vec<&dyn tiberius::ToSql> = st
                        .params
                        .iter()
                        .map(|p| p as &dyn tiberius::ToSql)
                        .collect();
                    match c.execute(st.sql.as_str(), &params).await {
                        Ok(r) => {
                            crate::console::record(
                                &st.sql,
                                started,
                                crate::console::Source::Data,
                                None,
                            );
                            affected += r.total();
                        }
                        Err(e) => {
                            let msg = err(e);
                            crate::console::record(
                                &st.sql,
                                started,
                                crate::console::Source::Data,
                                Some(&msg),
                            );
                            let _ = c.simple_query("IF @@TRANCOUNT > 0 ROLLBACK").await;
                            return Err(format!("{msg}\n  in: {}", st.sql));
                        }
                    }
                }
                c.simple_query("COMMIT")
                    .await
                    .map_err(err)?
                    .into_results()
                    .await
                    .map_err(err)?;
                Ok(affected)
            })
            .await
        })
    }
    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let target = D.qualified(&schema, &name, true);
        let def = self.rows(
            format!(
                "SELECT OBJECT_DEFINITION(OBJECT_ID({})) AS d",
                D.literal(&format!("{schema}.{name}"))
            ),
            1,
        );
        let cols = self.columns(schema, name);
        Box::pin(async move {
            match (kind, which) {
                (ObjKind::View | ObjKind::Function, Script::Create) => {
                    Ok(def.await?.first().map(|r| s(&r["d"])).unwrap_or_default())
                }
                (ObjKind::View, Script::Drop) => Ok(format!("DROP VIEW {target};")),
                (ObjKind::Function, Script::Drop) => Ok(format!("DROP PROCEDURE {target};")),
                (ObjKind::Function, _) => Ok(format!("EXEC {target};")),
                (_, Script::Create) => Ok(super::create_table_from(D, &target, &cols.await?)),
                (_, w) => Ok(super::dml_script(D, w, &target, &cols.await?)),
            }
        })
    }
    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let f = self.rows(
            format!(
                "SELECT i.name AS n, i.type_desc AS algo, i.is_unique AS u, i.is_primary_key AS p,
                        STUFF((SELECT ', ' + c.name FROM sys.index_columns ic
                                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                                WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0
                                ORDER BY ic.key_ordinal FOR XML PATH('')), 1, 2, '') AS cols,
                        i.filter_definition AS cond
                   FROM sys.indexes i
                  WHERE i.object_id = OBJECT_ID({}) AND i.name IS NOT NULL
                  ORDER BY i.is_primary_key DESC, i.name",
                D.literal(&format!("{schema}.{table}"))
            ),
            1000,
        );
        Box::pin(async move {
            Ok(f.await
                .unwrap_or_default()
                .iter()
                .map(|r| IndexDef {
                    name: s(&r["n"]),
                    algorithm: s(&r["algo"]).to_lowercase(),
                    unique: r["u"] == Value::Bool(true),
                    primary: r["p"] == Value::Bool(true),
                    columns: s(&r["cols"]),
                    include: String::new(),
                    condition: r["cond"].as_str().map(str::to_string),
                    comment: None,
                    constraint: None,
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn live_sql_server() {
        if !live::reachable(31433) {
            return;
        }
        let c = live::conn(Engine::MsSql, 31433, "sa", "master");
        let rt = crate::db::runtime();
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                "Tusk_pass123".into(),
            ))
            .unwrap();
        rt.block_on(db.driver().exec(
            "IF OBJECT_ID('dbo.people') IS NOT NULL DROP TABLE dbo.people;
             CREATE TABLE dbo.people (id INT PRIMARY KEY, email NVARCHAR(200), born DATE, seen DATETIME2, amount DECIMAL(10,2));
             INSERT INTO dbo.people VALUES (1, 'a@x', '1990-01-02', '2026-01-02T03:04:05', 12.50), (2, 'b@x', NULL, NULL, NULL);"
                .into(),
        ))
        .unwrap();
        live::exercise(c, "Tusk_pass123", "people", "email");
        let rows = rt
            .block_on(
                db.driver()
                    .query_rows("SELECT * FROM dbo.people ORDER BY id".into(), 5),
            )
            .unwrap();
        assert_eq!(rows[0]["born"], "1990-01-02");
        assert_eq!(rows[0]["amount"], "12.50");
    }
}
