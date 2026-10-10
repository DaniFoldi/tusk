//! MySQL and MariaDB (sqlx's MySQL driver). A MySQL "database" is a schema:
//! the sidebar's schema picker lists them, the connection's database is the
//! one it opens on.

use serde_json::Value;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, GridColumnMeta, IndexDef, ObjectTree, SavedConnection, SslMode, Stmt,
    WhereClause,
};
use crate::engine::{Dialect, Engine};
use crate::objects::{ObjKind, Script};

pub struct MySql {
    engine: Engine,
    database: String,
    pool: sqlx::MySqlPool,
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let mut opts = MySqlConnectOptions::new()
        .host(&host)
        .port(port)
        .username(&conn.user)
        .ssl_mode(match conn.ssl {
            SslMode::Disable => MySqlSslMode::Disabled,
            SslMode::Prefer => MySqlSslMode::Preferred,
            SslMode::Require => MySqlSslMode::Required,
        });
    // An empty password means none (MySQL rejects "using password: YES").
    if !password.is_empty() {
        opts = opts.password(&password);
    }
    if !conn.database.is_empty() {
        opts = opts.database(&conn.database);
    }
    let engine = conn.engine;
    let database = conn.database.clone();
    let pool = db::run_db(async move {
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect_with(opts)
            .await
            .map_err(err)?;
        Ok(pool)
    })
    .await?;
    Ok(Db::new(MySql {
        engine,
        database,
        pool,
    }))
}

fn err(e: sqlx::Error) -> String {
    match e.as_database_error() {
        Some(d) => match d.code() {
            Some(c) => format!("[{c}] {}", d.message()),
            None => d.message().to_string(),
        },
        None => e.to_string(),
    }
}

fn q(s: &str) -> String {
    Dialect::MySql.quote(s)
}

fn lit(s: &str) -> String {
    Dialect::MySql.literal(s)
}

/// A row's value as JSON (text protocol: every value arrives as text).
fn value(row: &sqlx::mysql::MySqlRow, i: usize) -> Value {
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    let ty = row.columns()[i].type_info().name().to_uppercase();
    let raw: Option<Vec<u8>> = row.try_get_unchecked(i).ok().flatten();
    let Some(bytes) = raw else { return Value::Null };
    let text = || String::from_utf8_lossy(&bytes).to_string();
    match ty.as_str() {
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
        | "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" | "YEAR" => {
            let t = text();
            t.parse::<i64>()
                .map(Value::from)
                .or_else(|_| t.parse::<u64>().map(Value::from))
                .unwrap_or(Value::String(t))
        }
        "FLOAT" | "DOUBLE" => text()
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(text())),
        "BOOLEAN" => Value::Bool(text() == "1"),
        "JSON" => serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(text())),
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" | "BIT"
        | "GEOMETRY" => match std::str::from_utf8(&bytes) {
            Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => {
                Value::String(s.to_string())
            }
            _ => super::hex(&bytes),
        },
        _ => Value::String(text()),
    }
}

async fn rows(pool: &sqlx::MySqlPool, sql: &str, limit: i64) -> DbResult<Vec<Value>> {
    use futures::TryStreamExt as _;
    use sqlx::{Column as _, Row as _};
    let pool = pool.clone();
    let sql = super::trim_sql(sql);
    db::run_logged(sql.clone(), crate::console::Source::Data, async move {
        let mut out = Vec::new();
        let mut stream = sqlx::raw_sql(&sql).fetch(&pool);
        while let Some(row) = stream.try_next().await.map_err(err)? {
            let names: Vec<String> = row.columns().iter().map(|c| c.name().to_string()).collect();
            let vals = (0..names.len()).map(|i| value(&row, i)).collect();
            out.extend(super::objects_from(&names, vec![vals]));
            if out.len() as i64 >= limit {
                break;
            }
        }
        Ok(out)
    })
    .await
}

