//! Editor completion from the connection's own catalog, for engines without
//! a language server: keywords of the engine's language, the current
//! schema's tables and views, and the columns of the tables a statement
//! names (`alias.` completes that table's columns). MongoDB completes
//! `db.<collection>.<method>` and `$` operators, Redis its commands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::{App, AppContext as _, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind as K, CompletionResponse,
    CompletionTextEdit, Range, TextEdit,
};

use crate::drivers::Db;
use crate::engine::Engine;

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "AS",
    "ON",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "EXCEPT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE INDEX",
    "ALTER TABLE",
    "DROP TABLE",
    "DROP VIEW",
    "TRUNCATE TABLE",
    "ADD COLUMN",
    "DROP COLUMN",
    "PRIMARY KEY",
    "FOREIGN KEY",
    "REFERENCES",
    "DEFAULT",
    "UNIQUE",
    "CHECK",
    "WITH",
    "OVER",
    "PARTITION BY",
    "WINDOW",
    "CAST",
    "TRUE",
    "FALSE",
];

const SQL_FUNCTIONS: &[&str] = &[
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "NULLIF",
    "LOWER",
    "UPPER",
    "TRIM",
    "LENGTH",
    "SUBSTR",
    "REPLACE",
    "ROUND",
    "ABS",
    "CONCAT",
    "NOW",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "ROW_NUMBER",
    "RANK",
    "DENSE_RANK",
    "LAG",
    "LEAD",
];

fn dialect_keywords(e: Engine) -> &'static [&'static str] {
    match e {
        Engine::Oracle => &[
            "FETCH FIRST",
            "ROWS ONLY",
            "ROWNUM",
            "ROWID",
            "DUAL",
            "SYSDATE",
            "NVL",
            "DECODE",
            "TO_CHAR",
            "TO_DATE",
            "MERGE INTO",
            "CONNECT BY",
            "SYSTIMESTAMP",
        ],
        Engine::Snowflake => &[
            "QUALIFY",
            "ILIKE",
            "FLATTEN",
            "LATERAL",
            "IFF",
            "TRY_CAST",
            "PARSE_JSON",
            "VARIANT",
            "USE WAREHOUSE",
            "SHOW TABLES",
            "DESCRIBE TABLE",
            "MERGE INTO",
        ],
        Engine::BigQuery => &[
            "QUALIFY",
            "UNNEST",
            "STRUCT",
            "ARRAY",
            "SAFE_CAST",
            "EXCEPT",
            "REPLACE",
            "PARSE_JSON",
            "FORMAT_DATE",
            "DATE_TRUNC",
            "TIMESTAMP_TRUNC",
            "MERGE",
        ],
        Engine::DuckDb => &[
            "ILIKE",
            "QUALIFY",
            "EXCLUDE",
            "PIVOT",
            "UNPIVOT",
            "DESCRIBE",
            "SUMMARIZE",
            "read_csv",
            "read_parquet",
            "read_json",
            "COPY",
            "ATTACH",
        ],
        Engine::Sqlite | Engine::LibSql | Engine::CloudflareD1 => &[
            "PRAGMA",
            "AUTOINCREMENT",
            "INTEGER PRIMARY KEY",
            "WITHOUT ROWID",
            "GLOB",
            "IFNULL",
            "ON CONFLICT",
            "RETURNING",
            "sqlite_master",
        ],
        Engine::Cassandra => &[
            "KEYSPACE",
            "ALLOW FILTERING",
            "USING TTL",
            "USING TIMESTAMP",
            "IF NOT EXISTS",
            "IF EXISTS",
            "CLUSTERING ORDER BY",
            "PARTITION KEY",
            "TOKEN",
            "WRITETIME",
            "TTL",
            "BATCH",
            "APPLY BATCH",
            "CONTAINS",
            "CONTAINS KEY",
            "DESCRIBE",
        ],
        Engine::DynamoDb => &[
            "EXISTS",
            "MISSING",
            "begins_with",
            "contains",
            "attribute_type",
            "size",
            "VALUE",
        ],
        _ => &[],
    }
}

