//! Database engines Tusk connects to: what each one is called, how its
//! connection form looks, which SQL dialect it speaks and which workspace
//! features it supports.

use serde::{Deserialize, Serialize};

/// Every engine the connection form offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Postgres,
    Redshift,
    Cockroach,
    Greenplum,
    Vertica,
    MySql,
    MariaDb,
    Sqlite,
    DuckDb,
    LibSql,
    CloudflareD1,
    MsSql,
    Oracle,
    ClickHouse,
    Snowflake,
    BigQuery,
    Redis,
    Valkey,
    MongoDb,
    Cassandra,
    DynamoDb,
}

/// Which fields the connection form shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Host, port, user, password, database (+ SSL, SSH).
    Server,
    /// A database file on disk.
    File,
    /// A server URL and an auth token.
    UrlToken,
    /// Account id, database id, API token.
    CloudflareD1,
    /// Account, user, token, warehouse, database, schema, role.
    Snowflake,
    /// Project, dataset, service-account key file (or an emulator URL).
    BigQuery,
    /// Region, access key, secret key, optional endpoint.
    DynamoDb,
}

/// SQL flavour: quoting, parameters, text casts, pagination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
    MsSql,
    Oracle,
    ClickHouse,
    DuckDb,
    Snowflake,
    BigQuery,
    /// Cassandra CQL (no joins, no OFFSET).
    Cql,
    /// Postgres-like SQL without Postgres types (`::text`) or catalogs.
    Vertica,
    /// Key-value / document stores: no SQL of their own for the grid.
    NoSql,
}

/// What the workspace can offer for a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// More than one schema / namespace to switch between.
    pub schemas: bool,
    /// Views in the sidebar.
    pub views: bool,
    pub matviews: bool,
    pub functions: bool,
    /// Rows can be edited, inserted and deleted from the grid.
    pub edit_rows: bool,
    /// Structure view can alter columns (Postgres-style DDL).
    pub edit_structure: bool,
    pub indexes: bool,
    pub triggers: bool,
    /// Roles / users management.
    pub roles: bool,
    /// Running queries list (cancel / kill).
    pub processes: bool,
    /// pg_dump / psql backup & restore.
    pub backup: bool,
    /// The bundled SQL language server (Postgres grammar).
    pub lsp: bool,
    /// Several statements can run in one transaction (save batch).
    pub transactions: bool,
    /// Queries typed in the editor are SQL (false: a command language).
    pub sql: bool,
}

const FULL: Caps = Caps {
    schemas: true,
    views: true,
    matviews: true,
    functions: true,
    edit_rows: true,
    edit_structure: true,
    indexes: true,
    triggers: true,
    roles: true,
    processes: true,
    backup: true,
    lsp: true,
    transactions: true,
    sql: true,
};

const SQL_BASIC: Caps = Caps {
    schemas: true,
    views: true,
    matviews: false,
    functions: false,
    edit_rows: true,
    edit_structure: false,
    indexes: true,
    triggers: false,
    roles: false,
    processes: false,
    backup: false,
    lsp: false,
    transactions: true,
    sql: true,
};

