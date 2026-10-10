//! Postgres and the engines on its wire protocol (Redshift, CockroachDB,
//! Greenplum, Vertica). Postgres / Greenplum use the full catalog queries;
//! the others read `information_schema`, which they all provide.

use serde_json::Value;

use super::{Db, Driver, Fut, WindowReq};
use crate::db::{
    self, DbResult, EditSource, GridColumnMeta, IndexDef, ObjectTree, Stmt, WhereClause,
};
use crate::engine::Engine;
use crate::objects::{ObjKind, Script};

pub struct Pg {
    pub engine: Engine,
    pub pool: sqlx::PgPool,
}

impl Pg {
    /// Full Postgres catalogs (`pg_class`, `pg_attribute`, …) with the
    /// functions the grid relies on (`row_to_json`, `format_type`).
    fn native(&self) -> bool {
        matches!(self.engine, Engine::Postgres | Engine::Greenplum)
    }
}

pub async fn connect(
    conn: &db::SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let mut opts =
        db::connect_options(&host, port, &conn.database, &conn.user, &password, conn.ssl);
    if matches!(
        conn.engine,
        Engine::Redshift | Engine::Vertica | Engine::Cockroach
    ) {
        // `statement_timeout` / extra startup options aren't accepted by all of them.
        opts =
            db::connect_options_plain(&host, port, &conn.database, &conn.user, &password, conn.ssl);
    }
    let pool = db::connect_pool(opts).await?;
    Ok(Db::new(Pg {
        engine: conn.engine,
        pool,
    }))
}

fn fut<T: Send + 'static>(
    f: impl std::future::Future<Output = DbResult<T>> + Send + 'static,
) -> Fut<T> {
    Box::pin(f)
}

impl Driver for Pg {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn pg(&self) -> Option<&sqlx::PgPool> {
        Some(&self.pool)
    }

    fn row_key(&self) -> bool {
        self.native()
    }

    fn version(&self) -> Fut<String> {
        let pool = self.pool.clone();
        fut(async move {
            db::run_db(async move {
                sqlx::query_scalar::<_, String>("SELECT version()")
                    .fetch_one(&pool)
                    .await
                    .map_err(db::pg_error_message)
            })
            .await
        })
    }

