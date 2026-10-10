//! Redis / Valkey: each database index is a schema (`db0` …), with one
//! `keys` table (key, type, ttl, value). Keys are collected with SCAN,
//! sorted and paged; the editor runs Redis commands (`GET k`, `HGETALL h`,
//! …).

use std::collections::HashMap;
use std::sync::Arc;

use redis::aio::ConnectionManager;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{Db, Driver, Fut, GridOp, WindowReq};
use crate::db::{
    self, DbResult, FilterTerm, GridColumnMeta, ObjectTree, SavedConnection, Stmt, WhereClause,
};
use crate::engine::Engine;
use crate::filter::FilterOp;
use crate::objects::{ObjKind, Script};

/// Keys read per table view (SCAN stops there).
const KEY_CAP: usize = 200_000;
pub const TABLE: &str = "keys";

pub struct Redis {
    /// Redis or Valkey — same wire protocol (RESP), same commands.
    engine: Engine,
    info: redis::ConnectionInfo,
    default_db: i64,
    conns: Arc<Mutex<HashMap<i64, ConnectionManager>>>,
}

pub async fn connect(
    conn: &SavedConnection,
    host: String,
    port: u16,
    password: String,
) -> DbResult<Db> {
    let default_db: i64 = conn
        .database
        .trim()
        .trim_start_matches("db")
        .parse()
        .unwrap_or(0);
    let info = redis::ConnectionInfo {
        addr: redis::ConnectionAddr::Tcp(host, port),
        redis: redis::RedisConnectionInfo {
            db: 0,
            username: Some(conn.user.clone()).filter(|u| !u.is_empty()),
            password: Some(password).filter(|p| !p.is_empty()),
            ..Default::default()
        },
    };
    let r = Redis {
        engine: conn.engine,
        info,
        default_db,
        conns: Default::default(),
    };
    r.conn(default_db).await?;
    Ok(Db::new(r))
}

fn err(e: redis::RedisError) -> String {
    e.to_string()
}

fn db_index(schema: &str) -> i64 {
    schema.trim_start_matches("db").parse().unwrap_or(0)
}

/// A reply as JSON.
fn json_of(v: redis::Value) -> Value {
    use redis::Value as R;
    match v {
        R::Nil => Value::Null,
        R::Int(n) => n.into(),
        R::BulkString(b) => match String::from_utf8(b) {
            Ok(s) => Value::String(s),
            Err(e) => super::hex(e.as_bytes()),
        },
        R::SimpleString(s) => Value::String(s),
        R::Okay => Value::String("OK".into()),
        R::Array(a) | R::Set(a) => Value::Array(a.into_iter().map(json_of).collect()),
        R::Map(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let k = match json_of(k) {
                        Value::String(s) => s,
                        o => o.to_string(),
                    };
                    (k, json_of(v))
                })
                .collect(),
        ),
        R::Double(f) => Value::from(f),
        R::Boolean(b) => Value::Bool(b),
        R::VerbatimString { text, .. } => Value::String(text),
        R::BigNumber(n) => Value::String(n.to_string()),
        R::Attribute { data, .. } => json_of(*data),
        R::Push { data, .. } => Value::Array(data.into_iter().map(json_of).collect()),
        R::ServerError(e) => Value::String(format!("{e:?}")),
    }
}

/// `SET k "a b"` → ["SET", "k", "a b"] (double / single quotes, backslash escapes).
pub fn split_command(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (c, quote) {
            ('\\', Some('"')) => {
                if let Some(n) = chars.next() {
                    cur.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        o => o,
                    });
                }
            }
            (q, Some(open)) if q == open => quote = None,
            (c, Some(_)) => cur.push(c),
            ('"' | '\'', None) => {
                quote = Some(c);
                any = true;
            }
            (c, None) if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (c, None) => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// `CLIENT LIST` lines (`id=3 addr=… cmd=get`) as rows, id first.