/// The agent path (run_sql, Read-only mode): the statement runs inside a
/// READ ONLY transaction (`SET TRANSACTION READ ONLY`), so the server
/// refuses writes even if one slips past the statement classifier. If
/// the server doesn't take the mode, the query doesn't run.
async fn rows_readonly(pool: &sqlx::MySqlPool, sql: &str, limit: i64) -> DbResult<Vec<Value>> {
    use futures::TryStreamExt as _;
    use sqlx::{Column as _, Row as _};
    let pool = pool.clone();
    let sql = super::trim_sql(sql);
    db::run_logged(sql.clone(), crate::console::Source::Data, async move {
        let mut tx = pool.begin().await.map_err(err)?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(err)?;
        let mut out = Vec::new();
        {
            let mut stream = sqlx::raw_sql(&sql).fetch(&mut *tx);
            while let Some(row) = stream.try_next().await.map_err(err)? {
                let names: Vec<String> =
                    row.columns().iter().map(|c| c.name().to_string()).collect();
                let vals = (0..names.len()).map(|i| value(&row, i)).collect();
                out.extend(super::objects_from(&names, vec![vals]));
                if out.len() as i64 >= limit {
                    break;
                }
            }
        }
        tx.commit().await.map_err(err)?;
        Ok(out)
    })
    .await
}