    fn databases(&self) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        if self.native() || self.engine == Engine::Cockroach {
            return fut(async move { db::pg_fetch_databases(&pool).await });
        }
        fut(async move {
            let rows = db::pg_run_query_rows(&pool, "SELECT current_database() AS name", 1).await?;
            Ok(rows
                .iter()
                .filter_map(|r| r["name"].as_str().map(str::to_string))
                .collect())
        })
    }

    fn schemas(&self) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        if self.native() {
            return fut(async move { db::pg_fetch_schemas(&pool).await });
        }
        fut(async move {
            let rows = db::pg_run_query_rows(
                &pool,
                "SELECT schema_name AS name FROM information_schema.schemata
                  WHERE schema_name NOT IN ('information_schema', 'pg_catalog', 'crdb_internal',
                        'pg_extension', 'pg_internal', 'v_catalog', 'v_monitor', 'v_internal')
                    AND schema_name NOT LIKE 'pg_%'
                  ORDER BY CASE WHEN schema_name = 'public' THEN 0 ELSE 1 END, schema_name",
                10_000,
            )
            .await?;
            Ok(rows
                .iter()
                .filter_map(|r| r["name"].as_str().map(str::to_string))
                .collect())
        })
    }

    fn objects(&self, schema: String) -> Fut<ObjectTree> {
        let pool = self.pool.clone();
        if self.native() {
            return fut(async move { db::pg_fetch_objects(&pool, &schema).await });
        }
        fut(async move {
            let sql = format!(
                "SELECT table_name AS name, table_type AS kind FROM information_schema.tables
                  WHERE table_schema = {} ORDER BY table_name",
                db::quote_literal(&schema)
            );
            let rows = db::pg_run_query_rows(&pool, &sql, 100_000).await?;
            let mut tree = ObjectTree::default();
            for r in rows {
                let name = r["name"].as_str().unwrap_or_default().to_string();
                match r["kind"].as_str().unwrap_or_default() {
                    "VIEW" => tree.views.push(name),
                    "MATERIALIZED VIEW" => tree.matviews.push(name),
                    _ => tree.tables.push(name),
                }
            }
            Ok(tree)
        })
    }

    /// Postgres / Greenplum: heap + toast + indexes per table. The
    /// pg-protocol trio (Redshift / Vertica / Cockroach) has no
    /// pg_total_relation_size — the trait default reports none.
    fn table_sizes(&self, schema: String) -> Fut<Vec<(String, u64)>> {
        let pool = self.pool.clone();
        let native = self.native();
        fut(async move {
            if native {
                db::pg_fetch_table_sizes(&pool, &schema).await
            } else {
                Ok(Vec::new())
            }
        })
    }

    fn columns(&self, schema: String, table: String) -> Fut<Vec<GridColumnMeta>> {
        let pool = self.pool.clone();
        if self.native() {
            return fut(async move { db::pg_fetch_columns(&pool, &schema, &table).await });
        }
        fut(async move { info_schema_columns(&pool, &schema, &table).await })
    }

    fn count(&self, schema: String, table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let pool = self.pool.clone();
        fut(async move { db::pg_fetch_count(&pool, &schema, &table, filter.as_ref()).await })
    }

    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        // ctid exists on Postgres / Greenplum only.
        let with_key = req.with_key && self.native();
        let native = self.native();
        fut(async move {
            if native {
                return db::pg_fetch_window(
                    &pool,
                    &req.schema,
                    &req.table,
                    req.filter.as_ref(),
                    req.order_by.as_deref(),
                    with_key,
                    req.limit,
                    req.offset,
                )
                .await;
            }
            // No row_to_json everywhere: read typed columns as text.
            let target = format!(
                "{}.{}",
                db::quote_ident(&req.schema),
                db::quote_ident(&req.table)
            );
            let where_sql = match &req.filter {
                Some(w) if !w.sql.is_empty() => {
                    format!("WHERE {}", crate::filter::inline_params(w))
                }
                _ => String::new(),
            };
            let sql = format!(
                "SELECT * FROM {target} {where_sql} {} LIMIT {} OFFSET {}",
                req.order_by.as_deref().unwrap_or(""),
                req.limit,
                req.offset
            );
            text_rows(&pool, &sql, req.limit).await
        })
    }

    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        let native = self.native();
        fut(async move {
            if native {
                db::pg_run_query_rows(&pool, &sql, limit).await
            } else {
                text_rows(&pool, &sql, limit).await
            }
        })
    }

    /// Postgres / Greenplum: the statement runs in a server-enforced
    /// READ ONLY transaction (SET TRANSACTION READ ONLY), so a write
    /// hiding behind a SELECT-shaped function body is refused by the
    /// server itself. Redshift / Vertica / Cockroach keep the plain path:
    /// their servers don't take the mode, so the classifier stays the gate.
    fn query_rows_readonly(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        let native = self.native();
        fut(async move {
            if native {
                db::pg_run_query_rows_readonly(&pool, &sql, limit).await
            } else {
                text_rows(&pool, &sql, limit).await
            }
        })
    }

    fn query_columns(&self, sql: String) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        fut(async move { db::pg_run_query_columns(&pool, &sql).await })
    }

    fn exec(&self, sql: String) -> Fut<u64> {
        let pool = self.pool.clone();
        fut(async move { db::pg_run_exec(&pool, &sql).await })
    }

    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let pool = self.pool.clone();
        // CockroachDB can't rewrite a column's data inside a transaction:
        // its structure edits run one statement at a time.
        let one_by_one = self.engine == Engine::Cockroach
            && stmts
                .iter()
                .any(|s| s.sql.trim_start().to_uppercase().starts_with("ALTER TABLE"));
        fut(async move {
            if !one_by_one {
                return db::pg_execute_batch(&pool, stmts).await;
            }
            let mut n = 0;
            for st in stmts {
                if !st.params.is_empty() {
                    n += db::pg_execute_batch(&pool, vec![st]).await?;
                    continue;
                }
                let pool = pool.clone();
                let sql = st.sql.clone();
                n += db::run_logged(st.sql, crate::console::Source::Data, async move {
                    Ok(sqlx::raw_sql(&sql)
                        .execute(&pool)
                        .await
                        .map_err(|e| e.to_string())?
                        .rows_affected())
                })
                .await?;
            }
            Ok(n)
        })
    }

    fn script(&self, kind: ObjKind, schema: String, name: String, which: Script) -> Fut<String> {
        let pool = self.pool.clone();
        let native = self.native();
        fut(async move {
            if native {
                return crate::objects::pg_script(&pool, kind, &schema, &name, which).await;
            }
            let cols = info_schema_columns(&pool, &schema, &name).await?;
            let q = format!("{}.{}", db::quote_ident(&schema), db::quote_ident(&name));
            Ok(match which {
                Script::Create => {
                    super::create_table_from(crate::engine::Dialect::Postgres, &q, &cols)
                }
                w => super::dml_script(crate::engine::Dialect::Postgres, w, &q, &cols),
            })
        })
    }

    fn edit_source(
        &self,
        sql: String,
        schema: String,
        result: Vec<String>,
    ) -> Fut<Result<EditSource, String>> {
        let pool = self.pool.clone();
        if !self.native() {
            return super::edit_source::generic(|s, t| self.columns(s, t), &sql, schema, result);
        }
        fut(async move { db::pg_result_edit_source(&pool, &sql).await })
    }

    fn user_types(&self, schema: String) -> Fut<Vec<String>> {
        let pool = self.pool.clone();
        if !self.native() {
            return Box::pin(async { Ok(Vec::new()) });
        }
        fut(async move { db::pg_fetch_user_types(&pool, &schema).await })
    }

    fn triggers(&self, schema: String, table: String) -> Fut<Vec<Value>> {
        let pool = self.pool.clone();
        if !self.native() {
            return Box::pin(async { Ok(Vec::new()) });
        }
        fut(async move { db::pg_fetch_triggers(&pool, &schema, &table).await })
    }

    fn indexes(&self, schema: String, table: String) -> Fut<Vec<IndexDef>> {
        let pool = self.pool.clone();
        match self.engine {
            Engine::Cockroach => fut(async move {
                let sql = format!(
                    "SELECT indexname AS name, indexdef AS def FROM pg_catalog.pg_indexes
                      WHERE schemaname = {} AND tablename = {} ORDER BY indexname",
                    db::quote_literal(&schema),
                    db::quote_literal(&table)
                );
                let rows = text_rows(&pool, &sql, 1_000).await?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        index_from_def(
                            r["name"].as_str().unwrap_or_default(),
                            r["def"].as_str().unwrap_or_default(),
                            &table,
                        )
                    })
                    .collect())
            }),
            _ if self.native() => {
                fut(async move { db::pg_fetch_indexes(&pool, &schema, &table).await })
            }
            // Redshift / Vertica: sort keys and projections, not indexes.
            _ => Box::pin(async { Ok(Vec::new()) }),
        }
    }
}