impl Engine {
    pub const ALL: [Engine; 21] = [
        Engine::Postgres,
        Engine::MySql,
        Engine::MariaDb,
        Engine::Sqlite,
        Engine::MsSql,
        Engine::Redis,
        Engine::Valkey,
        Engine::Cassandra,
        Engine::MongoDb,
        Engine::Oracle,
        Engine::Redshift,
        Engine::Cockroach,
        Engine::Vertica,
        Engine::Snowflake,
        Engine::Greenplum,
        Engine::BigQuery,
        Engine::DuckDb,
        Engine::ClickHouse,
        Engine::DynamoDb,
        Engine::LibSql,
        Engine::CloudflareD1,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::Redshift => "Amazon Redshift",
            Engine::Cockroach => "CockroachDB",
            Engine::Greenplum => "Greenplum",
            Engine::Vertica => "Vertica",
            Engine::MySql => "MySQL",
            Engine::MariaDb => "MariaDB",
            Engine::Sqlite => "SQLite",
            Engine::DuckDb => "DuckDB",
            Engine::LibSql => "LibSQL",
            Engine::CloudflareD1 => "Cloudflare D1",
            Engine::MsSql => "Microsoft SQL Server",
            Engine::Oracle => "Oracle",
            Engine::ClickHouse => "ClickHouse",
            Engine::Snowflake => "Snowflake",
            Engine::BigQuery => "BigQuery",
            Engine::Redis => "Redis",
            Engine::Valkey => "Valkey",
            Engine::MongoDb => "MongoDB",
            Engine::Cassandra => "Cassandra",
            Engine::DynamoDb => "DynamoDB",
        }
    }

    /// Short badge text for the engine picker (two letters, like a logo).
    pub fn abbr(self) -> &'static str {
        match self {
            Engine::Postgres => "Pg",
            Engine::Redshift => "Rs",
            Engine::Cockroach => "Cr",
            Engine::Greenplum => "Gp",
            Engine::Vertica => "Ve",
            Engine::MySql => "My",
            Engine::MariaDb => "Ma",
            Engine::Sqlite => "Sl",
            Engine::DuckDb => "Dk",
            Engine::LibSql => "Ls",
            Engine::CloudflareD1 => "D1",
            Engine::MsSql => "Ss",
            Engine::Oracle => "Or",
            Engine::ClickHouse => "Ch",
            Engine::Snowflake => "Sf",
            Engine::BigQuery => "Bq",
            Engine::Redis => "Re",
            Engine::Valkey => "Vk",
            Engine::MongoDb => "Mg",
            Engine::Cassandra => "Ca",
            Engine::DynamoDb => "Dy",
        }
    }

    /// Brand-ish badge color for the picker and connection list.
    pub fn color(self) -> u32 {
        match self {
            Engine::Postgres | Engine::Greenplum => 0x336791,
            Engine::Redshift | Engine::DynamoDb => 0x4B6BFB,
            Engine::Cockroach => 0x6933FF,
            Engine::Vertica => 0x1F5FAF,
            Engine::MySql => 0x00758F,
            Engine::MariaDb => 0xC0765A,
            Engine::Sqlite => 0x44A8E0,
            Engine::DuckDb => 0xD4A017,
            Engine::LibSql => 0x4FF8D2,
            Engine::CloudflareD1 => 0xF38020,
            Engine::MsSql => 0xCC2927,
            Engine::Oracle => 0xC74634,
            Engine::ClickHouse => 0xE8C84A,
            Engine::Snowflake => 0x29B5E8,
            Engine::BigQuery => 0x669DF6,
            Engine::Redis => 0xDC382D,
            Engine::Valkey => 0x0A7EF2,
            Engine::MongoDb => 0x47A248,
            Engine::Cassandra => 0x1287B1,
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres | Engine::Greenplum => 5432,
            Engine::Redshift => 5439,
            Engine::Cockroach => 26257,
            Engine::Vertica => 5433,
            Engine::MySql | Engine::MariaDb => 3306,
            Engine::MsSql => 1433,
            Engine::Oracle => 1521,
            Engine::ClickHouse => 8123,
            Engine::Redis | Engine::Valkey => 6379,
            Engine::MongoDb => 27017,
            Engine::Cassandra => 9042,
            _ => 0,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            Engine::Postgres | Engine::Greenplum => "postgres",
            Engine::Redshift => "awsuser",
            Engine::Cockroach => "root",
            Engine::Vertica => "dbadmin",
            Engine::MySql | Engine::MariaDb => "root",
            Engine::MsSql => "sa",
            Engine::Oracle => "system",
            Engine::ClickHouse => "default",
            Engine::Cassandra => "cassandra",
            _ => "",
        }
    }

    /// The database a profile lands in when its database field is blank
    /// ("" = stay server-wide / pick after connecting). ClickHouse
    /// substitutes "default" in its driver, so it stays "".
    pub fn default_database(self) -> &'static str {
        match self {
            Engine::Postgres | Engine::Greenplum => "postgres",
            Engine::Cockroach => "defaultdb",
            Engine::MsSql => "master",
            // Redis / Valkey: the server's unnamed logical databases are
            // indexes; blank connects to db 0.
            Engine::Redis | Engine::Valkey => "0",
            _ => "",
        }
    }

    /// Placeholder for the database field (what it means per engine).
    pub fn database_hint(self) -> &'static str {
        match self {
            Engine::Oracle => "Service name (FREEPDB1)",
            Engine::Cassandra => "Keyspace (optional)",
            Engine::Redis | Engine::Valkey => "Database index (0)",
            Engine::MongoDb => "Database (optional)",
            Engine::ClickHouse => "default",
            Engine::MsSql => "master",
            Engine::Cockroach => "defaultdb",
            Engine::Postgres | Engine::Greenplum => "mydb (optional — defaults to postgres)",
            Engine::MySql | Engine::MariaDb => "mydb (optional)",
            Engine::CloudflareD1 => "Database (optional — pick after connecting)",
            _ => "mydb",
        }
    }

    pub fn form(self) -> Form {
        match self {
            Engine::Sqlite | Engine::DuckDb => Form::File,
            Engine::LibSql => Form::UrlToken,
            Engine::CloudflareD1 => Form::CloudflareD1,
            Engine::Snowflake => Form::Snowflake,
            Engine::BigQuery => Form::BigQuery,
            Engine::DynamoDb => Form::DynamoDb,
            _ => Form::Server,
        }
    }

    /// Engines that speak the Postgres wire protocol (one driver).
    pub fn pg_wire(self) -> bool {
        matches!(
            self,
            Engine::Postgres
                | Engine::Redshift
                | Engine::Cockroach
                | Engine::Greenplum
                | Engine::Vertica
        )
    }

    pub fn dialect(self) -> Dialect {
        match self {
            Engine::Postgres | Engine::Redshift | Engine::Cockroach | Engine::Greenplum => {
                Dialect::Postgres
            }
            Engine::Vertica => Dialect::Vertica,
            Engine::MySql | Engine::MariaDb => Dialect::MySql,
            Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 => Dialect::Sqlite,
            Engine::DuckDb => Dialect::DuckDb,
            Engine::MsSql => Dialect::MsSql,
            Engine::Oracle => Dialect::Oracle,
            Engine::ClickHouse => Dialect::ClickHouse,
            Engine::Snowflake => Dialect::Snowflake,
            Engine::BigQuery => Dialect::BigQuery,
            Engine::Cassandra => Dialect::Cql,
            Engine::Redis | Engine::Valkey | Engine::MongoDb | Engine::DynamoDb => Dialect::NoSql,
        }
    }

    /// A database *is* the schema level (MySQL databases, Redis indexes,
    /// Mongo databases, keyspaces …): the title bar shows one picker.
    /// D1 is *not* here — an account holds many named databases and the
    /// title bar gets a real database picker (its only schema is "main").
    pub fn databases_are_schemas(self) -> bool {
        matches!(
            self,
            Engine::MySql
                | Engine::MariaDb
                | Engine::Redis
                | Engine::Valkey
                | Engine::MongoDb
                | Engine::ClickHouse
                | Engine::Cassandra
                | Engine::Sqlite
                | Engine::LibSql
                | Engine::DynamoDb
        )
    }

    pub fn caps(self) -> Caps {
        let caps = match self {
            Engine::Postgres | Engine::Greenplum => FULL,
            // Postgres-protocol warehouses: the catalogs differ, so no
            // Postgres-style structure editing, backups or roles UI.
            Engine::Redshift | Engine::Vertica => Caps {
                matviews: self == Engine::Redshift,
                // Sort keys / projections, not indexes.
                indexes: false,
                ..SQL_BASIC
            },
            Engine::Cockroach => Caps {
                functions: true,
                ..SQL_BASIC
            },
            Engine::MySql | Engine::MariaDb => Caps {
                functions: true,
                triggers: true,
                ..SQL_BASIC
            },
            Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 => Caps {
                schemas: false,
                triggers: true,
                transactions: self == Engine::Sqlite,
                ..SQL_BASIC
            },
            Engine::DuckDb => Caps {
                functions: false,
                ..SQL_BASIC
            },
            Engine::MsSql => Caps {
                functions: true,
                triggers: true,
                ..SQL_BASIC
            },
            Engine::Oracle => Caps {
                functions: true,
                triggers: true,
                ..SQL_BASIC
            },
            Engine::ClickHouse => Caps {
                transactions: false,
                indexes: false,
                matviews: true,
                ..SQL_BASIC
            },
            Engine::Snowflake | Engine::BigQuery => Caps {
                transactions: false,
                indexes: false,
                ..SQL_BASIC
            },
            Engine::Cassandra => Caps {
                views: true,
                indexes: true,
                transactions: false,
                ..SQL_BASIC
            },
            Engine::Redis | Engine::Valkey => Caps {
                views: false,
                indexes: false,
                transactions: false,
                sql: false,
                ..SQL_BASIC
            },
            Engine::MongoDb => Caps {
                views: false,
                indexes: true,
                transactions: false,
                sql: false,
                ..SQL_BASIC
            },
            Engine::DynamoDb => Caps {
                schemas: false,
                views: false,
                indexes: true,
                transactions: false,
                ..SQL_BASIC
            },
        };
        // Engines whose drivers list server sessions (drivers::sessions).
        let processes = crate::drivers::sessions::list_sql(self).is_some()
            || matches!(self, Engine::Redis | Engine::Valkey | Engine::MongoDb);
        // Structure / index edits and New Table as each engine's DDL (crate::ddl).
        let edit_structure = !matches!(
            self,
            Engine::Redis | Engine::Valkey | Engine::MongoDb | Engine::DynamoDb
        );
        Caps {
            processes,
            edit_structure,
            ..caps
        }
    }

    /// Type of New Table's starting `id` primary key: the engine's
    /// auto-numbering integer where a type alone gives one.
    pub fn new_table_id_type(self) -> &'static str {
        match self {
            Engine::Postgres | Engine::Greenplum | Engine::Cockroach => "serial",
            // `INTEGER PRIMARY KEY` is SQLite's rowid alias (auto-numbered).
            Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 => "INTEGER",
            Engine::DuckDb | Engine::Redshift | Engine::Snowflake | Engine::Vertica => "INTEGER",
            Engine::BigQuery => "INT64",
            Engine::ClickHouse => "UInt64",
            Engine::Oracle => "NUMBER",
            _ => "int",
        }
    }

    /// URL scheme for "Copy as URL".
    pub fn scheme(self) -> &'static str {
        match self {
            Engine::Postgres | Engine::Greenplum | Engine::Cockroach | Engine::Vertica => {
                "postgresql"
            }
            Engine::Redshift => "redshift",
            Engine::MySql => "mysql",
            Engine::MariaDb => "mariadb",
            Engine::Sqlite => "sqlite",
            Engine::DuckDb => "duckdb",
            Engine::LibSql => "libsql",
            Engine::CloudflareD1 => "d1",
            Engine::MsSql => "sqlserver",
            Engine::Oracle => "oracle",
            Engine::ClickHouse => "clickhouse",
            Engine::Snowflake => "snowflake",
            Engine::BigQuery => "bigquery",
            Engine::Redis => "redis",
            Engine::Valkey => "valkey",
            Engine::MongoDb => "mongodb",
            Engine::Cassandra => "cassandra",
            Engine::DynamoDb => "dynamodb",
        }
    }
}

