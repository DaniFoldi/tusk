//! Structure edits as each engine's DDL: column changes of the Structure
//! view, `CREATE TABLE` for a designed table, index create / drop / rename
//! and object renames. Postgres-family syntax is the default; the others
//! differ in how a column is redefined (MySQL `CHANGE COLUMN`, SQL Server
//! `ALTER COLUMN` + default constraints, Oracle `MODIFY`, ClickHouse
//! `MODIFY COLUMN`, …). Changes an engine can't make in place are errors,
//! shown before anything runs.

use crate::db::IndexDef;
use crate::engine::{Dialect, Engine};
use crate::indexes::IndexRow;
use crate::structure::StructRow;

fn q(e: Engine, name: &str) -> String {
    e.dialect().quote(name)
}

fn lit(e: Engine, s: &str) -> String {
    e.dialect().literal(s)
}

/// Built-in types the Structure view's data_type picker offers (the
/// schema's own types are added from the database).
pub fn type_names(e: Engine) -> &'static [&'static str] {
    match e {
        Engine::MySql | Engine::MariaDb => &[
            "bigint",
            "binary",
            "bit",
            "blob",
            "bool",
            "char",
            "date",
            "datetime",
            "decimal",
            "double",
            "enum",
            "float",
            "int",
            "json",
            "longblob",
            "longtext",
            "mediumint",
            "mediumtext",
            "set",
            "smallint",
            "text",
            "time",
            "timestamp",
            "tinyint",
            "tinytext",
            "varbinary",
            "varchar(255)",
            "year",
        ],
        Engine::MsSql => &[
            "bigint",
            "binary",
            "bit",
            "char",
            "date",
            "datetime",
            "datetime2",
            "datetimeoffset",
            "decimal(18,2)",
            "float",
            "image",
            "int",
            "money",
            "nchar",
            "ntext",
            "numeric",
            "nvarchar(255)",
            "nvarchar(max)",
            "real",
            "smalldatetime",
            "smallint",
            "text",
            "time",
            "tinyint",
            "uniqueidentifier",
            "varbinary(max)",
            "varchar(255)",
            "xml",
        ],
        Engine::Oracle => &[
            "BINARY_DOUBLE",
            "BINARY_FLOAT",
            "BLOB",
            "CHAR",
            "CLOB",
            "DATE",
            "FLOAT",
            "INTERVAL DAY TO SECOND",
            "JSON",
            "NCHAR",
            "NCLOB",
            "NUMBER",
            "NUMBER(10)",
            "NUMBER(19,4)",
            "NVARCHAR2(255)",
            "RAW(16)",
            "TIMESTAMP",
            "TIMESTAMP WITH TIME ZONE",
            "VARCHAR2(255)",
        ],
        Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 => {
            &["BLOB", "INTEGER", "NUMERIC", "REAL", "TEXT"]
        }
        Engine::DuckDb => &[
            "BIGINT",
            "BLOB",
            "BOOLEAN",
            "DATE",
            "DECIMAL(18,3)",
            "DOUBLE",
            "FLOAT",
            "HUGEINT",
            "INTEGER",
            "INTERVAL",
            "JSON",
            "SMALLINT",
            "TIME",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "TINYINT",
            "UBIGINT",
            "UINTEGER",
            "UUID",
            "VARCHAR",
        ],
        Engine::ClickHouse => &[
            "Bool",
            "Date",
            "Date32",
            "DateTime",
            "DateTime64(3)",
            "Decimal(18,4)",
            "Float32",
            "Float64",
            "IPv4",
            "IPv6",
            "Int8",
            "Int16",
            "Int32",
            "Int64",
            "Int128",
            "JSON",
            "LowCardinality(String)",
            "String",
            "UInt8",
            "UInt16",
            "UInt32",
            "UInt64",
            "UUID",
        ],
        Engine::Snowflake => &[
            "ARRAY",
            "BINARY",
            "BOOLEAN",
            "DATE",
            "FLOAT",
            "GEOGRAPHY",
            "NUMBER(38,0)",
            "NUMBER(18,2)",
            "OBJECT",
            "TIME",
            "TIMESTAMP_LTZ",
            "TIMESTAMP_NTZ",
            "TIMESTAMP_TZ",
            "VARCHAR",
            "VARIANT",
        ],
        Engine::BigQuery => &[
            "BIGNUMERIC",
            "BOOL",
            "BYTES",
            "DATE",
            "DATETIME",
            "FLOAT64",
            "GEOGRAPHY",
            "INT64",
            "INTERVAL",
            "JSON",
            "NUMERIC",
            "STRING",
            "TIME",
            "TIMESTAMP",
            "ARRAY<STRING>",
            "ARRAY<INT64>",
        ],
        Engine::Cassandra => &[
            "ascii",
            "bigint",
            "blob",
            "boolean",
            "counter",
            "date",
            "decimal",
            "double",
            "duration",
            "float",
            "inet",
            "int",
            "list<text>",
            "map<text, text>",
            "set<text>",
            "smallint",
            "text",
            "time",
            "timestamp",
            "timeuuid",
            "tinyint",
            "uuid",
            "varint",
        ],
        Engine::Vertica => &[
            "BIGINT",
            "BINARY",
            "BOOLEAN",
            "CHAR",
            "DATE",
            "FLOAT",
            "INTEGER",
            "INTERVAL",
            "LONG VARCHAR",
            "NUMERIC(18,4)",
            "TIME",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "UUID",
            "VARBINARY",
            "VARCHAR(255)",
        ],
        Engine::Redshift => &[
            "BIGINT",
            "BOOLEAN",
            "CHAR",
            "DATE",
            "DECIMAL(18,4)",
            "DOUBLE PRECISION",
            "GEOMETRY",
            "INTEGER",
            "REAL",
            "SMALLINT",
            "SUPER",
            "TIME",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "VARBYTE",
            "VARCHAR(256)",
        ],
        _ => &[
            "bigint",
            "bigserial",
            "bit",
            "bool",
            "boolean",
            "box",
            "bytea",
            "char",
            "cidr",
            "circle",
            "date",
            "decimal",
            "double precision",
            "float4",
            "float8",
            "inet",
            "int2",
            "int4",
            "int8",
            "integer",
            "interval",
            "json",
            "jsonb",
            "line",
            "lseg",
            "macaddr",
            "money",
            "numeric",
            "path",
            "point",
            "polygon",
            "real",
            "serial",
            "smallint",
            "smallserial",
            "text",
            "time",
            "timestamp",
            "timestamptz",
            "timetz",
            "tsquery",
            "tsvector",
            "uuid",
            "varchar",
            "xml",
        ],
    }
}