const MONGO_DB_METHODS: &[&str] = &[
    "getCollection",
    "getCollectionNames",
    "getSiblingDB",
    "runCommand",
    "adminCommand",
    "createCollection",
    "dropDatabase",
    "stats",
];
const MONGO_METHODS: &[&str] = &[
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "drop",
    "getIndexes",
    "createIndex",
    "dropIndex",
];
const MONGO_CURSOR: &[&str] = &["sort", "limit", "skip", "count"];
const MONGO_OPERATORS: &[&str] = &[
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$and",
    "$or",
    "$nor",
    "$not",
    "$exists",
    "$type",
    "$regex",
    "$elemMatch",
    "$size",
    "$all",
    "$set",
    "$unset",
    "$inc",
    "$push",
    "$pull",
    "$addToSet",
    "$match",
    "$group",
    "$project",
    "$sort",
    "$limit",
    "$skip",
    "$unwind",
    "$lookup",
    "$count",
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$first",
    "$last",
    "$addFields",
    "$facet",
];
const REDIS_COMMANDS: &[&str] = &[
    "GET",
    "SET",
    "MGET",
    "MSET",
    "DEL",
    "EXISTS",
    "EXPIRE",
    "PERSIST",
    "TTL",
    "PTTL",
    "TYPE",
    "KEYS",
    "SCAN",
    "RENAME",
    "INCR",
    "INCRBY",
    "DECR",
    "APPEND",
    "STRLEN",
    "GETRANGE",
    "HGET",
    "HSET",
    "HGETALL",
    "HDEL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HINCRBY",
    "HSCAN",
    "LPUSH",
    "RPUSH",
    "LPOP",
    "RPOP",
    "LRANGE",
    "LLEN",
    "LINDEX",
    "LREM",
    "LTRIM",
    "SADD",
    "SREM",
    "SMEMBERS",
    "SISMEMBER",
    "SCARD",
    "SSCAN",
    "SUNION",
    "SINTER",
    "ZADD",
    "ZREM",
    "ZRANGE",
    "ZREVRANGE",
    "ZRANGEBYSCORE",
    "ZSCORE",
    "ZCARD",
    "ZINCRBY",
    "ZSCAN",
    "XADD",
    "XRANGE",
    "XLEN",
    "XREAD",
    "PUBLISH",
    "INFO",
    "DBSIZE",
    "FLUSHDB",
    "SELECT",
    "PING",
    "CONFIG GET",
    "CLIENT LIST",
    "MEMORY USAGE",
    "OBJECT ENCODING",
];

#[derive(Clone, Default)]
struct Catalog {
    /// Tables / views of a schema: (name, is_view).
    objects: HashMap<String, Vec<(String, bool)>>,
    /// (column, type) of `schema.table`.
    columns: HashMap<String, Vec<(String, String)>>,
    /// When a schema's object list was last re-read for an unknown name.
    refreshed: HashMap<String, std::time::Instant>,
}

/// One editor's completion source; cheap to clone (shared catalog cache).
#[derive(Clone)]
pub struct SchemaCompletion {
    db: Db,
    schema: Arc<Mutex<String>>,
    catalog: Arc<Mutex<Catalog>>,
}

impl SchemaCompletion {
    pub fn new(db: Db, schema: String) -> Self {
        Self {
            db,
            schema: Arc::new(Mutex::new(schema)),
            catalog: Default::default(),
        }
    }

    /// The schema unqualified names resolve in (the sidebar's).
    pub fn set_schema(&self, schema: &str) {
        *self.schema.lock().unwrap() = schema.to_string();
    }
}

async fn objects(db: &Db, cat: &Arc<Mutex<Catalog>>, schema: &str) -> Vec<(String, bool)> {
    if let Some(o) = cat.lock().unwrap().objects.get(schema) {
        return o.clone();
    }
    let tree = db
        .driver()
        .objects(schema.to_string())
        .await
        .unwrap_or_default();
    let mut out: Vec<(String, bool)> = tree.tables.into_iter().map(|t| (t, false)).collect();
    out.extend(
        tree.views
            .into_iter()
            .chain(tree.matviews)
            .map(|v| (v, true)),
    );
    cat.lock()
        .unwrap()
        .objects
        .insert(schema.to_string(), out.clone());
    out
}