pub fn parse_client_list(text: &str) -> Vec<Value> {
    const SHOWN: &[&str] = &[
        "id", "addr", "name", "user", "db", "age", "idle", "cmd", "flags",
    ];
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let fields: HashMap<&str, &str> =
                l.split(' ').filter_map(|kv| kv.split_once('=')).collect();
            Value::Object(
                SHOWN
                    .iter()
                    .map(|k| {
                        let v = fields.get(k).copied().unwrap_or("");
                        let v = v
                            .parse::<i64>()
                            .map(Value::from)
                            .unwrap_or_else(|_| Value::String(v.to_string()));
                        (k.to_string(), v)
                    })
                    .collect(),
            )
        })
        .collect()
}

/// A glob for SCAN MATCH from the `key` filter terms (else `*`).
fn key_pattern(terms: &[FilterTerm]) -> String {
    if let Some(t) = terms.iter().find(|t| t.column == "key") {
        // Glob metacharacters in a typed value would widen the match.
        let v = t.value.replace(['*', '?', '[', ']'], "");
        return match t.op {
            FilterOp::Eq => v,
            FilterOp::HasPrefix => format!("{v}*"),
            FilterOp::HasSuffix => format!("*{v}"),
            FilterOp::Like | FilterOp::ILike => t.value.replace('%', "*").replace('_', "?"),
            _ => format!("*{v}*"),
        };
    }
    "*".into()
}

fn type_filter(terms: &[FilterTerm]) -> Option<String> {
    terms
        .iter()
        .find(|t| t.column == "type" && t.op == FilterOp::Eq)
        .map(|t| t.value.to_lowercase())
}

impl Redis {
    async fn conn(&self, index: i64) -> DbResult<ConnectionManager> {
        let mut map = self.conns.lock().await;
        if let Some(c) = map.get(&index) {
            return Ok(c.clone());
        }
        let mut info = self.info.clone();
        info.redis.db = index;
        let client = redis::Client::open(info).map_err(err)?;
        let c =
            db::run_db(async move { ConnectionManager::new(client).await.map_err(err) }).await?;
        map.insert(index, c.clone());
        Ok(c)
    }

    async fn command(&self, index: i64, args: Vec<String>) -> DbResult<redis::Value> {
        let mut c = self.conn(index).await?;
        let label = args.join(" ");
        db::run_logged(label, crate::console::Source::Data, async move {
            let mut cmd = redis::cmd(&args[0]);
            for a in &args[1..] {
                cmd.arg(a);
            }
            cmd.query_async::<redis::Value>(&mut c).await.map_err(err)
        })
        .await
    }