/// Index methods the Indexes view's algorithm picker offers.
pub fn index_algorithms(e: Engine) -> &'static [&'static str] {
    match e {
        Engine::MySql | Engine::MariaDb => &["BTREE", "HASH", "FULLTEXT", "SPATIAL"],
        Engine::MsSql => &["NONCLUSTERED", "CLUSTERED"],
        Engine::Oracle => &["NORMAL", "BITMAP"],
        Engine::Cockroach => &["BTREE", "GIN"],
        Engine::Sqlite
        | Engine::LibSql
        | Engine::CloudflareD1
        | Engine::DuckDb
        | Engine::Cassandra => &["BTREE"],
        _ => &["BTREE", "HASH", "GIST", "GIN", "BRIN", "SPGIST"],
    }
}

/// A new query tab's first statement: the first rows of `table` in the
/// engine's own language (or a trivial statement without a table).
pub fn starter_query(e: Engine, schema: &str, table: Option<&str>) -> String {
    let Some(t) = table else {
        return match e {
            Engine::MongoDb => "db.getCollectionNames()".into(),
            Engine::Redis | Engine::Valkey => "INFO server".into(),
            Engine::Oracle => "SELECT 1 FROM dual;".into(),
            Engine::Cassandra => "SELECT release_version FROM system.local;".into(),
            Engine::DynamoDb => "SELECT * FROM \"table\"".into(),
            _ => "SELECT 1;".into(),
        };
    };
    let target = target(e, schema, t);
    match e {
        Engine::MongoDb if t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            format!("db.{t}.find({{}}).limit(100)")
        }
        Engine::MongoDb => format!(
            "db.getCollection({}).find({{}}).limit(100)",
            serde_json::json!(t)
        ),
        Engine::Redis | Engine::Valkey => "SCAN 0 MATCH * COUNT 100".into(),
        Engine::DynamoDb => format!("SELECT * FROM {}", q(e, t)),
        Engine::MsSql => format!("SELECT TOP 100 * FROM {target};"),
        Engine::Oracle => format!("SELECT * FROM {target} FETCH FIRST 100 ROWS ONLY;"),
        _ => format!("SELECT * FROM {target} LIMIT 100;"),
    }
}

/// `schema.table` (just the table on schema-less engines).
pub fn target(e: Engine, schema: &str, table: &str) -> String {
    e.dialect().qualified(schema, table, e.caps().schemas)
}

/// A default as the engine reads it back into DDL. MySQL reports string
/// defaults unquoted (`abc`, where MariaDB says `'abc'`).
fn default_sql(e: Engine, d: &str) -> String {
    let t = d.trim();
    if e != Engine::MySql || t.starts_with('\'') || t.starts_with('(') || t.parse::<f64>().is_ok() {
        return t.to_string();
    }
    let u = t.to_uppercase();
    if u == "NULL"
        || u.starts_with("CURRENT_")
        || u.starts_with("NOW(")
        || u.starts_with("B'")
        || u == "TRUE"
        || u == "FALSE"
    {
        return t.to_string();
    }
    lit(e, t)
}

/// A MySQL column definition (`CHANGE COLUMN` restates the whole column).
fn mysql_column(e: Engine, r: &StructRow) -> String {
    let mut s = format!("{} {}", q(e, &r.name), r.sql_type);
    s.push_str(if r.nullable { " NULL" } else { " NOT NULL" });
    match r.default.as_deref() {
        Some("auto_increment") => s.push_str(" AUTO_INCREMENT"),
        Some(d) => s.push_str(&format!(" DEFAULT {}", default_sql(e, d))),
        None => {}
    }
    if let Some(c) = &r.comment {
        s.push_str(&format!(" COMMENT {}", lit(e, c)));
    }
    s
}