async fn columns(
    db: &Db,
    cat: &Arc<Mutex<Catalog>>,
    schema: &str,
    table: &str,
) -> Vec<(String, String)> {
    let key = format!("{schema}.{table}");
    if let Some(c) = cat.lock().unwrap().columns.get(&key) {
        return c.clone();
    }
    // Only tables / views the (cached) catalog knows: while typing, half a
    // word after FROM (`pro`, `wh`) would otherwise cost a metadata query
    // per keystroke.
    let is_known =
        |objs: &[(String, bool)]| objs.iter().any(|(n, _)| n.eq_ignore_ascii_case(table));
    if !is_known(&objects(db, cat, schema).await) {
        // Maybe created since the list was read: re-read it, at most every
        // few seconds (not per keystroke).
        let stale = {
            let mut c = cat.lock().unwrap();
            let now = std::time::Instant::now();
            let due = c
                .refreshed
                .get(schema)
                .is_none_or(|t| now.duration_since(*t).as_secs() >= 5);
            if due {
                c.refreshed.insert(schema.to_string(), now);
                c.objects.remove(schema);
            }
            due
        };
        if !stale || !is_known(&objects(db, cat, schema).await) {
            return Vec::new();
        }
    }
    let cols: Vec<(String, String)> = db
        .driver()
        .columns(schema.to_string(), table.to_string())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c.name, c.sql_type))
        .collect();
    cat.lock().unwrap().columns.insert(key, cols.clone());
    cols
}

/// An identifier with its quotes removed.
fn unquote(s: &str) -> String {
    let s = s.trim();
    for (a, b) in [('"', '"'), ('`', '`'), ('[', ']')] {
        if let Some(inner) = s.strip_prefix(a).and_then(|r| r.strip_suffix(b)) {
            return inner.to_string();
        }
    }
    s.to_string()
}

/// Tables a statement names after FROM / JOIN / UPDATE / INTO, with their
/// aliases: `(alias or name, table)`.
pub fn referenced_tables(sql: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == ';')
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = Vec::new();
    let stop = |w: &str| {
        matches!(
            w.to_uppercase().as_str(),
            "WHERE"
                | "JOIN"
                | "INNER"
                | "LEFT"
                | "RIGHT"
                | "FULL"
                | "CROSS"
                | "ON"
                | "GROUP"
                | "ORDER"
                | "LIMIT"
                | "SET"
                | "VALUES"
                | "USING"
                | "HAVING"
                | "UNION"
                | "WINDOW"
                | "QUALIFY"
                | "ALLOW"
                | "OUTER"
                | "NATURAL"
                | "SELECT"
        )
    };
    for (i, w) in words.iter().enumerate() {
        if !matches!(
            w.to_uppercase().as_str(),
            "FROM" | "JOIN" | "UPDATE" | "INTO"
        ) {
            continue;
        }
        let Some(t) = words.get(i + 1).filter(|t| !stop(t)) else {
            continue;
        };
        let table = unquote(t.rsplit('.').next().unwrap_or(t));
        let mut alias = table.clone();
        let mut j = i + 2;
        if words.get(j).is_some_and(|a| a.eq_ignore_ascii_case("AS")) {
            j += 1;
        }
        if let Some(a) = words
            .get(j)
            .filter(|a| !stop(a) && a.chars().all(|c| c.is_alphanumeric() || c == '_'))
        {
            alias = a.to_string();
        }
        out.push((alias, table));
    }
    out
}

/// The identifier typed right before `offset`, and the qualifier before a
/// `.` that precedes it (`alias.na|` → (`na`, Some(`alias`))).
fn context(text: &str, offset: usize) -> (String, Option<String>) {
    let prefix = crate::lsp::word_prefix(text, offset);
    let start = offset - prefix.len();
    let before = &text[..start];
    let qualifier = before.strip_suffix('.').map(|b| {
        let q = crate::lsp::word_prefix(b, b.len());
        if q.is_empty() {
            // A quoted qualifier: `"my table".`
            b.rsplit(['"', '`']).nth(1).unwrap_or("").to_string()
        } else {
            q
        }
    });
    (prefix, qualifier)
}

fn item(label: &str, kind: K, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: Some(detail.to_string()).filter(|d| !d.is_empty()),
        ..Default::default()
    }
}

/// Items matching `prefix` (case-insensitive prefix first, then substring),
/// each replacing `range` on accept.
fn finish(items: Vec<CompletionItem>, prefix: &str, range: Range) -> Vec<CompletionItem> {
    let p = prefix.to_lowercase();
    let upper = crate::settings::get().editor_uppercase_keywords;
    let mut starts = Vec::new();
    let mut inside = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for mut it in items {
        if !seen.insert((it.label.clone(), it.kind.map(|k| format!("{k:?}")))) {
            continue;
        }
        if it.kind == Some(K::KEYWORD) && !upper && it.label.chars().any(|c| c.is_ascii_uppercase())
        {
            it.label = it.label.to_lowercase();
        }
        let l = it.label.to_lowercase();
        let bucket = if l.starts_with(&p) {
            &mut starts
        } else if !p.is_empty() && l.contains(&p) {
            &mut inside
        } else {
            continue;
        };
        it.filter_text = Some(if l.starts_with(&p) {
            prefix.to_string()
        } else {
            String::new()
        });
        it.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: it.label.clone(),
        }));
        bucket.push(it);
    }
    starts.extend(inside);
    starts.truncate(300);
    starts
}