impl Dialect {
    /// Quote an identifier.
    pub fn quote(self, name: &str) -> String {
        match self {
            Dialect::MySql | Dialect::ClickHouse | Dialect::BigQuery => {
                format!("`{}`", name.replace('`', "``"))
            }
            Dialect::MsSql => format!("[{}]", name.replace(']', "]]")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }

    /// `schema.table` (engines without schemas: just the table).
    pub fn qualified(self, schema: &str, table: &str, has_schemas: bool) -> String {
        if !has_schemas || schema.is_empty() {
            self.quote(table)
        } else {
            format!("{}.{}", self.quote(schema), self.quote(table))
        }
    }

    /// The `n`-th (1-based) bind parameter, cast to `sql_type` when the
    /// dialect needs it (text parameters into typed columns).
    pub fn param(self, n: usize, sql_type: Option<&str>) -> String {
        match (self, sql_type) {
            (Dialect::Postgres | Dialect::DuckDb, Some(t)) => format!("CAST(${n} AS {t})"),
            (Dialect::Postgres | Dialect::DuckDb, None) => format!("${n}"),
            (Dialect::MsSql, Some(t)) => format!("CAST(@P{n} AS {t})"),
            (Dialect::MsSql, None) => format!("@P{n}"),
            (Dialect::Oracle, _) => format!(":{n}"),
            _ => "?".to_string(),
        }
    }

    /// An expression as text (for text filters on any column type).
    pub fn text(self, expr: &str) -> String {
        match self {
            Dialect::Postgres => format!("{expr}::text"),
            Dialect::MySql => format!("CAST({expr} AS CHAR)"),
            Dialect::MsSql => format!("CAST({expr} AS NVARCHAR(MAX))"),
            Dialect::Oracle => format!("TO_CHAR({expr})"),
            Dialect::ClickHouse => format!("toString({expr})"),
            Dialect::BigQuery => format!("CAST({expr} AS STRING)"),
            Dialect::Snowflake => format!("TO_VARCHAR({expr})"),
            Dialect::Vertica => format!("CAST({expr} AS VARCHAR)"),
            _ => format!("CAST({expr} AS TEXT)"),
        }
    }

    /// `a || b || c` in this dialect.
    pub fn concat(self, parts: &[&str]) -> String {
        match self {
            Dialect::MySql | Dialect::ClickHouse | Dialect::BigQuery => {
                format!("CONCAT({})", parts.join(", "))
            }
            Dialect::MsSql => parts.join(" + "),
            _ => parts.join(" || "),
        }
    }

    /// Case-insensitive `expr LIKE pattern`.
    pub fn ilike(self, expr: &str, pattern: &str, negate: bool) -> String {
        let not = if negate { "NOT " } else { "" };
        match self {
            Dialect::Postgres
            | Dialect::DuckDb
            | Dialect::Snowflake
            | Dialect::ClickHouse
            | Dialect::Vertica => {
                format!("{expr} {not}ILIKE {pattern}")
            }
            // MySQL / SQLite / SQL Server compare case-insensitively by default
            // collation for LIKE; lower() makes it explicit everywhere.
            _ => format!("LOWER({expr}) {not}LIKE LOWER({pattern})"),
        }
    }

    /// `LIMIT … OFFSET …` with bind parameters `limit` / `offset` (already
    /// rendered by [`Self::param`]). SQL Server / Oracle need an ORDER BY,
    /// `order_by` is used (or a constant one) there.
    pub fn page(self, order_by: &str, limit: &str, offset: &str) -> String {
        match self {
            Dialect::MsSql | Dialect::Oracle => {
                let order = if order_by.trim().is_empty() {
                    if self == Dialect::MsSql {
                        "ORDER BY (SELECT NULL)"
                    } else {
                        ""
                    }
                } else {
                    order_by
                };
                format!("{order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY")
            }
            Dialect::Cql => format!("{order_by} LIMIT {limit}"),
            _ => format!("{order_by} LIMIT {limit} OFFSET {offset}"),
        }
    }

    /// `INSERT INTO t DEFAULT VALUES` (MySQL spells it differently).
    pub fn insert_defaults(self, target: &str) -> String {
        match self {
            Dialect::MySql => format!("INSERT INTO {target} () VALUES ()"),
            _ => format!("INSERT INTO {target} DEFAULT VALUES"),
        }
    }

    /// Single-quoted literal.
    pub fn literal(self, s: &str) -> String {
        match self {
            // MySQL treats backslash as an escape inside quotes.
            Dialect::MySql | Dialect::ClickHouse => {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
            }
            Dialect::BigQuery => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            // Backslash starts an escape inside Snowflake strings too.
            Dialect::Snowflake => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
            _ => format!("'{}'", s.replace('\'', "''")),
        }
    }

    /// Engine-specific key column of a table without a primary key
    /// (`ctid` / `rowid` / `ROWID`) as a SELECT expression, and how a
    /// WHERE compares it with bind parameter `param`.
    pub fn row_key(self) -> Option<RowKey> {
        match self {
            Dialect::Postgres => Some(("ctid::text", |p| format!("ctid = CAST({p} AS tid)"))),
            Dialect::Sqlite => Some(("CAST(rowid AS TEXT)", |p| format!("rowid = {p}"))),
            Dialect::DuckDb => Some(("CAST(rowid AS VARCHAR)", |p| {
                format!("rowid = CAST({p} AS BIGINT)")
            })),
            Dialect::Oracle => Some(("ROWIDTOCHAR(ROWID)", |p| {
                format!("ROWID = CHARTOROWID({p})")
            })),
            _ => None,
        }
    }
}

/// A row-key SELECT expression and how a WHERE compares it with a parameter.
pub type RowKey = (&'static str, fn(&str) -> String);

/// A connection URL, split into form fields.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedUrl {
    pub engine: Engine,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    /// File path (sqlite:///path) or the whole URL (libsql://…).
    pub path: Option<String>,
}

/// `scheme://user:pass@host:port/db` for any engine's scheme.
pub fn parse_url(url: &str) -> Option<ParsedUrl> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_lowercase();
    let engine = match scheme.as_str() {
        "postgres" | "postgresql" => Engine::Postgres,
        "redshift" => Engine::Redshift,
        "cockroach" | "cockroachdb" => Engine::Cockroach,
        "mysql" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        "sqlite" | "file" => Engine::Sqlite,
        "duckdb" => Engine::DuckDb,
        "libsql" | "wss" | "https" => Engine::LibSql,
        "sqlserver" | "mssql" => Engine::MsSql,
        "oracle" => Engine::Oracle,
        "clickhouse" => Engine::ClickHouse,
        "redis" | "rediss" => Engine::Redis,
        "valkey" => Engine::Valkey,
        "mongodb" | "mongodb+srv" => Engine::MongoDb,
        "cassandra" => Engine::Cassandra,
        _ => return None,
    };
    let mut out = ParsedUrl {
        engine,
        ..Default::default()
    };
    match engine.form() {
        Form::File => {
            out.path = Some(rest.to_string());
            return Some(out);
        }
        Form::UrlToken => {
            out.path = Some(url.to_string());
            return Some(out);
        }
        _ => {}
    }
    let decode = |s: &str| {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Some(Ok(v)) = s.get(i + 1..i + 3).map(|h| u8::from_str_radix(h, 16))
            {
                out.push(v);
                i += 3;
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (auth, hostpath) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    if let Some(a) = auth {
        let (u, p) = a.split_once(':').map_or((a, None), |(u, p)| (u, Some(p)));
        out.user = Some(decode(u)).filter(|u| !u.is_empty());
        out.password = p.map(decode);
    }
    let (hostport, db) = hostpath
        .split_once('/')
        .map_or((hostpath, None), |(h, d)| (h, Some(d)));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h, p.parse().ok())
        }
        _ => (hostport, None),
    };
    out.host = Some(host.trim_matches(['[', ']']).to_string()).filter(|h| !h.is_empty());
    out.port = port;
    out.database = db.map(decode).filter(|d| !d.is_empty());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{Dialect, Engine};