/// `col type [DEFAULT d] [NOT NULL]` for ADD COLUMN / CREATE TABLE.
fn column_def(e: Engine, r: &StructRow) -> String {
    match e.dialect() {
        Dialect::MySql => return mysql_column(e, r),
        Dialect::Cql => return format!("{} {}", q(e, &r.name), r.sql_type),
        _ => {}
    }
    let ty = match e {
        // Nullability is part of a ClickHouse type.
        Engine::ClickHouse if r.nullable && !r.sql_type.starts_with("Nullable(") && !r.pk => {
            format!("Nullable({})", r.sql_type)
        }
        _ => r.sql_type.clone(),
    };
    let mut s = format!("{} {ty}", q(e, &r.name));
    if let Some(d) = &r.default {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if !r.nullable && e != Engine::ClickHouse {
        s.push_str(" NOT NULL");
    }
    match (e, &r.comment) {
        (Engine::ClickHouse, Some(c)) => s.push_str(&format!(" COMMENT {}", lit(e, c))),
        (Engine::BigQuery, Some(c)) => s.push_str(&format!(" OPTIONS(description={})", lit(e, c))),
        (Engine::Snowflake, Some(c)) => s.push_str(&format!(" COMMENT {}", lit(e, c))),
        _ => {}
    }
    s
}

/// Whether `COMMENT ON COLUMN t.c IS '…'` sets a column's comment.
fn comment_on(e: Engine) -> bool {
    matches!(
        e,
        Engine::Postgres
            | Engine::Greenplum
            | Engine::Cockroach
            | Engine::Redshift
            | Engine::Oracle
            | Engine::DuckDb
            | Engine::Snowflake
    )
}

fn comment_stmt(
    e: Engine,
    t: &str,
    schema: &str,
    table: &str,
    col: &str,
    comment: &Option<String>,
) -> Result<String, String> {
    let text = comment.as_deref().map(|c| lit(e, c));
    Ok(match e {
        _ if comment_on(e) => format!(
            "COMMENT ON COLUMN {t}.{} IS {}",
            q(e, col),
            text.unwrap_or_else(|| "NULL".into())
        ),
        Engine::ClickHouse => format!(
            "ALTER TABLE {t} COMMENT COLUMN {} {}",
            q(e, col),
            text.unwrap_or_else(|| "''".into())
        ),
        Engine::BigQuery => format!(
            "ALTER TABLE {t} ALTER COLUMN {} SET OPTIONS(description={})",
            q(e, col),
            text.unwrap_or_else(|| "NULL".into())
        ),
        Engine::MsSql => {
            // The description lives in an extended property.
            let (s, tb, c) = (lit(e, schema), lit(e, table), lit(e, col));
            let args = format!(
                "@level0type = N'SCHEMA', @level0name = {s}, @level1type = N'TABLE', @level1name = {tb}, @level2type = N'COLUMN', @level2name = {c}"
            );
            let exists = format!(
                "EXISTS (SELECT 1 FROM sys.extended_properties WHERE major_id = OBJECT_ID({}) AND minor_id = COLUMNPROPERTY(OBJECT_ID({}), {c}, 'ColumnId') AND name = N'MS_Description')",
                lit(e, &format!("{schema}.{table}")),
                lit(e, &format!("{schema}.{table}"))
            );
            match text {
                Some(v) => format!(
                    "IF {exists} EXEC sys.sp_updateextendedproperty @name = N'MS_Description', @value = {v}, {args} ELSE EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = {v}, {args}"
                ),
                None => format!(
                    "IF {exists} EXEC sys.sp_dropextendedproperty @name = N'MS_Description', {args}"
                ),
            }
        }
        _ => return Err(format!("{} columns have no comments", e.label())),
    })
}

/// SQL Server: drop the default constraint of a column (it has a
/// generated name).
fn mssql_drop_default(e: Engine, t: &str, schema: &str, table: &str, col: &str) -> String {
    let obj = lit(e, &format!("{schema}.{table}"));
    format!(
        "DECLARE @df sysname = (SELECT name FROM sys.default_constraints WHERE parent_object_id = OBJECT_ID({obj}) AND parent_column_id = COLUMNPROPERTY(OBJECT_ID({obj}), {}, 'ColumnId')); \
         IF @df IS NOT NULL BEGIN DECLARE @sql nvarchar(max) = N'ALTER TABLE {} DROP CONSTRAINT ' + QUOTENAME(@df); EXEC(@sql) END",
        lit(e, col),
        t.replace('\'', "''")
    )
}

/// Pending Structure-view changes of an existing table as statements:
/// drops, then per-column changes (renames first), then additions.
pub fn alter_columns(
    e: Engine,
    schema: &str,
    table: &str,
    rows: &[StructRow],
) -> Result<Vec<String>, String> {
    let t = target(e, schema, table);
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| r.deleted) {
        let Some(o) = &r.orig else { continue };
        if e == Engine::MsSql {
            out.push(mssql_drop_default(e, &t, schema, table, &o.name));
        }
        out.push(match e.dialect() {
            Dialect::Cql => format!("ALTER TABLE {t} DROP {}", q(e, &o.name)),
            _ => format!("ALTER TABLE {t} DROP COLUMN {}", q(e, &o.name)),
        });
    }
    for r in rows.iter().filter(|r| !r.deleted) {
        let Some(o) = &r.orig else { continue };
        let renamed = o.name != r.name;
        // Type names are case-insensitive (ClickHouse's aren't).
        let same_type = if e == Engine::ClickHouse {
            o.sql_type == r.sql_type
        } else {
            o.sql_type.eq_ignore_ascii_case(&r.sql_type)
        };
        let (retyped, renulled) = (!same_type, o.nullable != r.nullable);
        let (redefaulted, recommented) = (o.default != r.default, o.comment != r.comment);
        if !(renamed || retyped || renulled || redefaulted || recommented) {
            continue;
        }
        let col = q(e, &r.name);
        match e {
            Engine::MySql | Engine::MariaDb => {
                // One statement restates the column.
                out.push(format!(
                    "ALTER TABLE {t} CHANGE COLUMN {} {}",
                    q(e, &o.name),
                    mysql_column(e, r)
                ));
                continue;
            }
            Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1
                if retyped || renulled || redefaulted =>
            {
                return Err(format!(
                    "SQLite can't change the type, nullability or default of {} in place — rename, add or drop columns, or rebuild the table.",
                    r.name
                ));
            }
            Engine::Cassandra if retyped || renulled || redefaulted || recommented => {
                return Err(
                    "Cassandra columns can only be added, dropped or (key columns) renamed.".into(),
                );
            }
            _ => {}
        }
        if renamed {
            out.push(match e {
                Engine::MsSql => format!(
                    "EXEC sp_rename {}, {}, 'COLUMN'",
                    lit(e, &format!("{schema}.{table}.{}", o.name)),
                    lit(e, &r.name)
                ),
                Engine::Cassandra => format!("ALTER TABLE {t} RENAME {} TO {col}", q(e, &o.name)),
                _ => format!("ALTER TABLE {t} RENAME COLUMN {} TO {col}", q(e, &o.name)),
            });
        }
        let ty = &r.sql_type;
        match e {
            Engine::MsSql => {
                if retyped || renulled {
                    let null = if r.nullable { "NULL" } else { "NOT NULL" };
                    out.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {ty} {null}"));
                }
                if redefaulted {
                    out.push(mssql_drop_default(e, &t, schema, table, &r.name));
                    if let Some(d) = &r.default {
                        out.push(format!("ALTER TABLE {t} ADD DEFAULT {d} FOR {col}"));
                    }
                }
            }
            Engine::Oracle => {
                if retyped {
                    out.push(format!("ALTER TABLE {t} MODIFY ({col} {ty})"));
                }
                if renulled {
                    out.push(format!(
                        "ALTER TABLE {t} MODIFY ({col} {})",
                        if r.nullable { "NULL" } else { "NOT NULL" }
                    ));
                }
                if redefaulted {
                    out.push(format!(
                        "ALTER TABLE {t} MODIFY ({col} DEFAULT {})",
                        r.default.as_deref().unwrap_or("NULL")
                    ));
                }
            }
            Engine::ClickHouse => {
                if retyped || renulled {
                    let inner = ty
                        .strip_prefix("Nullable(")
                        .and_then(|x| x.strip_suffix(')'))
                        .unwrap_or(ty);
                    let ty = if r.nullable {
                        format!("Nullable({inner})")
                    } else {
                        inner.to_string()
                    };
                    out.push(format!("ALTER TABLE {t} MODIFY COLUMN {col} {ty}"));
                }
                if redefaulted {
                    out.push(match &r.default {
                        Some(d) => format!("ALTER TABLE {t} MODIFY COLUMN {col} DEFAULT {d}"),
                        None => format!("ALTER TABLE {t} MODIFY COLUMN {col} REMOVE DEFAULT"),
                    });
                }
            }
            Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 | Engine::Cassandra => {}
            Engine::BigQuery | Engine::Snowflake => {
                if retyped {
                    out.push(format!(
                        "ALTER TABLE {t} ALTER COLUMN {col} SET DATA TYPE {ty}"
                    ));
                }
                if renulled {
                    if !r.nullable && e == Engine::BigQuery {
                        return Err("BigQuery can't make a column NOT NULL after the fact.".into());
                    }
                    out.push(format!(
                        "ALTER TABLE {t} ALTER COLUMN {col} {} NOT NULL",
                        if r.nullable { "DROP" } else { "SET" }
                    ));
                }
                if redefaulted {
                    out.push(match (&r.default, e) {
                        (None, _) => format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT"),
                        (Some(_), Engine::Snowflake) => {
                            return Err(
                                "Snowflake can only set a column default when the column is added."
                                    .into(),
                            );
                        }
                        (Some(d), _) => {
                            format!("ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {d}")
                        }
                    });
                }
            }
            // Postgres and its family.
            _ => {
                if retyped {
                    out.push(match e {
                        Engine::Vertica => {
                            format!("ALTER TABLE {t} ALTER COLUMN {col} SET DATA TYPE {ty}")
                        }
                        Engine::Redshift => format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty}"),
                        _ => format!(
                            "ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty} USING {col}::{ty}"
                        ),
                    });
                }
                if renulled {
                    if e == Engine::Redshift {
                        return Err("Redshift can't change a column's nullability.".into());
                    }
                    out.push(format!(
                        "ALTER TABLE {t} ALTER COLUMN {col} {} NOT NULL",
                        if r.nullable { "DROP" } else { "SET" }
                    ));
                }
                if redefaulted {
                    if e == Engine::Redshift {
                        return Err("Redshift can't change a column's default.".into());
                    }
                    out.push(match &r.default {
                        Some(d) => format!("ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {d}"),
                        None => format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT"),
                    });
                }
            }
        }
        if recommented {
            out.push(comment_stmt(e, &t, schema, table, &r.name, &r.comment)?);
        }
    }
    for r in rows.iter().filter(|r| !r.deleted && r.orig.is_none()) {
        out.push(match e {
            Engine::Cassandra => format!("ALTER TABLE {t} ADD {}", column_def(e, r)),
            Engine::MsSql => format!("ALTER TABLE {t} ADD {}", column_def(e, r)),
            Engine::Oracle => format!("ALTER TABLE {t} ADD ({})", column_def(e, r)),
            _ => format!("ALTER TABLE {t} ADD COLUMN {}", column_def(e, r)),
        });
        if r.comment.is_some()
            && !matches!(e.dialect(), Dialect::MySql)
            && !matches!(e, Engine::ClickHouse | Engine::BigQuery | Engine::Snowflake)
        {
            out.push(comment_stmt(e, &t, schema, table, &r.name, &r.comment)?);
        }
    }
    Ok(out)
}