impl SchemaCompletion {
    async fn sql_items(
        self,
        text: String,
        prefix: String,
        qualifier: Option<String>,
    ) -> Vec<CompletionItem> {
        let schema = self.schema.lock().unwrap().clone();
        let engine = self.db.engine();
        let refs = referenced_tables(&text);
        if let Some(q) = qualifier {
            // `alias.` / `table.` → its columns; `schema.` → its tables.
            let table = refs
                .iter()
                .find(|(a, _)| a.eq_ignore_ascii_case(&q))
                .map(|(_, t)| t.clone())
                .unwrap_or(q.clone());
            let cols = columns(&self.db, &self.catalog, &schema, &table).await;
            if !cols.is_empty() {
                return cols.iter().map(|(c, t)| item(c, K::FIELD, t)).collect();
            }
            return objects(&self.db, &self.catalog, &q)
                .await
                .iter()
                .map(|(n, view)| {
                    item(
                        n,
                        if *view { K::INTERFACE } else { K::CLASS },
                        if *view { "View" } else { "Table" },
                    )
                })
                .collect();
        }
        let mut items = Vec::new();
        for (_, t) in &refs {
            for (c, ty) in columns(&self.db, &self.catalog, &schema, t).await {
                items.push(item(&c, K::FIELD, &format!("{ty} · {t}")));
            }
        }
        for (n, view) in objects(&self.db, &self.catalog, &schema).await {
            items.push(item(
                &n,
                if view { K::INTERFACE } else { K::CLASS },
                &format!("{} · {schema}", if view { "View" } else { "Table" }),
            ));
        }
        let language = if engine == Engine::DynamoDb {
            "PartiQL"
        } else if engine == Engine::Cassandra {
            "CQL"
        } else {
            ""
        };
        for k in dialect_keywords(engine).iter().chain(SQL_KEYWORDS) {
            items.push(item(k, K::KEYWORD, language));
        }
        if !prefix.is_empty() {
            for f in SQL_FUNCTIONS {
                items.push(item(f, K::FUNCTION, "function"));
            }
        }
        items
    }

    async fn mongo_items(
        self,
        text: String,
        offset: usize,
        prefix: String,
        qualifier: Option<String>,
    ) -> Vec<CompletionItem> {
        let schema = self.schema.lock().unwrap().clone();
        let before = &text[..offset];
        let statement = before.rsplit(['\n', ';']).next().unwrap_or("");
        let collection = || -> Option<String> {
            let rest = statement.trim_start().strip_prefix("db.")?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if let Some(q) = rest.strip_prefix("getCollection(") {
                return Some(unquote(
                    q.split(')').next().unwrap_or("").trim_matches('\''),
                ));
            }
            Some(name).filter(|n| !n.is_empty())
        };
        match qualifier.as_deref() {
            Some("db") => {
                let mut items: Vec<CompletionItem> = objects(&self.db, &self.catalog, &schema)
                    .await
                    .iter()
                    .map(|(n, _)| item(n, K::CLASS, "collection"))
                    .collect();
                items.extend(MONGO_DB_METHODS.iter().map(|m| item(m, K::METHOD, "db")));
                return items;
            }
            Some(_) if statement.contains(')') => {
                return MONGO_CURSOR
                    .iter()
                    .chain(MONGO_METHODS)
                    .map(|m| item(m, K::METHOD, "cursor"))
                    .collect();
            }
            Some(_) => {
                return MONGO_METHODS
                    .iter()
                    .map(|m| item(m, K::METHOD, "collection"))
                    .collect();
            }
            None => {}
        }
        // Inside the call's arguments: fields and operators.
        let dollar = before[..offset - prefix.len()].ends_with('$');
        let mut items: Vec<CompletionItem> = MONGO_OPERATORS
            .iter()
            .map(|o| item(if dollar { &o[1..] } else { o }, K::OPERATOR, "operator"))
            .collect();
        if !dollar && let Some(c) = collection() {
            for (f, t) in columns(&self.db, &self.catalog, &schema, &c).await {
                items.push(item(&f, K::FIELD, &t));
            }
        }
        if statement.trim().is_empty() || "db".starts_with(statement.trim()) {
            items.push(item("db", K::VARIABLE, "database"));
        }
        items
    }
}