    #[test]
    fn quoting_and_params() {
        assert_eq!(Dialect::Postgres.quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect::MySql.quote("a`b"), "`a``b`");
        assert_eq!(Dialect::MsSql.quote("a]b"), "[a]]b]");
        assert_eq!(
            Dialect::Postgres.param(2, Some("integer")),
            "CAST($2 AS integer)"
        );
        assert_eq!(Dialect::MySql.param(2, Some("int")), "?");
        assert_eq!(Dialect::MsSql.param(1, None), "@P1");
        assert_eq!(
            Dialect::MySql.concat(&["'%'", "?", "'%'"]),
            "CONCAT('%', ?, '%')"
        );
        assert_eq!(
            Dialect::MsSql.page("", "@P1", "@P2"),
            "ORDER BY (SELECT NULL) OFFSET @P2 ROWS FETCH NEXT @P1 ROWS ONLY"
        );
        assert_eq!(Dialect::MySql.literal("a\\'b"), "'a\\\\''b'");
    }

    #[test]
    fn parses_connection_urls() {
        let u = super::parse_url("mysql://root:p%40ss@db.local:3307/shop?ssl=true").unwrap(); // gitleaks:allow (test fixture)
        assert_eq!(u.engine, Engine::MySql);
        assert_eq!(u.host.as_deref(), Some("db.local"));
        assert_eq!(u.port, Some(3307));
        assert_eq!(u.user.as_deref(), Some("root"));
        assert_eq!(u.password.as_deref(), Some("p@ss"));
        assert_eq!(u.database.as_deref(), Some("shop"));
        let s = super::parse_url("sqlite:///tmp/a.db").unwrap();
        assert_eq!(s.path.as_deref(), Some("/tmp/a.db"));
        assert!(super::parse_url("nope").is_none());
        assert_eq!(super::parse_url("redis://localhost").unwrap().port, None);
    }

    #[test]
    fn every_engine_has_metadata() {
        for e in Engine::ALL {
            assert!(!e.label().is_empty());
            assert_eq!(e.abbr().len(), 2);
        }
        // RES twins with bundled logos (the rest fall back to the badge).
        assert!(crate::icons::engine_logo(Engine::Redis).is_some());
        assert!(crate::icons::engine_logo(Engine::Valkey).is_some());
        assert_eq!(Engine::ALL.len(), 21);
    }
}