    async fn keys(&self, index: i64, filter: &Option<WhereClause>) -> DbResult<Vec<String>> {
        let terms = filter.as_ref().map(|f| f.terms.clone()).unwrap_or_default();
        let pattern = key_pattern(&terms);
        let ty = type_filter(&terms);
        let mut c = self.conn(index).await?;
        db::run_logged(
            format!("SCAN 0 MATCH {pattern}"),
            crate::console::Source::Meta,
            async move {
                let mut keys = Vec::new();
                let mut cursor: u64 = 0;
                loop {
                    let mut cmd = redis::cmd("SCAN");
                    cmd.arg(cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(1000);
                    if let Some(t) = &ty {
                        cmd.arg("TYPE").arg(t);
                    }
                    let (next, batch): (u64, Vec<String>) =
                        cmd.query_async(&mut c).await.map_err(err)?;
                    keys.extend(batch);
                    cursor = next;
                    if cursor == 0 || keys.len() >= KEY_CAP {
                        break;
                    }
                }
                keys.sort();
                keys.dedup();
                Ok(keys)
            },
        )
        .await
    }

    /// (type, ttl, value) of a key.
    async fn describe(&self, c: &mut ConnectionManager, key: &str) -> (String, i64, Value) {
        let ty: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(c)
            .await
            .unwrap_or_else(|_| "none".into());
        let ttl: i64 = redis::cmd("TTL")
            .arg(key)
            .query_async(c)
            .await
            .unwrap_or(-1);
        let q = |name: &str| {
            let mut cmd = redis::cmd(name);
            cmd.arg(key);
            cmd
        };
        let v = match ty.as_str() {
            "string" => q("GET").query_async::<redis::Value>(c).await,
            "hash" => q("HGETALL")
                .query_async::<HashMap<String, String>>(c)
                .await
                .map(|m| {
                    let mut v: Vec<(String, String)> = m.into_iter().collect();
                    v.sort();
                    redis::Value::Map(
                        v.into_iter()
                            .map(|(k, x)| {
                                (
                                    redis::Value::BulkString(k.into_bytes()),
                                    redis::Value::BulkString(x.into_bytes()),
                                )
                            })
                            .collect(),
                    )
                }),
            "list" => q("LRANGE").arg(0).arg(199).query_async(c).await,
            "set" => q("SMEMBERS").query_async(c).await,
            "zset" => {
                q("ZRANGE")
                    .arg(0)
                    .arg(199)
                    .arg("WITHSCORES")
                    .query_async(c)
                    .await
            }
            "stream" => {
                q("XRANGE")
                    .arg("-")
                    .arg("+")
                    .arg("COUNT")
                    .arg(50)
                    .query_async(c)
                    .await
            }
            _ => Ok(redis::Value::Nil),
        };
        (ty, ttl, v.map(json_of).unwrap_or(Value::Null))
    }
}

fn columns() -> Vec<GridColumnMeta> {
    let col = |name: &str, ty: &str, pk: bool| GridColumnMeta {
        name: name.into(),
        pg_type: super::short_type(ty),
        sql_type: ty.into(),
        nullable: !pk,
        default: None,
        comment: None,
        is_pk: pk,
        foreign_key: None,
        enum_values: Vec::new(),
    };
    vec![
        col("key", "text", true),
        col("type", "text", false),
        col("ttl", "bigint", false),
        col("value", "text", false),
    ]
}

impl Driver for Redis {
    fn engine(&self) -> Engine {
        self.engine
    }
    fn default_schema(&self) -> Option<String> {
        Some(format!("db{}", self.default_db))
    }
    fn row_key(&self) -> bool {
        false
    }
    fn version(&self) -> Fut<String> {
        let this = self.clone_handle();
        let label = self.engine.label().to_string();
        Box::pin(async move {
            let v = this
                .command(this.default_db, vec!["INFO".into(), "server".into()])
                .await?;
            let text = json_of(v).as_str().unwrap_or_default().to_string();
            // Valkey reports valkey_version: (plus a redis_version: for
            // compatibility); Redis only the latter.
            let ver = text
                .lines()
                .find_map(|l| l.strip_prefix("valkey_version:"))
                .or_else(|| text.lines().find_map(|l| l.strip_prefix("redis_version:")))
                .unwrap_or("?")
                .trim()
                .to_string();
            Ok(format!("{label} {ver}"))
        })
    }
    fn databases(&self) -> Fut<Vec<String>> {
        self.schemas()
    }
    /// "Size" per db index: the number of keys, from INFO keyspace —
    /// one command for every index, no SELECT churn. Empty indexes
    /// (no keyspace line) report nothing.
    fn database_sizes(&self) -> Fut<Vec<(String, u64)>> {
        let this = self.clone_handle();
        Box::pin(async move {
            let v = this
                .command(this.default_db, vec!["INFO".into(), "keyspace".into()])
                .await?;
            let text = json_of(v).as_str().unwrap_or_default().to_string();
            Ok(text
                .lines()
                .filter_map(|l| {
                    let (db, rest) = l.split_once(':')?;
                    if !db.starts_with("db") {
                        return None;
                    }
                    let keys = rest
                        .split(',')
                        .find_map(|p| p.strip_prefix("keys="))?
                        .parse()
                        .ok()?;
                    Some((db.to_string(), keys))
                })
                .collect())
        })
    }
    fn schemas(&self) -> Fut<Vec<String>> {
        let this = self.clone_handle();
        Box::pin(async move {
            let n = match this
                .command(
                    this.default_db,
                    vec!["CONFIG".into(), "GET".into(), "databases".into()],
                )
                .await
            {
                Ok(v) => match json_of(v) {
                    Value::Array(a) => a
                        .get(1)
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(16),
                    Value::Object(o) => o
                        .get("databases")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(16),
                    _ => 16,
                },
                Err(_) => 16,
            };
            Ok((0..n).map(|i| format!("db{i}")).collect())
        })
    }
    fn objects(&self, _schema: String) -> Fut<ObjectTree> {
        Box::pin(async {
            Ok(ObjectTree {
                tables: vec![TABLE.into()],
                ..Default::default()
            })
        })
    }
    fn columns(&self, _schema: String, _table: String) -> Fut<Vec<GridColumnMeta>> {
        Box::pin(async { Ok(columns()) })
    }
    fn count(&self, schema: String, _table: String, filter: Option<WhereClause>) -> Fut<i64> {
        let this = self.clone_handle();
        Box::pin(async move {
            let index = db_index(&schema);
            if filter.as_ref().is_none_or(|f| f.terms.is_empty()) {
                let n = this.command(index, vec!["DBSIZE".into()]).await?;
                return Ok(json_of(n).as_i64().unwrap_or(0));
            }
            Ok(this.keys(index, &filter).await?.len() as i64)
        })
    }
    fn window(&self, req: WindowReq) -> Fut<Vec<Value>> {
        let this = self.clone_handle();
        Box::pin(async move {
            let index = db_index(&req.schema);
            let keys = this.keys(index, &req.filter).await?;
            let desc = req.order_by.as_deref().is_some_and(|o| o.contains("DESC"));
            let page: Vec<String> = if desc {
                keys.iter()
                    .rev()
                    .skip(req.offset as usize)
                    .take(req.limit as usize)
                    .cloned()
                    .collect()
            } else {
                keys.iter()
                    .skip(req.offset as usize)
                    .take(req.limit as usize)
                    .cloned()
                    .collect()
            };
            let mut c = this.conn(index).await?;
            let mut out = Vec::new();
            for k in page {
                let (ty, ttl, value) = this.describe(&mut c, &k).await;
                out.push(json!({ "key": k, "type": ty, "ttl": ttl, "value": value }));
            }
            Ok(out)
        })
    }
    fn query_rows(&self, sql: String, limit: i64) -> Fut<Vec<Value>> {
        let this = self.clone_handle();
        Box::pin(async move {
            let args = split_command(super::trim_sql(&sql).trim());
            if args.is_empty() {
                return Ok(Vec::new());
            }
            let v = json_of(this.command(this.default_db, args).await?);
            Ok(match v {
                Value::Array(items) => items
                    .into_iter()
                    .take(limit.max(0) as usize)
                    .enumerate()
                    .map(|(i, v)| json!({ "#": i + 1, "value": v }))
                    .collect(),
                Value::Object(m) => m
                    .into_iter()
                    .map(|(k, v)| json!({ "field": k, "value": v }))
                    .collect(),
                other => vec![json!({ "result": other })],
            })
        })
    }
    fn query_columns(&self, _sql: String) -> Fut<Vec<String>> {
        Box::pin(async { Ok(vec!["result".to_string()]) })
    }
    fn exec(&self, sql: String) -> Fut<u64> {
        let f = self.query_rows(sql, i64::MAX);
        Box::pin(async move { Ok(f.await?.len() as u64) })
    }
    fn batch(&self, stmts: Vec<Stmt>) -> Fut<u64> {
        let this = self.clone_handle();
        Box::pin(async move {
            let mut n = 0;
            for st in &stmts {
                let op = super::parse_grid_stmt(st)
                    .ok_or_else(|| format!("Redis can't run: {}", st.sql))?;
                let index = match &op {
                    GridOp::Insert { schema, .. }
                    | GridOp::Update { schema, .. }
                    | GridOp::Delete { schema, .. } => {
                        schema.as_deref().map(db_index).unwrap_or(this.default_db)
                    }
                };
                let key_of = |pairs: &[(String, Option<String>)]| {
                    pairs
                        .iter()
                        .find(|(c, _)| c == "key")
                        .and_then(|(_, v)| v.clone())
                };
                match op {
                    GridOp::Delete { key, .. } => {
                        let k = key_of(&key).ok_or("no key")?;
                        this.command(index, vec!["DEL".into(), k]).await?;
                    }
                    GridOp::Insert { values, .. } => {
                        let k = key_of(&values).ok_or("A new row needs a key")?;
                        let v = values
                            .iter()
                            .find(|(c, _)| c == "value")
                            .and_then(|(_, v)| v.clone())
                            .unwrap_or_default();
                        this.command(index, vec!["SET".into(), k.clone(), v])
                            .await?;
                        if let Some(ttl) = values
                            .iter()
                            .find(|(c, _)| c == "ttl")
                            .and_then(|(_, v)| v.clone())
                        {
                            this.command(index, vec!["EXPIRE".into(), k, ttl]).await?;
                        }
                    }
                    GridOp::Update { set, key, .. } => {
                        let k = key_of(&key).ok_or("no key")?;
                        for (col, v) in set {
                            match col.as_str() {
                                "ttl" => match v.as_deref().and_then(|t| t.parse::<i64>().ok()) {
                                    Some(t) if t >= 0 => {
                                        this.command(
                                            index,
                                            vec!["EXPIRE".into(), k.clone(), t.to_string()],
                                        )
                                        .await?;
                                    }
                                    _ => {
                                        this.command(index, vec!["PERSIST".into(), k.clone()])
                                            .await?;
                                    }
                                },
                                "value" => {
                                    this.write_value(index, &k, v.unwrap_or_default()).await?
                                }
                                "key" => {
                                    if let Some(new) = v {
                                        this.command(index, vec!["RENAME".into(), k.clone(), new])
                                            .await?;
                                    }
                                }
                                other => return Err(format!("The {other} column can't be edited")),
                            }
                        }
                    }
                }
                n += 1;
            }
            Ok(n)
        })
    }
    fn sessions(&self) -> Fut<Vec<Value>> {
        let this = self.clone_handle();
        Box::pin(async move {
            let v = json_of(
                this.command(this.default_db, vec!["CLIENT".into(), "LIST".into()])
                    .await?,
            );
            Ok(parse_client_list(v.as_str().unwrap_or_default()))
        })
    }
    fn signal_session(&self, id: String, kill: bool) -> Fut<()> {
        let this = self.clone_handle();
        Box::pin(async move {
            if !kill {
                return Err("Redis can only close a client's connection.".into());
            }
            let id = id
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("'{id}' isn't a client id"))?;
            this.command(
                this.default_db,
                vec!["CLIENT".into(), "KILL".into(), "ID".into(), id.to_string()],
            )
            .await?;
            Ok(())
        })
    }
    fn script(&self, _kind: ObjKind, _schema: String, _name: String, which: Script) -> Fut<String> {
        Box::pin(async move {
            Ok(match which {
                Script::Select => "SCAN 0 MATCH * COUNT 100".into(),
                Script::Drop | Script::Truncate => "FLUSHDB".into(),
                _ => "# Redis keys have no schema: SET / HSET / LPUSH create them.".into(),
            })
        })
    }
}