/// An index from its `CREATE [UNIQUE] INDEX n ON t USING m (a ASC, b DESC)`.
fn index_from_def(name: &str, def: &str, table: &str) -> IndexDef {
    let unique = def.contains("UNIQUE INDEX");
    let after = def.split_once(" USING ").map(|(_, r)| r).unwrap_or(def);
    let algorithm = after
        .split_whitespace()
        .next()
        .unwrap_or("btree")
        .to_lowercase();
    let cols = after
        .split_once('(')
        .and_then(|(_, r)| r.rsplit_once(')'))
        .map(|(c, _)| {
            c.split(',')
                .map(|p| p.trim().trim_end_matches(" ASC").to_string())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let primary = name == format!("{table}_pkey") || name == "primary";
    IndexDef {
        name: name.to_string(),
        algorithm: if algorithm.starts_with('(') {
            "btree".into()
        } else {
            algorithm
        },
        unique: unique || primary,
        primary,
        columns: cols,
        include: String::new(),
        condition: def.split_once(" WHERE ").map(|(_, w)| w.trim().to_string()),
        comment: None,
        constraint: primary.then(|| name.to_string()),
    }
}

/// Column metadata from `information_schema` (Redshift, CockroachDB, Vertica).
async fn info_schema_columns(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> DbResult<Vec<GridColumnMeta>> {
    let sql = format!(
        "SELECT c.column_name AS name, c.data_type AS ty, c.is_nullable AS nullable,
                c.column_default AS dflt,
                EXISTS (SELECT 1 FROM information_schema.table_constraints tc
                          JOIN information_schema.key_column_usage k
                            ON k.constraint_name = tc.constraint_name
                           AND k.table_schema = tc.table_schema AND k.table_name = tc.table_name
                         WHERE tc.constraint_type = 'PRIMARY KEY'
                           AND tc.table_schema = c.table_schema AND tc.table_name = c.table_name
                           AND k.column_name = c.column_name) AS pk
           FROM information_schema.columns c
          WHERE c.table_schema = {} AND c.table_name = {}
          ORDER BY c.ordinal_position",
        db::quote_literal(schema),
        db::quote_literal(table)
    );
    let rows = text_rows(pool, &sql, 10_000).await?;
    // Column comments, where the engine has `col_description`.
    let comments: std::collections::HashMap<String, String> = text_rows(
        pool,
        &format!(
            "SELECT a.attname AS name, col_description(a.attrelid, a.attnum) AS cmt
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid = {}::regclass AND a.attnum > 0",
            db::quote_literal(&format!(
                "{}.{}",
                db::quote_ident(schema),
                db::quote_ident(table)
            ))
        ),
        10_000,
    )
    .await
    .unwrap_or_default()
    .iter()
    .filter_map(|r| {
        Some((
            r["name"].as_str()?.to_string(),
            r["cmt"].as_str()?.to_string(),
        ))
    })
    .collect();
    Ok(rows
        .iter()
        .map(|r| {
            let ty = r["ty"].as_str().unwrap_or_default().to_string();
            let name = r["name"].as_str().unwrap_or_default();
            GridColumnMeta {
                name: r["name"].as_str().unwrap_or_default().to_string(),
                pg_type: crate::drivers::short_type(&ty),
                sql_type: ty,
                nullable: r["nullable"].as_str() != Some("NO"),
                default: r["dflt"].as_str().map(str::to_string),
                comment: comments.get(name).filter(|c| !c.is_empty()).cloned(),
                is_pk: matches!(r["pk"].as_str(), Some("t" | "true" | "1"))
                    || r["pk"] == Value::Bool(true),
                foreign_key: None,
                enum_values: Vec::new(),
            }
        })
        .collect())
}

/// Rows of any statement with every value read as text (engines on the
/// Postgres protocol without `row_to_json`).
async fn text_rows(pool: &sqlx::PgPool, sql: &str, limit: i64) -> DbResult<Vec<Value>> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    let pool = pool.clone();
    let sql = super::trim_sql(sql);
    let label = sql.clone();
    db::run_logged(label, crate::console::Source::Data, async move {
        use futures::TryStreamExt as _;
        let mut out = Vec::new();
        let mut stream = sqlx::raw_sql(&sql).fetch(&pool);
        while let Some(row) = stream.try_next().await.map_err(db::pg_error_message)? {
            let names: Vec<String> = row.columns().iter().map(|c| c.name().to_string()).collect();
            let vals: Vec<Value> = (0..names.len())
                .map(|i| {
                    let ty = row.columns()[i].type_info().name().to_uppercase();
                    pg_value(&row, i, &ty)
                })
                .collect();
            out.extend(super::objects_from(&names, vec![vals]));
            if out.len() as i64 >= limit {
                break;
            }
        }
        Ok(out)
    })
    .await
}

fn pg_value(row: &sqlx::postgres::PgRow, i: usize, ty: &str) -> Value {
    use sqlx::Row as _;
    macro_rules! get {
        ($t:ty) => {
            row.try_get::<Option<$t>, _>(i).ok().flatten()
        };
    }
    match ty {
        "BOOL" => get!(bool).map(Value::Bool),
        "INT2" => get!(i16).map(Value::from),
        "INT4" => get!(i32).map(Value::from),
        "INT8" => get!(i64).map(Value::from),
        "FLOAT4" => get!(f32).map(|v| Value::from(v as f64)),
        "FLOAT8" => get!(f64).map(Value::from),
        "JSON" | "JSONB" => get!(serde_json::Value),
        "BYTEA" => get!(Vec<u8>).map(|b| super::hex(&b)),
        _ => get!(String).map(Value::String).or_else(|| {
            // Types sqlx can't decode as text: show that there is a value.
            row.try_get_raw(i)
                .ok()
                .filter(|r| !sqlx::ValueRef::is_null(r))
                .map(|_| Value::String(format!("({ty})")))
        }),
    }
    .unwrap_or(Value::Null)
}

#[cfg(test)]
mod index_def_tests {
    #[test]
    fn index_definitions_parse() {
        let d = super::index_from_def(
            "t_a_idx",
            "CREATE UNIQUE INDEX t_a_idx ON shop.public.t USING btree (a ASC, b DESC) WHERE a > 0",
            "t",
        );
        assert!(d.unique && !d.primary);
        assert_eq!(
            (d.columns.as_str(), d.algorithm.as_str()),
            ("a, b DESC", "btree")
        );
        assert_eq!(d.condition.as_deref(), Some("a > 0"));
        assert!(
            super::index_from_def(
                "t_pkey",
                "CREATE UNIQUE INDEX t_pkey ON t USING btree (id ASC)",
                "t"
            )
            .primary
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn live_postgres_scratch_and_cockroach() {
        if live::reachable(55432) {
            // tusk_scratch is the table tests may write to (seed data stays untouched).
            live::exercise(
                live::conn(Engine::Postgres, 55432, "tusk", "tusk_dev"),
                "tusk",
                "tusk_scratch",
                "status",
            );
        }
        if live::reachable(26257) {
            live::exercise(
                live::conn(Engine::Cockroach, 26257, "root", "shop"),
                "",
                "customers",
                "email",
            );
        }
    }
}