fn where_sql(filter: &Option<WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

impl Driver for MySql {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn default_schema(&self) -> Option<String> {
        (!self.database.is_empty()).then(|| self.database.clone())
    }

    fn version(&self) -> Fut<String> {
        let pool = self.pool.clone();
        let label = self.engine.label();
        Box::pin(async move {
            let r = rows(&pool, "SELECT VERSION() AS v", 1).await?;
            Ok(format!(
                "{label} {}",
                r.first().and_then(|r| r["v"].as_str()).unwrap_or_default()
            ))
        })
    }

    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }

    fn schemas(&self) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let r = rows(
                &pool,
                "SELECT SCHEMA_NAME AS name FROM information_schema.SCHEMATA
                  WHERE SCHEMA_NAME NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
                  ORDER BY SCHEMA_NAME",
                10_000,
            )
            .await?;
            Ok(r.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_string))
                .collect())
        })
    }

    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let t = rows(
                &pool,
                &format!(
                    "SELECT TABLE_NAME AS name, TABLE_TYPE AS kind FROM information_schema.TABLES
                      WHERE TABLE_SCHEMA = {} ORDER BY TABLE_NAME",
                    lit(&schema)
                ),
                100_000,
            )
            .await?;
            let f = rows(
                &pool,
                &format!(
                    "SELECT ROUTINE_NAME AS name FROM information_schema.ROUTINES
                      WHERE ROUTINE_SCHEMA = {} ORDER BY ROUTINE_NAME",
                    lit(&schema)
                ),
                100_000,
            )
            .await
            .unwrap_or_default();
            let mut tree = ObjectTree::default();
            for r in t {
                let name = r["name"].as_str().unwrap_or_default().to_string();
                if r["kind"].as_str() == Some("VIEW") {
                    tree.views.push(name);
                } else {
                    tree.tables.push(name);
                }
            }
            tree.functions = f
                .iter()
                .filter_map(|r| r["name"].as_str().map(str::to_string))
                .collect();
            Ok(tree)
        })
    }

    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let r = rows(
                &pool,
                &format!(
                    "SELECT c.COLUMN_NAME AS name, c.COLUMN_TYPE AS ty, c.DATA_TYPE AS dt,
                            c.IS_NULLABLE AS nullable, c.COLUMN_DEFAULT AS dflt, c.EXTRA AS extra,
                            c.COLUMN_KEY AS ckey, c.COLUMN_COMMENT AS cmt,
                            (SELECT CONCAT(k.REFERENCED_TABLE_NAME, '(', k.REFERENCED_COLUMN_NAME, ')')
                               FROM information_schema.KEY_COLUMN_USAGE k
                              WHERE k.TABLE_SCHEMA = c.TABLE_SCHEMA AND k.TABLE_NAME = c.TABLE_NAME
                                AND k.COLUMN_NAME = c.COLUMN_NAME AND k.REFERENCED_TABLE_NAME IS NOT NULL
                              LIMIT 1) AS fk
                       FROM information_schema.COLUMNS c
                      WHERE c.TABLE_SCHEMA = {} AND c.TABLE_NAME = {}
                      ORDER BY c.ORDINAL_POSITION",
                    lit(&schema),
                    lit(&table)
                ),
                10_000,
            )
            .await?;
            Ok(r.iter()
                .map(|r| {
                    let ty = r["ty"].as_str().unwrap_or_default().to_string();
                    let dt = r["dt"].as_str().unwrap_or_default().to_lowercase();
                    let enum_values = if dt == "enum" || dt == "set" {
                        parse_enum(&ty)
                    } else {
                        Vec::new()
                    };
                    let extra = r["extra"].as_str().unwrap_or_default();
                    // MariaDB says `NULL` for "no default".
                    let default = r["dflt"]
                        .as_str()
                        .filter(|d| !d.eq_ignore_ascii_case("NULL"))
                        .map(str::to_string)
                        .or_else(|| {
                            extra
                                .contains("auto_increment")
                                .then(|| "auto_increment".to_string())
                        });
                    GridColumnMeta {
                        name: r["name"].as_str().unwrap_or_default().to_string(),
                        pg_type: if ty == "tinyint(1)" {
                            "bool".into()
                        } else {
                            super::short_type(&dt)
                        },
                        sql_type: ty,
                        nullable: r["nullable"].as_str() == Some("YES"),
                        default,
                        comment: r["cmt"]
                            .as_str()
                            .filter(|c| !c.is_empty())
                            .map(str::to_string),
                        is_pk: r["ckey"].as_str() == Some("PRI"),
                        foreign_key: r["fk"].as_str().map(str::to_string),
                        enum_values,
                    }
                })
                .collect())
        })
    }

    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let sql = format!(
                "SELECT COUNT(*) AS n FROM {}.{} {}",
                q(&schema),
                q(&table),
                where_sql(&filter)
            );
            let r = rows(&pool, &sql, 1).await?;
            Ok(r.first().and_then(|r| r["n"].as_i64()).unwrap_or(0))
        })
    }

    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let sql = format!(
                "SELECT * FROM {}.{} {} {} LIMIT {} OFFSET {}",
                q(&req.schema),
                q(&req.table),
                where_sql(&req.filter),
                req.order_by.unwrap_or_default(),
                req.limit.min(i64::from(i32::MAX)),
                req.offset
            );
            rows(&pool, &sql, req.limit).await
        })
    }

    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        Box::pin(async move { rows(&pool, &sql, limit).await })
    }

    /// MySQL / MariaDB: server-enforced READ ONLY transaction.
    fn query_rows_readonly(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        Box::pin(async move { rows_readonly(&pool, &sql, limit).await })
    }

    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            use sqlx::{Column as _, Executor as _};
            let sql = super::trim_sql(&sql);
            db::run_db(async move {
                let d = pool.describe(&sql).await.map_err(err)?;
                Ok(d.columns().iter().map(|c| c.name().to_string()).collect())
            })
            .await
        })
    }

    fn exec(&self, sql: String) -> Fut<u64> {
        let pool = self.pool.clone();
        Box::pin(async move {
            db::run_logged(sql.clone(), crate::console::Source::Data, async move {
                let done = sqlx::raw_sql(&sql).execute(&pool).await.map_err(err)?;
                Ok(done.rows_affected())
            })
            .await
        })
    }

    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let pool = self.pool.clone();
        Box::pin(async move {
            db::run_db(async move {
                let mut tx = pool.begin().await.map_err(err)?;
                let mut affected = 0;
                for stmt in &stmts {
                    let started = std::time::Instant::now();
                    let done = run_stmt(&mut tx, stmt).await.map_err(|e| {
                        let msg = err(e);
                        crate::console::record(
                            &stmt.sql,
                            started,
                            crate::console::Source::Data,
                            Some(&msg),
                        );
                        format!("{msg}\n  in: {}", stmt.sql)
                    })?;
                    crate::console::record(&stmt.sql, started, crate::console::Source::Data, None);
                    affected += done;
                }
                tx.commit().await.map_err(err)?;
                Ok(affected)
            })
            .await
        })
    }

    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let pool = self.pool.clone();
        let this_cols = self.columns(schema.clone(), name.clone());
        Box::pin(async move {
            let target = format!("{}.{}", q(&schema), q(&name));
            match (kind, which) {
                (ObjKind::Function, Script::Create) => {
                    let r = rows(&pool, &format!("SHOW CREATE FUNCTION {target}"), 1).await;
                    let r = match r {
                        Ok(r) if !r.is_empty() => r,
                        _ => rows(&pool, &format!("SHOW CREATE PROCEDURE {target}"), 1).await?,
                    };
                    Ok(second_text(&r))
                }
                (ObjKind::Function, Script::Drop) => Ok(format!("DROP FUNCTION {target};")),
                (ObjKind::Function, _) => Ok(format!("SELECT {target}();")),
                (ObjKind::View, Script::Create) => Ok(format!(
                    "{};",
                    second_text(&rows(&pool, &format!("SHOW CREATE VIEW {target}"), 1).await?)
                )),
                (ObjKind::View, Script::Drop) => Ok(format!("DROP VIEW {target};")),
                (_, Script::Create) => Ok(format!(
                    "{};",
                    second_text(&rows(&pool, &format!("SHOW CREATE TABLE {target}"), 1).await?)
                )),
                (_, w) => Ok(super::dml_script(
                    Dialect::MySql,
                    w,
                    &target,
                    &this_cols.await?,
                )),
            }
        })
    }

    fn triggers(&self, schema: String, table: String) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            rows(
                &pool,
                &format!(
                    "SELECT TRIGGER_NAME AS trigger_name, ACTION_TIMING AS timing,
                            EVENT_MANIPULATION AS event, ACTION_ORIENTATION AS level,
                            ACTION_STATEMENT AS definition
                       FROM information_schema.TRIGGERS
                      WHERE EVENT_OBJECT_SCHEMA = {} AND EVENT_OBJECT_TABLE = {}
                      ORDER BY TRIGGER_NAME",
                    lit(&schema),
                    lit(&table)
                ),
                10_000,
            )
            .await
        })
    }

    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            // Functional key parts (EXPRESSION) exist in MySQL 8 only.
            let sql = |part: &str| {
                format!(
                    "SELECT INDEX_NAME AS name, INDEX_TYPE AS algo, NON_UNIQUE AS non_unique,
                            GROUP_CONCAT({part} ORDER BY SEQ_IN_INDEX SEPARATOR ', ') AS cols,
                            MAX(INDEX_COMMENT) AS cmt
                       FROM information_schema.STATISTICS
                      WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}
                      GROUP BY INDEX_NAME, INDEX_TYPE, NON_UNIQUE
                      ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME",
                    lit(&schema),
                    lit(&table)
                )
            };
            let r = match rows(&pool, &sql("COALESCE(COLUMN_NAME, EXPRESSION)"), 10_000).await {
                Ok(r) => r,
                Err(_) => rows(&pool, &sql("COLUMN_NAME"), 10_000).await?,
            };
            Ok(r.iter()
                .map(|r| {
                    let name = r["name"].as_str().unwrap_or_default().to_string();
                    let primary = name == "PRIMARY";
                    IndexDef {
                        algorithm: r["algo"].as_str().unwrap_or("BTREE").to_lowercase(),
                        unique: r["non_unique"].as_i64() == Some(0)
                            || r["non_unique"].as_str() == Some("0"),
                        primary,
                        columns: r["cols"].as_str().unwrap_or_default().to_string(),
                        include: String::new(),
                        condition: None,
                        comment: r["cmt"]
                            .as_str()
                            .filter(|c| !c.is_empty())
                            .map(str::to_string),
                        constraint: primary.then(|| "PRIMARY".to_string()),
                        name,
                    }
                })
                .collect())
        })
    }
}