/// `CREATE TABLE` for a designed table (+ comment statements).
pub fn create_table(
    e: Engine,
    schema: &str,
    table: &str,
    rows: &[StructRow],
) -> Result<Vec<String>, String> {
    let t = target(e, schema, table);
    let cols: Vec<&StructRow> = rows.iter().filter(|r| !r.deleted).collect();
    let pk: Vec<String> = cols
        .iter()
        .filter(|r| r.pk)
        .map(|r| q(e, &r.name))
        .collect();
    if e == Engine::Cassandra && pk.is_empty() {
        return Err("A Cassandra table needs a primary key: mark its key columns.".into());
    }
    let mut defs: Vec<String> = cols.iter().map(|r| column_def(e, r)).collect();
    if !pk.is_empty() && e != Engine::ClickHouse {
        let pk = pk.join(", ");
        defs.push(match e {
            Engine::BigQuery => format!("PRIMARY KEY ({pk}) NOT ENFORCED"),
            _ => format!("PRIMARY KEY ({pk})"),
        });
    }
    let mut sql = format!("CREATE TABLE {t} (\n    {}\n)", defs.join(",\n    "));
    if e == Engine::ClickHouse {
        let order = if pk.is_empty() {
            "tuple()".to_string()
        } else {
            format!("({})", pk.join(", "))
        };
        sql.push_str(&format!(" ENGINE = MergeTree ORDER BY {order}"));
    }
    let mut out = vec![sql];
    let inline_comments = matches!(e.dialect(), Dialect::MySql)
        || matches!(e, Engine::ClickHouse | Engine::BigQuery | Engine::Snowflake);
    if !inline_comments {
        for r in cols.iter().filter(|r| r.comment.is_some()) {
            out.push(comment_stmt(e, &t, schema, table, &r.name, &r.comment)?);
        }
    }
    Ok(out)
}