impl Redis {
    fn clone_handle(&self) -> Arc<Redis> {
        Arc::new(Redis {
            engine: self.engine,
            info: self.info.clone(),
            default_db: self.default_db,
            conns: self.conns.clone(),
        })
    }

    /// Write a grid-edited value back in the key's own type (JSON for
    /// hash / list / set / zset).
    async fn write_value(&self, index: i64, key: &str, text: String) -> DbResult<()> {
        let ty = json_of(self.command(index, vec!["TYPE".into(), key.into()]).await?);
        let ty = ty.as_str().unwrap_or("string").to_string();
        let parsed: Option<Value> = serde_json::from_str(&text).ok();
        let items = |v: &Value| -> Vec<String> {
            v.as_array()
                .into_iter()
                .flatten()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                })
                .collect()
        };
        let base = |cmd: &str| vec![cmd.to_string(), key.to_string()];
        match (ty.as_str(), &parsed) {
            ("hash", Some(Value::Object(m))) => {
                self.command(index, base("DEL")).await?;
                let mut args = base("HSET");
                for (f, v) in m {
                    args.push(f.clone());
                    args.push(match v {
                        Value::String(s) => s.clone(),
                        o => o.to_string(),
                    });
                }
                self.command(index, args).await?;
            }
            ("list", Some(v @ Value::Array(_))) => {
                self.command(index, base("DEL")).await?;
                let mut args = base("RPUSH");
                args.extend(items(v));
                self.command(index, args).await?;
            }
            ("set", Some(v @ Value::Array(_))) => {
                self.command(index, base("DEL")).await?;
                let mut args = base("SADD");
                args.extend(items(v));
                self.command(index, args).await?;
            }
            ("string" | "none", _) => {
                self.command(
                    index,
                    vec!["SET".into(), key.into(), text, "KEEPTTL".into()],
                )
                .await?;
            }
            (t, _) => {
                return Err(format!(
                    "Edit {t} values as JSON (an object for a hash, an array for a list / set)"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::drivers::live;
    use crate::engine::Engine;

    #[test]
    fn client_list_rows() {
        let rows = super::parse_client_list(
            "id=3 addr=127.0.0.1:5000 name= db=0 age=9 idle=0 cmd=client|list flags=N\n",
        );
        assert_eq!(rows[0]["id"], 3);
        assert_eq!(rows[0]["cmd"], "client|list");
    }

    #[test]
    fn commands_split() {
        assert_eq!(
            super::split_command(r#"SET "a b" 'c d' e\n"#),
            ["SET", "a b", "c d", "e\\n"]
        );
        assert_eq!(
            super::split_command(r#"SET k "x\"y""#),
            ["SET", "k", "x\"y"]
        );
    }

    #[test]
    fn live_redis() {
        if !live::reachable(36379) {
            return;
        }
        let c = live::conn(Engine::Redis, 36379, "", "0");
        let rt = crate::db::runtime();
        let db = rt
            .block_on(crate::drivers::connect(
                &c,
                c.host.clone(),
                c.port,
                String::new(),
            ))
            .unwrap();
        let d = db.driver();
        for cmd in [
            "FLUSHDB",
            "SET user:1 alice@x",
            "HSET user:2 name bob plan pro",
            "RPUSH queue a b",
            "EXPIRE user:1 3600",
        ] {
            rt.block_on(d.exec(cmd.into())).unwrap();
        }
        assert_eq!(
            rt.block_on(d.count("db0".into(), "keys".into(), None))
                .unwrap(),
            3
        );
        let rows = rt
            .block_on(d.window(crate::drivers::WindowReq {
                schema: "db0".into(),
                table: "keys".into(),
                filter: None,
                order_by: None,
                with_key: false,
                limit: 10,
                offset: 0,
            }))
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["key"], "queue");
        assert_eq!(rows[0]["value"], serde_json::json!(["a", "b"]));
        assert_eq!(rows[1]["value"], "alice@x");
        assert!(rows[1]["ttl"].as_i64().unwrap() > 0);
        assert_eq!(rows[2]["value"]["plan"], "pro");
        // Edit, rename-free: value + ttl, then delete — the way the grid saves.
        let st = |sql: &str, p: &[&str]| crate::db::Stmt {
            sql: sql.into(),
            params: p.iter().map(|s| Some(s.to_string())).collect(),
        };
        rt.block_on(d.batch(vec![
            st(
                r#"UPDATE "db0"."keys" SET "value" = ? WHERE "key" = ?"#,
                &["carol@x", "user:1"],
            ),
            st(
                r#"UPDATE "db0"."keys" SET "value" = ? WHERE "key" = ?"#,
                &[r#"{"name":"bob","plan":"free"}"#, "user:2"],
            ),
            st(r#"DELETE FROM "db0"."keys" WHERE "key" = ?"#, &["queue"]),
        ]))
        .unwrap();
        let r = rt.block_on(d.query_rows("GET user:1".into(), 5)).unwrap();
        assert_eq!(r[0]["result"], "carol@x");
        let r = rt
            .block_on(d.query_rows("HGET user:2 plan".into(), 5))
            .unwrap();
        assert_eq!(r[0]["result"], "free");
        // Filter by key prefix.
        let f = crate::db::WhereClause {
            terms: vec![crate::db::FilterTerm {
                column: "key".into(),
                op: crate::filter::FilterOp::HasPrefix,
                value: "user".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            rt.block_on(d.count("db0".into(), "keys".into(), Some(f)))
                .unwrap(),
            2
        );
        rt.block_on(d.exec("FLUSHDB".into())).unwrap();
    }
}