async fn run_stmt(conn: &mut sqlx::MySqlConnection, stmt: &Stmt) -> Result<u64, sqlx::Error> {
    use sqlx::Executor as _;
    if stmt.params.is_empty() {
        return Ok(conn
            .execute(sqlx::raw_sql(&stmt.sql))
            .await?
            .rows_affected());
    }
    let mut qy = sqlx::query(&stmt.sql);
    for p in &stmt.params {
        qy = qy.bind(p.clone());
    }
    Ok(conn.execute(qy).await?.rows_affected())
}

/// `SHOW CREATE …` puts the statement in its second column.
fn second_text(rows: &[Value]) -> String {
    rows.first()
        .and_then(Value::as_object)
        .and_then(|o| o.values().nth(1))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `enum('a','b','it''s')` → `["a", "b", "it's"]`.
fn parse_enum(ty: &str) -> Vec<String> {
    let Some(inner) = ty
        .find('(')
        .and_then(|i| ty.rfind(')').map(|j| &ty[i + 1..j]))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('\'', false) => quoted = true,
            ('\'', true) if chars.peek() == Some(&'\'') => {
                cur.push('\'');
                chars.next();
            }
            ('\'', true) => {
                quoted = false;
                out.push(std::mem::take(&mut cur));
            }
            (c, true) => cur.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn live_mysql_and_mariadb() {
        for (engine, port) in [(Engine::MySql, 33306), (Engine::MariaDb, 33307)] {
            if live::reachable(port) {
                live::exercise(
                    live::conn(engine, port, "root", "shop"),
                    "tusk",
                    "customers",
                    "email",
                );
            }
        }
    }

    #[test]
    fn enum_values() {
        assert_eq!(
            super::parse_enum("enum('a','b','it''s')"),
            ["a", "b", "it's"]
        );
        assert_eq!(super::parse_enum("int(11)"), Vec::<String>::new());
    }
}