impl CompletionProvider for SchemaCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let full = text.to_string();
        let (prefix, qualifier) = context(&full, offset);
        let range = Range {
            start: text.offset_to_position(offset - prefix.len()),
            end: text.offset_to_position(offset),
        };
        let this = self.clone();
        let engine = self.db.engine();
        cx.background_spawn(async move {
            let items = match engine {
                Engine::Redis | Engine::Valkey => {
                    // Commands at the start of a line only.
                    let line = full[..offset - prefix.len()]
                        .rsplit('\n')
                        .next()
                        .unwrap_or("");
                    if line.trim().is_empty() {
                        REDIS_COMMANDS
                            .iter()
                            .map(|c| item(c, K::KEYWORD, "command"))
                            .collect()
                    } else {
                        Vec::new()
                    }
                }
                Engine::MongoDb => {
                    this.mongo_items(full, offset, prefix.clone(), qualifier)
                        .await
                }
                _ => this.sql_items(full, prefix.clone(), qualifier).await,
            };
            Ok(CompletionResponse::Array(finish(items, &prefix, range)))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '$')
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn statements_name_their_tables() {
        let r = super::referenced_tables(
            "SELECT c.na FROM shop.customers c JOIN \"orders\" AS o ON o.cid = c.id WHERE",
        );
        assert_eq!(
            r,
            [
                ("c".to_string(), "customers".to_string()),
                ("o".to_string(), "orders".to_string())
            ]
        );
        let r = super::referenced_tables("UPDATE people SET x = 1");
        assert_eq!(r, [("people".to_string(), "people".to_string())]);
        assert_eq!(
            super::context("SELECT c.na", 11),
            ("na".to_string(), Some("c".to_string()))
        );
        assert_eq!(
            super::context("SELECT \"my t\".", 14),
            (String::new(), Some("my t".to_string()))
        );
        assert_eq!(super::context("SELECT na", 9), ("na".to_string(), None));
    }

    #[test]
    fn live_catalog_completion() {
        if !crate::drivers::live::reachable(37017) || !crate::drivers::live::reachable(33306) {
            return;
        }
        let rt = crate::db::runtime();
        let labels = |items: Vec<lsp_types::CompletionItem>| {
            items.into_iter().map(|i| i.label).collect::<Vec<_>>()
        };
        // SQL: tables, then `alias.` columns.
        let c = crate::drivers::live::conn(crate::engine::Engine::MySql, 33306, "root", "shop");
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                "tusk".into(),
            ))
            .unwrap();
        let sc = super::SchemaCompletion::new(db, "shop".into());
        let t = labels(rt.block_on(sc.clone().sql_items(
            "SELECT * FROM ".into(),
            String::new(),
            None,
        )));
        assert!(t.contains(&"customers".to_string()), "{t:?}");
        let cols = labels(rt.block_on(sc.sql_items(
            "SELECT c. FROM customers c".into(),
            String::new(),
            Some("c".into()),
        )));
        assert!(cols.contains(&"email".to_string()), "{cols:?}");
        // MongoDB: collections after `db.`, fields inside the filter.
        let c =
            crate::drivers::live::conn(crate::engine::Engine::MongoDb, 37017, "", "tusk_complete");
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                String::new(),
            ))
            .unwrap();
        rt.block_on(
            db.driver()
                .exec(r#"db.people.insertOne({email: "a@x", age: 3})"#.into()),
        )
        .unwrap();
        let sc = super::SchemaCompletion::new(db.clone(), "tusk_complete".into());
        let text = "db.";
        let colls = labels(rt.block_on(sc.clone().mongo_items(
            text.into(),
            3,
            String::new(),
            Some("db".into()),
        )));
        assert!(colls.contains(&"people".to_string()), "{colls:?}");
        let text = "db.people.find({ em";
        let fields =
            labels(rt.block_on(sc.mongo_items(text.into(), text.len(), "em".into(), None)));
        assert!(
            fields.contains(&"email".to_string()) && fields.contains(&"$gt".to_string()),
            "{fields:?}"
        );
        rt.block_on(db.driver().exec("db.dropDatabase()".into()))
            .unwrap();
    }
}