/// `a, lower(b)` → `"a", lower(b)`: plain column names are quoted,
/// expressions and `col DESC` pass through as typed.
pub fn quote_list(e: Engine, list: &str) -> String {
    list.split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| {
            let (name, dir) = match c.rsplit_once(' ') {
                Some((n, d)) if d.eq_ignore_ascii_case("ASC") || d.eq_ignore_ascii_case("DESC") => {
                    (n.trim(), format!(" {d}"))
                }
                _ => (c, String::new()),
            };
            if name.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
                format!("{}{dir}", q(e, name))
            } else {
                c.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `CREATE INDEX` for index row `row` named `name`.
pub fn create_index(
    e: Engine,
    schema: &str,
    table: &str,
    row: &IndexRow,
    name: &str,
) -> Result<String, String> {
    let t = target(e, schema, table);
    let unique = if row.unique { "UNIQUE " } else { "" };
    let cols = quote_list(e, &row.columns);
    let algo = row.algorithm.to_lowercase();
    let mut sql = match e {
        Engine::Postgres | Engine::Greenplum | Engine::Cockroach => {
            let mut s = format!(
                "CREATE {unique}INDEX {} ON {t} USING {algo} ({cols})",
                q(e, name)
            );
            if !row.include.trim().is_empty() {
                s.push_str(&format!(" INCLUDE ({})", quote_list(e, &row.include)));
            }
            s
        }
        Engine::MySql | Engine::MariaDb => {
            let using = match algo.as_str() {
                "btree" | "hash" => format!(" USING {}", algo.to_uppercase()),
                _ => String::new(),
            };
            let kind = match algo.as_str() {
                "fulltext" => "FULLTEXT ",
                "spatial" => "SPATIAL ",
                _ => unique,
            };
            format!("CREATE {kind}INDEX {} ON {t} ({cols}){using}", q(e, name))
        }
        Engine::MsSql => {
            let clustered = if algo == "clustered" {
                "CLUSTERED "
            } else {
                "NONCLUSTERED "
            };
            let mut s = format!(
                "CREATE {unique}{clustered}INDEX {} ON {t} ({cols})",
                q(e, name)
            );
            if !row.include.trim().is_empty() {
                s.push_str(&format!(" INCLUDE ({})", quote_list(e, &row.include)));
            }
            s
        }
        Engine::Oracle => {
            let kind = if algo == "bitmap" { "BITMAP " } else { unique };
            format!(
                "CREATE {kind}INDEX {} ON {t} ({cols})",
                target(e, schema, name)
            )
        }
        Engine::Cassandra => {
            if row.unique {
                return Err("Cassandra secondary indexes can't be unique.".into());
            }
            format!("CREATE INDEX {} ON {t} ({cols})", q(e, name))
        }
        Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 | Engine::DuckDb => {
            format!("CREATE {unique}INDEX {} ON {t} ({cols})", q(e, name))
        }
        _ => return Err(format!("{} has no indexes to create", e.label())),
    };
    if let Some(c) = &row.condition {
        match e {
            Engine::Postgres
            | Engine::Greenplum
            | Engine::Cockroach
            | Engine::MsSql
            | Engine::Sqlite
            | Engine::LibSql
            | Engine::CloudflareD1 => sql.push_str(&format!(" WHERE {c}")),
            _ => return Err(format!("{} indexes can't be partial", e.label())),
        }
    }
    Ok(sql)
}

pub fn drop_index(e: Engine, schema: &str, table: &str, o: &IndexDef) -> String {
    let t = target(e, schema, table);
    if matches!(e, Engine::MySql | Engine::MariaDb) && o.primary {
        return format!("ALTER TABLE {t} DROP PRIMARY KEY");
    }
    if let Some(con) = &o.constraint {
        return format!("ALTER TABLE {t} DROP CONSTRAINT {}", q(e, con));
    }
    match e {
        Engine::MySql | Engine::MariaDb | Engine::MsSql => {
            format!("DROP INDEX {} ON {t}", q(e, &o.name))
        }
        Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 => {
            format!("DROP INDEX {}", q(e, &o.name))
        }
        _ => format!("DROP INDEX {}", target(e, schema, &o.name)),
    }
}

/// Rename an index; `None` when the engine can't (drop + create instead).
pub fn rename_index(e: Engine, schema: &str, table: &str, old: &str, new: &str) -> Option<String> {
    let t = target(e, schema, table);
    Some(match e {
        Engine::Postgres | Engine::Greenplum | Engine::Cockroach => {
            format!(
                "ALTER INDEX {} RENAME TO {}",
                target(e, schema, old),
                q(e, new)
            )
        }
        Engine::MySql | Engine::MariaDb => format!(
            "ALTER TABLE {t} RENAME INDEX {} TO {}",
            q(e, old),
            q(e, new)
        ),
        Engine::MsSql => format!(
            "EXEC sp_rename {}, {}, 'INDEX'",
            lit(e, &format!("{schema}.{table}.{old}")),
            lit(e, new)
        ),
        Engine::Oracle => format!(
            "ALTER INDEX {} RENAME TO {}",
            target(e, schema, old),
            q(e, new)
        ),
        _ => return None,
    })
}

/// `COMMENT ON INDEX`, where the engine has index comments.
pub fn comment_index(
    e: Engine,
    schema: &str,
    name: &str,
    comment: &Option<String>,
) -> Option<String> {
    matches!(e, Engine::Postgres | Engine::Greenplum | Engine::Cockroach).then(|| {
        format!(
            "COMMENT ON INDEX {} IS {}",
            target(e, schema, name),
            comment
                .as_deref()
                .map(|c| lit(e, c))
                .unwrap_or_else(|| "NULL".into())
        )
    })
}

/// Rename a table / view (`keyword`: TABLE, VIEW, MATERIALIZED VIEW, …).
pub fn rename_object(
    e: Engine,
    keyword: &str,
    schema: &str,
    old: &str,
    new: &str,
) -> Result<String, String> {
    let from = target(e, schema, old);
    Ok(match e {
        Engine::MsSql => format!(
            "EXEC sp_rename {}, {}",
            lit(e, &format!("{schema}.{old}")),
            lit(e, new)
        ),
        Engine::MySql | Engine::MariaDb => {
            format!("RENAME TABLE {from} TO {}", target(e, schema, new))
        }
        Engine::ClickHouse => format!("RENAME TABLE {from} TO {}", target(e, schema, new)),
        Engine::Oracle if keyword != "TABLE" => format!("RENAME {} TO {}", q(e, old), q(e, new)),
        Engine::Cassandra => return Err("Cassandra tables can't be renamed.".into()),
        Engine::Snowflake | Engine::BigQuery | Engine::Oracle => {
            format!(
                "ALTER {keyword} {from} RENAME TO {}",
                if e == Engine::Snowflake {
                    target(e, schema, new)
                } else {
                    q(e, new)
                }
            )
        }
        _ => format!("ALTER {keyword} {from} RENAME TO {}", q(e, new)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::GridColumnMeta;

    fn meta(name: &str, ty: &str, nullable: bool, default: Option<&str>) -> GridColumnMeta {
        GridColumnMeta {
            name: name.into(),
            pg_type: String::new(),
            sql_type: ty.into(),
            nullable,
            default: default.map(str::to_string),
            comment: None,
            is_pk: false,
            foreign_key: None,
            enum_values: Vec::new(),
        }
    }

    fn row(m: GridColumnMeta) -> StructRow {
        StructRow {
            name: m.name.clone(),
            sql_type: m.sql_type.clone(),
            nullable: m.nullable,
            default: m.default.clone(),
            comment: m.comment.clone(),
            deleted: false,
            pk: m.is_pk,
            orig: Some(m),
        }
    }

    fn new_col(name: &str, ty: &str) -> StructRow {
        StructRow {
            orig: None,
            name: name.into(),
            sql_type: ty.into(),
            nullable: true,
            default: None,
            comment: None,
            deleted: false,
            pk: false,
        }
    }

    #[test]
    fn mysql_restates_columns() {
        let mut a = row(meta("a", "varchar(10)", true, Some("x")));
        a.name = "a2".into();
        a.nullable = false;
        let id = row(meta("id", "int", false, Some("auto_increment")));
        let mut idc = id.clone();
        idc.comment = Some("key".into());
        let out =
            alter_columns(Engine::MySql, "shop", "t", &[a, idc, new_col("n", "int")]).unwrap();
        assert_eq!(
            out,
            [
                "ALTER TABLE `shop`.`t` CHANGE COLUMN `a` `a2` varchar(10) NOT NULL DEFAULT 'x'",
                "ALTER TABLE `shop`.`t` CHANGE COLUMN `id` `id` int NOT NULL AUTO_INCREMENT COMMENT 'key'",
                "ALTER TABLE `shop`.`t` ADD COLUMN `n` int NULL",
            ]
        );
    }

    #[test]
    fn engines_differ() {
        let mut a = row(meta("a", "int", true, None));
        a.sql_type = "bigint".into();
        a.nullable = false;
        a.default = Some("0".into());
        let ms = alter_columns(Engine::MsSql, "dbo", "t", std::slice::from_ref(&a)).unwrap();
        assert_eq!(
            ms[0],
            "ALTER TABLE [dbo].[t] ALTER COLUMN [a] bigint NOT NULL"
        );
        assert!(ms[1].contains("sys.default_constraints"));
        assert_eq!(ms[2], "ALTER TABLE [dbo].[t] ADD DEFAULT 0 FOR [a]");
        let ora = alter_columns(Engine::Oracle, "S", "T", std::slice::from_ref(&a)).unwrap();
        assert_eq!(
            ora,
            [
                r#"ALTER TABLE "S"."T" MODIFY ("a" bigint)"#,
                r#"ALTER TABLE "S"."T" MODIFY ("a" NOT NULL)"#,
                r#"ALTER TABLE "S"."T" MODIFY ("a" DEFAULT 0)"#
            ]
        );
        assert!(alter_columns(Engine::Sqlite, "main", "t", std::slice::from_ref(&a)).is_err());
        let mut rn = row(meta("a", "text", true, None));
        rn.name = "b".into();
        assert_eq!(
            alter_columns(Engine::Sqlite, "main", "t", &[rn]).unwrap(),
            [r#"ALTER TABLE "t" RENAME COLUMN "a" TO "b""#]
        );
        let ch = alter_columns(Engine::ClickHouse, "shop", "t", std::slice::from_ref(&a)).unwrap();
        assert_eq!(ch[0], "ALTER TABLE `shop`.`t` MODIFY COLUMN `a` bigint");
        let mut pk = new_col("id", "UInt64");
        pk.pk = true;
        pk.nullable = false;
        let create = create_table(
            Engine::ClickHouse,
            "shop",
            "n",
            &[pk, new_col("v", "String")],
        )
        .unwrap();
        assert_eq!(
            create[0],
            "CREATE TABLE `shop`.`n` (\n    `id` UInt64,\n    `v` Nullable(String)\n) ENGINE = MergeTree ORDER BY (`id`)"
        );
    }

    #[test]
    fn index_ddl() {
        let r = IndexRow {
            orig: None,
            name: String::new(),
            algorithm: "BTREE".into(),
            unique: true,
            columns: "email, created_at DESC".into(),
            condition: None,
            include: String::new(),
            comment: None,
            deleted: false,
        };
        assert_eq!(
            create_index(Engine::MySql, "shop", "t", &r, "t_idx").unwrap(),
            "CREATE UNIQUE INDEX `t_idx` ON `shop`.`t` (`email`, `created_at` DESC) USING BTREE"
        );
        assert_eq!(
            create_index(Engine::Sqlite, "main", "t", &r, "t_idx").unwrap(),
            r#"CREATE UNIQUE INDEX "t_idx" ON "t" ("email", "created_at" DESC)"#
        );
        assert_eq!(
            rename_index(Engine::MySql, "shop", "t", "a", "b").unwrap(),
            "ALTER TABLE `shop`.`t` RENAME INDEX `a` TO `b`"
        );
        assert!(rename_index(Engine::Sqlite, "main", "t", "a", "b").is_none());
        assert_eq!(
            rename_object(Engine::MsSql, "TABLE", "dbo", "a", "b").unwrap(),
            "EXEC sp_rename 'dbo.a', 'b'"
        );
    }
}

/// Structure edits end to end on the local containers, on a scratch table
/// of the test's own (`tusk_ddl`, dropped at the end).
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::db::{GridColumnMeta, Stmt};
    use crate::drivers::live;

    fn rows_of(cols: &[GridColumnMeta]) -> Vec<StructRow> {
        cols.iter()
            .map(|m| StructRow {
                orig: Some(m.clone()),
                name: m.name.clone(),
                sql_type: m.sql_type.clone(),
                nullable: m.nullable,
                default: m.default.clone(),
                comment: m.comment.clone(),
                deleted: false,
                pk: m.is_pk,
            })
            .collect()
    }

    fn new_row(name: &str, ty: &str, pk: bool) -> StructRow {
        StructRow {
            orig: None,
            name: name.into(),
            sql_type: ty.into(),
            nullable: !pk,
            default: None,
            comment: None,
            deleted: false,
            pk,
        }
    }

    fn run(db: &crate::drivers::Db, sqls: Vec<String>, what: &str) {
        let stmts: Vec<Stmt> = sqls.iter().cloned().map(Stmt::plain).collect();
        crate::db::runtime()
            .block_on(db.driver().batch(stmts))
            .unwrap_or_else(|e| panic!("{} {what}: {e}\n{sqls:#?}", db.engine().label()));
    }

    #[test]
    fn live_structure_edits() {
        let rt = crate::db::runtime();
        let tmp = std::env::temp_dir().join(format!("tusk_ddl_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        // (engine, port, user, database, password, schema, int, text, wider int, can retype)
        type Case = (
            Engine,
            u16,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            bool,
        );
        let cases: Vec<Case> = vec![
            (
                Engine::MySql,
                33306,
                "root",
                "shop",
                "tusk",
                "shop",
                "int",
                "varchar(100)",
                "bigint",
                true,
            ),
            (
                Engine::MariaDb,
                33307,
                "root",
                "shop",
                "tusk",
                "shop",
                "int",
                "varchar(100)",
                "bigint",
                true,
            ),
            (
                Engine::MsSql,
                31433,
                "sa",
                "master",
                "Tusk_pass123",
                "dbo",
                "int",
                "nvarchar(100)",
                "bigint",
                true,
            ),
            (
                Engine::Oracle,
                31521,
                "tusk",
                "FREEPDB1",
                "tusk",
                "TUSK",
                "NUMBER(10)",
                "VARCHAR2(100)",
                "NUMBER(19)",
                true,
            ),
            (
                Engine::ClickHouse,
                38123,
                "default",
                "shop",
                "tusk",
                "shop",
                "Int32",
                "String",
                "Int64",
                true,
            ),
            (
                Engine::Cockroach,
                26257,
                "root",
                "shop",
                "",
                "public",
                "INT4",
                "STRING",
                "INT8",
                true,
            ),
            (
                Engine::Postgres,
                55432,
                "tusk",
                "tusk_dev",
                "tusk",
                "tusk_scratch",
                "int4",
                "text",
                "int8",
                true,
            ),
            (
                Engine::Sqlite,
                0,
                "",
                "",
                "",
                "main",
                "INTEGER",
                "TEXT",
                "INTEGER",
                false,
            ),
            (
                Engine::DuckDb,
                0,
                "",
                "",
                "",
                "main",
                "INTEGER",
                "VARCHAR",
                "BIGINT",
                true,
            ),
            (
                Engine::Cassandra,
                39042,
                "",
                "tusk_ddl_ks",
                "",
                "tusk_ddl_ks",
                "int",
                "text",
                "int",
                false,
            ),
            // INT is already 64-bit: round 2 only sets NOT NULL and a default.
            (
                Engine::Vertica,
                35433,
                "dbadmin",
                "docker",
                "",
                "public",
                "INT",
                "VARCHAR(100)",
                "INT",
                true,
            ),
        ];
        for (e, port, user, database, pass, schema, int, text, wide, retype) in cases {
            let mut c = live::conn(e, port, user, database);
            if port == 0 {
                c.path = Some(tmp.join(format!("{}.db", e.label())).display().to_string());
            } else if !live::reachable(port) {
                continue;
            }
            let db = rt
                .block_on(crate::drivers::connect(
                    &c,
                    c.host.clone(),
                    c.port,
                    pass.into(),
                ))
                .unwrap();
            let d = db.driver();
            if e == Engine::Cassandra {
                rt.block_on(d.exec(
                    "CREATE KEYSPACE IF NOT EXISTS tusk_ddl_ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}".into(),
                ))
                .unwrap();
            }
            if e == Engine::Postgres {
                rt.block_on(d.exec("CREATE SCHEMA IF NOT EXISTS tusk_scratch".into()))
                    .unwrap();
            }
            for t in ["tusk_ddl", "tusk_ddl2"] {
                let _ = rt.block_on(d.exec(format!("DROP TABLE {}", target(e, schema, t))));
            }
            // Create.
            run(
                &db,
                create_table(
                    e,
                    schema,
                    "tusk_ddl",
                    &[new_row("id", int, true), new_row("name", text, false)],
                )
                .unwrap(),
                "create",
            );
            let cols = rt
                .block_on(d.columns(schema.into(), "tusk_ddl".into()))
                .unwrap();
            assert_eq!(
                cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                ["id", "name"],
                "{}",
                e.label()
            );
            assert!(cols[0].is_pk, "{}: id not pk", e.label());
            // Round 1: rename, comment, add.
            let mut rows = rows_of(&cols);
            if e != Engine::Cassandra {
                rows[1].name = "full_name".into();
            }
            let comments = comment_stmt(e, "", schema, "tusk_ddl", "x", &None).is_ok()
                || e.dialect() == Dialect::MySql;
            if comments {
                rows[1].comment = Some("who's this".into());
            }
            rows.push(new_row("age", int, false));
            run(
                &db,
                alter_columns(e, schema, "tusk_ddl", &rows).unwrap(),
                "alter 1",
            );
            let cols = rt
                .block_on(d.columns(schema.into(), "tusk_ddl".into()))
                .unwrap();
            let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
            assert!(
                names.contains(&"age") && !names.contains(&"name") || e == Engine::Cassandra,
                "{}: {names:?}",
                e.label()
            );
            let text_col = if e == Engine::Cassandra {
                "name"
            } else {
                "full_name"
            };
            if comments {
                let fc = cols.iter().find(|c| c.name == text_col).unwrap();
                // SQL Server / SQLite read no comments back into the grid.
                assert!(
                    fc.comment.as_deref() == Some("who's this") || e == Engine::MsSql,
                    "{}: comment {:?}",
                    e.label(),
                    fc.comment
                );
            }
            // Round 2: widen a type, NOT NULL, a default.
            if retype {
                let mut rows = rows_of(&cols);
                let age = rows.iter_mut().find(|r| r.name == "age").unwrap();
                age.sql_type = wide.into();
                age.default = Some("0".into());
                let fname = rows.iter_mut().find(|r| r.name == text_col).unwrap();
                if e != Engine::ClickHouse {
                    fname.nullable = false;
                }
                run(
                    &db,
                    alter_columns(e, schema, "tusk_ddl", &rows).unwrap(),
                    "alter 2",
                );
                let cols = rt
                    .block_on(d.columns(schema.into(), "tusk_ddl".into()))
                    .unwrap();
                let age = cols.iter().find(|c| c.name == "age").unwrap();
                assert!(
                    age.sql_type
                        .to_lowercase()
                        .contains(&wide.to_lowercase()[..3]),
                    "{}: age type {}",
                    e.label(),
                    age.sql_type
                );
                assert!(
                    age.default.as_deref().is_some_and(|d| d.contains('0')),
                    "{}: age default {:?}",
                    e.label(),
                    age.default
                );
            } else {
                // The engine refuses in-place retyping up front.
                let mut rows = rows_of(&cols);
                rows.iter_mut().find(|r| r.name == "age").unwrap().sql_type = "TEXT".into();
                assert!(
                    alter_columns(e, schema, "tusk_ddl", &rows).is_err(),
                    "{}",
                    e.label()
                );
            }
            // Indexes (where the engine has them).
            if e.caps().indexes && e != Engine::ClickHouse {
                let ix = IndexRow {
                    orig: None,
                    name: "tusk_ddl_age_idx".into(),
                    algorithm: index_algorithms(e)[0].into(),
                    unique: false,
                    columns: "age".into(),
                    condition: None,
                    include: String::new(),
                    comment: None,
                    deleted: false,
                };
                run(
                    &db,
                    vec![create_index(e, schema, "tusk_ddl", &ix, &ix.name).unwrap()],
                    "create index",
                );
                let defs = rt
                    .block_on(d.indexes(schema.into(), "tusk_ddl".into()))
                    .unwrap();
                let def = defs
                    .iter()
                    .find(|d| d.name.eq_ignore_ascii_case("tusk_ddl_age_idx"))
                    .unwrap_or_else(|| panic!("{}: {defs:?}", e.label()))
                    .clone();
                let mut last = def.name.clone();
                if let Some(sql) =
                    rename_index(e, schema, "tusk_ddl", &def.name, "tusk_ddl_age_idx2")
                {
                    run(&db, vec![sql], "rename index");
                    last = "tusk_ddl_age_idx2".into();
                }
                let defs = rt
                    .block_on(d.indexes(schema.into(), "tusk_ddl".into()))
                    .unwrap();
                let def = defs
                    .iter()
                    .find(|d| d.name.eq_ignore_ascii_case(&last))
                    .unwrap_or_else(|| panic!("{}: {defs:?}", e.label()))
                    .clone();
                run(
                    &db,
                    vec![drop_index(e, schema, "tusk_ddl", &def)],
                    "drop index",
                );
                let defs = rt
                    .block_on(d.indexes(schema.into(), "tusk_ddl".into()))
                    .unwrap();
                assert!(
                    !defs.iter().any(|d| d.name.eq_ignore_ascii_case(&last)),
                    "{}: index still there",
                    e.label()
                );
            }
            // Rename, then drop the table.
            let final_name = match rename_object(e, "TABLE", schema, "tusk_ddl", "tusk_ddl2") {
                Ok(sql) => {
                    run(&db, vec![sql], "rename table");
                    "tusk_ddl2"
                }
                Err(_) => "tusk_ddl",
            };
            let tree = rt.block_on(d.objects(schema.into())).unwrap();
            assert!(
                tree.tables.iter().any(|t| t == final_name),
                "{}: {:?}",
                e.label(),
                tree.tables
            );
            run(
                &db,
                vec![format!("DROP TABLE {}", target(e, schema, final_name))],
                "drop",
            );
            if e == Engine::Cassandra {
                rt.block_on(d.exec("DROP KEYSPACE tusk_ddl_ks".into()))
                    .unwrap();
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
