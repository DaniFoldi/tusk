//! Tusk's tools for AI agents, as an MCP server.
//!
//! Agents start MCP servers themselves, so the server is this binary run as
//! `tusk --mcp-bridge <socket>` (listed in `session/new`). It speaks MCP on
//! stdio and relays each tool call over a Unix socket to the running app
//! ([`Host`]), which answers from the live connection and the open tabs.
//! Nothing here runs the user's SQL: queries go into a tab for the user to
//! review and run.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};

pub const FLAG: &str = "--mcp-bridge";
pub const SERVER_NAME: &str = "tusk";
/// The bridge's secret travels in its environment (not argv, which `ps` shows).
const TOKEN_ENV: &str = "TUSK_MCP_TOKEN";
/// The ACP-side bridge's own connection name (set in its env by
/// `server_spec`; the catalog shows it for `list_connections`).
const CONNECTION_ENV: &str = "TUSK_MCP_CONNECTION";

/// The tools, as `tools/list` returns them. Every tool takes an optional
/// `connection` — which running Tusk window to talk to (the
/// saved-connection name); without it, the most recently active one
/// answers. `list_connections` shows the options.
pub fn tools() -> Value {
    let connection = || {
        json!({
            "type": "string",
            "description": "The saved-connection name of the Tusk window to talk to \
                (optional; the most recently active window when omitted). \
                list_connections shows every live one."
        })
    };
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        let mut props = properties.as_object().cloned().unwrap_or_default();
        props.insert("connection".into(), connection());
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required
            }
        })
    };
    json!([
        tool(
            "open_sql_tab",
            "Write a SQL query into a new query tab in Tusk for the user to review. \
              The query is NOT executed; the user runs it. Use this whenever you give the user SQL.",
            json!({
                "sql": { "type": "string", "description": "The SQL text." },
                "title": { "type": "string", "description": "Short tab title (optional)." }
            }),
            &["sql"]
        ),
        tool(
            "replace_active_query",
            "Replace the text of the query tab the user has open (or open a new one) \
              with this SQL. Not executed.",
            json!({ "sql": { "type": "string" } }),
            &["sql"]
        ),
        tool(
            "get_active_query",
            "The SQL text of the query tab the user has open, and its selection.",
            json!({}),
            &[]
        ),
        tool(
            "run_sql",
            "Run SQL and return its rows as TSV (or rows-affected for writes). \
              Only works when the user's settings allow it: refused by default; Read-only \
              mode runs single SELECT-type statements only. When refused or unsure, use \
              open_sql_tab instead — the user reviews and runs it.",
            json!({
                "sql": { "type": "string", "description": "One SQL statement." },
                "limit": {
                    "type": "integer",
                    "description": "Max rows returned (default 100, cap 500)."
                }
            }),
            &["sql"]
        ),
        tool(
            "get_context",
            "The connection, database, current schema, the schemas, and what the user has open.",
            json!({}),
            &[]
        ),
        tool(
            "list_tables",
            "Tables, views and materialized views of a schema (default: the current one).",
            json!({ "schema": { "type": "string" } }),
            &[]
        ),
        tool(
            "describe_table",
            "The CREATE statement of a table or view: columns, types, defaults, keys, \
              indexes, constraints.",
            json!({
                "table": { "type": "string" },
                "schema": { "type": "string" }
            }),
            &["table"]
        ),
        tool(
            "list_connections",
            "Every running Tusk window: its connection name, database and engine — the \
              `connection` values the other tools accept. No window needed to answer.",
            json!({}),
            &[]
        )
    ])
}

/// A tool call from an agent, for the app to answer.
pub struct ToolRequest {
    pub name: String,
    pub args: Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// The bridge's end of the connection to the app: a Unix socket, or on
/// Windows a loopback TCP port (the per-launch token keeps others out).
#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::net::TcpStream;

fn connect(socket: &Path) -> std::io::Result<Stream> {
    #[cfg(unix)]
    return Stream::connect(socket);
    #[cfg(windows)]
    return Stream::connect(socket.to_string_lossy().as_ref());
}

/// The app side: a Unix socket (loopback TCP on Windows) the bridges connect to.
pub struct Host {
    pub socket: PathBuf,
    /// Per-launch secret the bridge must send with every call.
    token: String,
    /// `<run>/mcp-<pid>-<seq>.json` — socket + token + identity, how
    /// `tusk mcp` finds us and tells windows apart.
    instance: PathBuf,
    /// What this window is connected to: connection name, database,
    /// engine label (`null` connection = the welcome screen).
    identity: Value,
}

/// Per-host sequence: several windows of one process each get their own
/// socket and instance file (`mcp-<pid>-<seq>…`).
static HOST_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The identity half of an instance file — everything but the endpoint.
fn identity_value(connection: Option<&str>, database: &str, engine: &str) -> Value {
    json!({
        "connection": connection,
        "database": database,
        "engine": engine,
    })
}

/// 128 random bits as hex.
fn random_token() -> String {
    use std::io::Read as _;
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
        && rsa::rand_core::RngCore::try_fill_bytes(&mut rsa::rand_core::OsRng, &mut b).is_err()
    {
        // Fall back to time + pid (still unguessable enough for a 0600 socket).
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            ^ u128::from(std::process::id());
        b = n.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Host {
    pub fn start() -> Result<(Self, mpsc::UnboundedReceiver<ToolRequest>)> {
        // A private (0700) directory, the socket itself 0600, and a secret
        // per launch: only this user's bridge started by us gets in.
        let dir = crate::acp_registry::data_dir().join("run");
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let _rt = crate::db::runtime().enter();
        let seq = HOST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(unix)]
        let (socket, listener) = {
            let socket = if seq == 0 {
                dir.join(format!("mcp-{}.sock", std::process::id()))
            } else {
                dir.join(format!("mcp-{}-{seq}.sock", std::process::id()))
            };
            let _ = std::fs::remove_file(&socket);
            let listener = tokio::net::UnixListener::bind(&socket)?;
            (socket, listener)
        };
        #[cfg(windows)]
        let (socket, listener) = {
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
            listener.set_nonblocking(true)?;
            let socket = PathBuf::from(listener.local_addr()?.to_string());
            (socket, tokio::net::TcpListener::from_std(listener)?)
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        }
        let token = random_token();
        // The instance file (socket + token + identity) lets `tusk mcp` — an
        // MCP server started by *any* client — find this window, and tell it
        // apart from the others. It sits beside the socket, same 0600 / 0700
        // protection, so it widens nothing: only this user could read the
        // socket itself anyway.
        let instance = if seq == 0 {
            dir.join(format!("mcp-{}.json", std::process::id()))
        } else {
            dir.join(format!("mcp-{}-{seq}.json", std::process::id()))
        };
        let identity = identity_value(None, "", "");
        let _ = std::fs::write(
            &instance,
            json!({
                "socket": socket.display().to_string(),
                "token": token,
                "connection": identity["connection"],
                "database": identity["database"],
                "engine": identity["engine"],
            })
            .to_string(),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&instance, std::fs::Permissions::from_mode(0o600));
        }
        let expected = token.clone();
        let (tx, rx) = mpsc::unbounded_channel::<ToolRequest>();
        crate::db::runtime().spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                let expected = expected.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if msg["token"].as_str() != Some(expected.as_str()) {
                            return; // not our bridge: drop the connection
                        }
                        let (reply, rx) = oneshot::channel();
                        let _ = tx.send(ToolRequest {
                            name: msg["tool"].as_str().unwrap_or_default().to_string(),
                            args: msg.get("args").cloned().unwrap_or(json!({})),
                            reply,
                        });
                        let res = rx.await.unwrap_or_else(|_| Err("Tusk closed".into()));
                        let out = match res {
                            Ok(text) => json!({"id": msg["id"], "ok": true, "text": text}),
                            Err(text) => json!({"id": msg["id"], "ok": false, "text": text}),
                        };
                        let mut s = out.to_string();
                        s.push('\n');
                        if write.write_all(s.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Ok((
            Self {
                socket,
                token,
                instance,
                identity,
            },
            rx,
        ))
    }

    /// What this window is connected to. Called on connect, database
    /// switch and disconnect, so the catalog `tusk mcp` serves is always
    /// current. Best-effort: a failed rewrite keeps the last identity.
    pub fn set_connection(&mut self, connection: Option<&str>, database: &str, engine: &str) {
        self.identity = identity_value(connection, database, engine);
        let _ = std::fs::write(
            &self.instance,
            json!({
                "socket": self.socket.display().to_string(),
                "token": self.token,
                "connection": self.identity["connection"],
                "database": self.identity["database"],
                "engine": self.identity["engine"],
            })
            .to_string(),
        );
    }

    /// The MCP server entry for `session/new`. The connection name rides
    /// along in the env so the ACP-side bridge knows its own identity
    /// (a second window runs its own bridge with its own name).
    pub fn server_spec(&self) -> Value {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tusk"));
        let mut env = vec![json!({ "name": TOKEN_ENV, "value": self.token })];
        if let Some(name) = self.identity["connection"].as_str() {
            env.push(json!({ "name": CONNECTION_ENV, "value": name }));
        }
        json!({
            "name": SERVER_NAME,
            "command": exe.display().to_string(),
            "args": [FLAG, self.socket.display().to_string()],
            "env": env
        })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.instance);
    }
}

/// One live Tusk window as `tusk mcp` sees it: the endpoint plus what
/// that window is connected to.
#[derive(Clone, Debug)]
struct Instance {
    socket: PathBuf,
    token: String,
    /// Saved-connection name; `None` = welcome screen.
    connection: Option<String>,
    database: String,
    engine: String,
}

impl Instance {
    /// "Titanium" or "Titanium · cleverev-prod · PostgreSQL" — how the
    /// catalog and error messages name a window.
    fn label(&self) -> String {
        match &self.connection {
            Some(c) if !self.database.is_empty() => {
                format!("{c} · {} ({})", self.database, self.engine)
            }
            Some(c) => c.clone(),
            None => "Tusk (no connection)".to_string(),
        }
    }
}

/// Every live Tusk window, newest first — the catalog `tusk mcp` serves
/// and routes over. Instance files in the run dir (written by each
/// window's Host) are probed: a crashed Tusk leaves its file behind and
/// is skipped.
fn discover_all_in(dir: &Path) -> Vec<Instance> {
    let mut cands: Vec<(std::time::SystemTime, Instance)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        cands.extend(
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.starts_with("mcp-") && n.ends_with(".json")
                })
                .filter_map(|e| {
                    let v: Value =
                        serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
                    let socket = PathBuf::from(v["socket"].as_str()?);
                    let token = v["token"].as_str()?.to_string();
                    let connection = v["connection"].as_str().map(str::to_string);
                    let database = v["database"].as_str().unwrap_or_default().to_string();
                    let engine = v["engine"].as_str().unwrap_or_default().to_string();
                    let mtime = e.metadata().ok()?.modified().ok()?;
                    Some((
                        mtime,
                        Instance {
                            socket,
                            token,
                            connection,
                            database,
                            engine,
                        },
                    ))
                }),
        );
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    cands
        .into_iter()
        .map(|(_, i)| i)
        .filter(|i| connect(&i.socket).is_ok())
        .collect()
}

/// The newest running Tusk's endpoint, for `tusk mcp` (the default when
/// a tool call doesn't name a connection).
pub fn discover() -> Option<(PathBuf, String)> {
    let i = catalog().into_iter().next()?;
    Some((i.socket, i.token))
}

/// Every live Tusk window this user is running, newest first.
fn catalog() -> Vec<Instance> {
    discover_all_in(&crate::acp_registry::data_dir().join("run"))
}

/// The window a tool call wants: by connection name (case-insensitive,
/// then a database-name match), or the newest when it doesn't say.
fn pick<'a>(instances: &'a [Instance], want: Option<&str>) -> Result<&'a Instance, String> {
    let Some(want) = want else {
        return instances
            .first()
            .ok_or_else(|| "Tusk is not running — start Tusk first.".to_string());
    };
    let w = want.trim().to_lowercase();
    instances
        .iter()
        .find(|i| {
            i.connection
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(&w))
        })
        .or_else(|| {
            instances
                .iter()
                .find(|i| i.database.eq_ignore_ascii_case(&w))
        })
        .ok_or_else(|| {
            let names: Vec<String> = instances.iter().map(|i| i.label()).collect();
            if names.is_empty() {
                "Tusk is not running — start Tusk first.".to_string()
            } else {
                format!(
                    "No running Tusk is connected as \"{want}\". Live: {}",
                    names.join(" | ")
                )
            }
        })
}

/// `tusk --mcp-bridge <socket>`: an ACP agent's MCP stdio server, its
/// endpoint handed to the agent in the session spec (token in its env,
/// connection name so `list_connections` can name it).
pub fn run(socket: &Path) {
    let socket = socket.to_path_buf();
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let connection = std::env::var(CONNECTION_ENV).ok().filter(|c| !c.is_empty());
    serve(move || {
        vec![Instance {
            socket: socket.clone(),
            token: token.clone(),
            connection: connection.clone(),
            database: String::new(),
            engine: String::new(),
        }]
    });
}

/// `tusk mcp`: the same MCP server for any client — Claude Desktop, Claude
/// Code, Cursor, opencode… — without the AI panel. Every running Tusk
/// window is served at once: tools take a `connection` argument (the
/// saved-connection name), and without it the most recently active window
/// answers. The catalog is re-read per call, so app restarts — and new
/// windows — are followed without restarting the client.
pub fn run_standalone() {
    serve(catalog);
}

/// What run_sql may do with [`AgentRun`] permissions. Read-only is the
/// careful one: only a single statement, only SQL engines (the NoSQL
/// classifiers pass everything through), only statements that read — and
/// the body scan catches what the first keyword can't (data-modifying
/// CTEs, `SELECT … INTO`, assigning PRAGMAs), refusing rather than
/// risking a false pass.
pub fn agent_may_run(
    perm: crate::settings::AgentRun,
    engine: crate::engine::Engine,
    sql: &str,
) -> Result<(), String> {
    use crate::settings::AgentRun;
    match perm {
        AgentRun::Never => Err(
            "Tusk's settings don't let agents run queries (Settings → AI → Agents Can Run \
             Queries). Put the SQL into a tab with open_sql_tab — the user runs it."
                .into(),
        ),
        AgentRun::ReadOnly => {
            if !engine.caps().sql {
                return Err(format!(
                    "Read-only mode can't classify {} statements — it only runs SQL.",
                    engine.label()
                ));
            }
            if sql.split(';').filter(|s| !s.trim().is_empty()).count() > 1 {
                return Err("Read-only mode runs one statement at a time.".into());
            }
            if crate::db::classify_statement(sql) != crate::db::StmtKind::Query {
                return Err(
                    "Read-only mode refuses this statement: it only runs SELECT-type \
                     queries. Use open_sql_tab for the rest."
                        .into(),
                );
            }
            let body = sql.to_ascii_uppercase();
            // Word-boundary scan, so `OFFSET` is not `SET` and `POINT` is
            // not `INTO`; SET / SET_CONFIG close the GUC escape (a RO
            // transaction is the real gate — this is the backstop).
            const WRITES: [&str; 8] = [
                "INSERT",
                "UPDATE",
                "DELETE",
                "MERGE",
                "INTO",
                "PRAGMA",
                "SET",
                "SET_CONFIG",
            ];
            if WRITES.iter().any(|w| has_word(&body, w)) {
                return Err(
                    "Read-only mode refuses this statement: it writes or changes settings. \
                     Use open_sql_tab for the rest."
                        .into(),
                );
            }
            Ok(())
        }
        AgentRun::All => Ok(()),
    }
}

/// `hay` (already uppercased) contains `word` (ASCII) as a whole word —
/// `OFFSET` is not `SET`, `CURRENT_SETTING` is not `SETTING`. A word
/// glued to non-ASCII letters (caféSET) counts as a match: refusing more
/// is safe, refusing less isn't.
fn has_word(hay: &str, word: &str) -> bool {
    let hay = hay.as_bytes();
    let word = word.as_bytes();
    let boundary = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    hay.windows(word.len()).enumerate().any(|(i, w)| {
        w == word
            && (i == 0 || !boundary(hay[i - 1]))
            && (i + word.len() >= hay.len() || !boundary(hay[i + word.len()]))
    })
}

/// The MCP stdio loop, relaying each tool call to a Tusk window picked
/// from the catalog `resolve` returns.
fn serve(resolve: impl FnMut() -> Vec<Instance> + 'static) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut conn: Option<(Stream, BufReader<Stream>, String)> = None;
    // The endpoint `conn` is open to — a different target forces a
    // reconnect.
    let mut conn_key: Option<(PathBuf, String)> = None;
    let mut resolve = resolve;
    let mut next = 0u64;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notifications (initialized, cancelled)
        };
        let method = msg["method"].as_str().unwrap_or_default();
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "title": "Tusk", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Tools of Tusk, the database client the user is working in. \
                    Several Tusk windows may be running; list_connections shows them, and the \
                    optional `connection` argument of each tool picks one (without it, the most \
                    recently active window answers). Inspect the database with get_context / \
                    list_tables / describe_table. Put SQL for the user into a tab with \
                    open_sql_tab — they review and run it. run_sql only works if the user's \
                    settings allow it (never by default, read-only by choice); when refused, \
                    fall back to open_sql_tab."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => {
                next += 1;
                let name = msg["params"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                let (ok, text) = call(&mut conn, &mut conn_key, &mut resolve, next, &name, args);
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": !ok }))
            }
            _ => Err((-32601, format!("unknown method {method}"))),
        };
        let out = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err((code, m)) => json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":m}}),
        };
        if writeln!(stdout, "{out}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

fn call(
    conn: &mut Option<(Stream, BufReader<Stream>, String)>,
    conn_key: &mut Option<(PathBuf, String)>,
    resolve: &mut impl FnMut() -> Vec<Instance>,
    id: u64,
    name: &str,
    args: Value,
) -> (bool, String) {
    // The catalog is live per call: new windows appear, closed ones
    // drop out, identities follow connect / disconnect.
    let instances = resolve();
    // list_connections is answered by the bridge itself — it *is* the
    // catalog. (No Tusk window needed, though an empty one tells the
    // agent just as much.)
    if name == "list_connections" {
        return (true, catalog_text(&instances));
    }
    let want = args["connection"].as_str().map(|s| s.to_string());
    let target = match pick(&instances, want.as_deref()) {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    let key = (target.socket.clone(), target.token.clone());
    if *conn_key != Some(key.clone()) {
        *conn = None;
    }
    // One reconnect: the chosen window may have restarted between calls.
    for _ in 0..2 {
        if conn.is_none() {
            match connect(&target.socket) {
                Ok(s) => match s.try_clone() {
                    Ok(r) => {
                        *conn = Some((s, BufReader::new(r), target.token.clone()));
                        *conn_key = Some(key.clone());
                    }
                    Err(e) => return (false, e.to_string()),
                },
                Err(_) => {
                    *conn_key = None;
                    return (false, "Tusk is not running — start Tusk first.".into());
                }
            }
        }
        let Some((w, r, token)) = conn.as_mut() else {
            continue;
        };
        let req = json!({"id": id, "token": token, "tool": name, "args": args});
        if writeln!(w, "{req}").is_err() {
            *conn = None;
            continue;
        }
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(n) if n > 0 => {
                let v: Value = serde_json::from_str(&line).unwrap_or(json!({}));
                return (
                    v["ok"].as_bool().unwrap_or(false),
                    v["text"].as_str().unwrap_or_default().to_string(),
                );
            }
            _ => *conn = None,
        }
    }
    (false, "Lost the connection to Tusk.".into())
}

/// `list_connections`: one line per live window, newest first.
fn catalog_text(instances: &[Instance]) -> String {
    if instances.is_empty() {
        return "No Tusk window is running — start Tusk first.".to_string();
    }
    let mut s = format!("{} running Tusk window(s):\n", instances.len());
    for (i, inst) in instances.iter().enumerate() {
        s.push_str(&format!("{}. {}\n", i + 1, inst.label()));
    }
    s.push_str(
        "Pass the connection name as the `connection` argument to any tool; \
         without it the most recently active window answers.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bridge → app round trip over the real transport (Unix socket, or
    /// loopback TCP on Windows), the token check, and the instance file
    /// `tusk mcp` discovers. (Hosts are pid-<seq> named, so parallel
    /// starts don't collide.)
    #[test]
    fn bridge_reaches_host_and_checks_token() {
        let (host, mut rx) = Host::start().unwrap();
        crate::db::runtime().spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.reply.send(Ok(format!("hi {}", req.name)));
            }
        });
        let mut fixed = {
            let (sock, tok) = (host.socket.clone(), host.token.clone());
            move || {
                vec![Instance {
                    socket: sock.clone(),
                    token: tok.clone(),
                    connection: None,
                    database: String::new(),
                    engine: String::new(),
                }]
            }
        };
        let mut conn = None;
        let mut key = None;
        let (ok, text) = call(&mut conn, &mut key, &mut fixed, 1, "ping", json!({}));
        assert!(ok, "{text}");
        assert_eq!(text, "hi ping");
        let mut conn = None;
        let mut wrong = {
            let sock = host.socket.clone();
            move || {
                vec![Instance {
                    socket: sock.clone(),
                    token: "wrong".to_string(),
                    connection: None,
                    database: String::new(),
                    engine: String::new(),
                }]
            }
        };
        let (ok, _) = call(&mut conn, &mut key, &mut wrong, 2, "ping", json!({}));
        assert!(!ok, "a wrong token must be refused");
        // The published instance file matches the host, and dies with it.
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&host.instance).unwrap()).unwrap();
        assert_eq!(
            v["socket"].as_str().unwrap(),
            host.socket.display().to_string()
        );
        assert_eq!(v["token"].as_str().unwrap(), host.token);
        let instance = host.instance.clone();
        drop(host);
        assert!(!instance.exists(), "Drop removes the instance file");
    }

    /// set_connection rewrites the instance file's identity — that's how
    /// `tusk mcp` tells windows apart and names them to agents.
    #[test]
    fn set_connection_publishes_identity() {
        let (mut host, mut rx) = Host::start().unwrap();
        crate::db::runtime().spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.reply.send(Ok(String::new()));
            }
        });
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&host.instance).unwrap()).unwrap();
        assert!(v["connection"].is_null(), "starts as no-connection");
        host.set_connection(Some("Titanium"), "cleverev-prod", "PostgreSQL");
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&host.instance).unwrap()).unwrap();
        assert_eq!(v["connection"], "Titanium");
        assert_eq!(v["database"], "cleverev-prod");
        assert_eq!(v["engine"], "PostgreSQL");
        host.set_connection(None, "", "");
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(&host.instance).unwrap()).unwrap();
        assert!(v["connection"].is_null(), "disconnect clears it");
    }

    /// Two live windows in one catalog: the `connection` argument picks
    /// which one answers (case-insensitive), no argument picks the most
    /// recently touched, and an unknown name lists what's live.
    #[test]
    fn two_windows_route_by_connection() {
        let (mut alpha, mut rx_a) = Host::start().unwrap();
        let (mut beta, mut rx_b) = Host::start().unwrap();
        alpha.set_connection(Some("Alpha"), "db-a", "PostgreSQL");
        beta.set_connection(Some("Beta"), "db-b", "MySQL");
        crate::db::runtime().spawn(async move {
            while let Some(req) = rx_a.recv().await {
                let _ = req.reply.send(Ok("alpha".to_string()));
            }
        });
        crate::db::runtime().spawn(async move {
            while let Some(req) = rx_b.recv().await {
                let _ = req.reply.send(Ok("beta".to_string()));
            }
        });
        // The catalog as `tusk mcp` would read it from the run dir —
        // hand-built here so the test doesn't see the user's real windows.
        let inst = |h: &Host, connection: &str, database: &str, engine: &str| Instance {
            socket: h.socket.clone(),
            token: h.token.clone(),
            connection: Some(connection.to_string()),
            database: database.to_string(),
            engine: engine.to_string(),
        };
        let a = inst(&alpha, "Alpha", "db-a", "PostgreSQL");
        let b = inst(&beta, "Beta", "db-b", "MySQL");
        let mut catalog = move || vec![b.clone(), a.clone()];

        let mut conn = None;
        let mut key = None;
        let (ok, text) = call(
            &mut conn,
            &mut key,
            &mut catalog,
            1,
            "get_context",
            json!({ "connection": "alpha" }),
        );
        assert!(ok && text == "alpha", "routing is case-insensitive: {text}");
        let (ok, text) = call(
            &mut conn,
            &mut key,
            &mut catalog,
            2,
            "get_context",
            json!({ "connection": "Beta" }),
        );
        assert!(ok && text == "beta", "the second window answers: {text}");
        let (ok, text) = call(
            &mut conn,
            &mut key,
            &mut catalog,
            3,
            "get_context",
            json!({ "connection": "Ghost" }),
        );
        assert!(!ok, "an unknown connection is an error");
        assert!(
            text.contains("Alpha") && text.contains("Beta"),
            "the error lists the live windows: {text}"
        );
        drop((alpha, beta));
    }

    /// run_sql's permission gate, engine by engine and hole by hole.
    #[test]
    fn agent_may_run_enforces_the_modes() {
        use crate::engine::Engine;
        use crate::settings::AgentRun as R;
        let pg = Engine::Postgres;
        // Never refuses everything.
        assert!(super::agent_may_run(R::Never, pg, "SELECT 1").is_err());
        // Read-only passes a plain SELECT.
        assert!(super::agent_may_run(R::ReadOnly, pg, "SELECT 1").is_ok());
        assert!(
            super::agent_may_run(R::ReadOnly, pg, "select id, name from users where id = 7")
                .is_ok()
        );
        assert!(super::agent_may_run(R::ReadOnly, pg, "EXPLAIN SELECT 1").is_ok());
        // …and refuses writes at the first keyword…
        assert!(super::agent_may_run(R::ReadOnly, pg, "UPDATE users SET a = 1").is_err());
        assert!(super::agent_may_run(R::ReadOnly, pg, "TRUNCATE users").is_err());
        // …writes hidden behind a SELECT-shape statement…
        assert!(
            super::agent_may_run(
                R::ReadOnly,
                pg,
                "WITH d AS (DELETE FROM users RETURNING *) SELECT * FROM d"
            )
            .is_err()
        );
        assert!(super::agent_may_run(R::ReadOnly, pg, "SELECT * INTO tmp FROM users").is_err());
        // …the GUC escape: flipping read-only settings from inside a SELECT.
        assert!(
            super::agent_may_run(
                R::ReadOnly,
                pg,
                "SELECT set_config('default_transaction_read_only', 'off', false)"
            )
            .is_err()
        );
        // Word boundaries: OFFSET ≠ SET, CURRENT_SETTING ≠ SET, a table
        // named `settings` is fine — but a CTE named `set` is refused.
        assert!(super::agent_may_run(R::ReadOnly, pg, "SELECT * FROM t OFFSET 10").is_ok());
        assert!(
            super::agent_may_run(R::ReadOnly, pg, "SELECT current_setting('work_mem') AS m")
                .is_ok()
        );
        assert!(super::agent_may_run(R::ReadOnly, pg, "SELECT * FROM settings").is_ok());
        assert!(
            super::agent_may_run(R::ReadOnly, pg, "WITH set AS (SELECT 1) SELECT * FROM set")
                .is_err()
        );
        // …multi-statement smuggling (the trailing `;` is fine)…
        assert!(super::agent_may_run(R::ReadOnly, pg, "SELECT 1; DROP TABLE users").is_err());
        assert!(super::agent_may_run(R::ReadOnly, pg, "SELECT 1;").is_ok());
        // …and non-SQL engines, whose classifiers pass everything through.
        assert!(super::agent_may_run(R::ReadOnly, Engine::Redis, "GET k").is_err());
        assert!(super::agent_may_run(R::ReadOnly, Engine::MongoDb, "find").is_err());
        // All runs anything, on any engine.
        assert!(super::agent_may_run(R::All, pg, "DROP TABLE users").is_ok());
        assert!(super::agent_may_run(R::All, Engine::Redis, "FLUSHALL").is_ok());
    }

    /// `tusk mcp` discovery: only *live* instances are served — a crashed
    /// Tusk leaves a newer file behind, and its dead socket is skipped.
    #[cfg(unix)]
    #[test]
    fn discovery_skips_dead_instances() {
        let dir = std::env::temp_dir().join(format!("tusk-mcp-disc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live.sock");
        let _ = std::fs::remove_file(&live);
        let listener = std::os::unix::net::UnixListener::bind(&live).unwrap();

        let write = |name: &str, socket: &str, mtime: std::time::SystemTime| {
            let path = dir.join(name);
            std::fs::write(
                &path,
                json!({
                    "socket": socket,
                    "token": format!("{name}-tok"),
                    "connection": null,
                    "database": "",
                    "engine": ""
                })
                .to_string(),
            )
            .unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        };
        let now = std::time::SystemTime::now();
        // Older file → the live socket; newer file → a gone socket.
        write(
            "mcp-111.json",
            &live.display().to_string(),
            now - std::time::Duration::from_secs(60),
        );
        write(
            "mcp-222.json",
            &dir.join("gone.sock").display().to_string(),
            now,
        );

        let found = discover_all_in(&dir);
        assert_eq!(found.len(), 1, "only the live one: {found:?}");
        assert_eq!(found[0].socket, live);
        assert_eq!(found[0].token, "mcp-111.json-tok");
        assert!(found[0].connection.is_none());

        drop(listener);
        assert!(
            discover_all_in(&dir).is_empty(),
            "no live socket, no instance"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_in_an_empty_dir_finds_nothing() {
        let dir = std::env::temp_dir().join(format!("tusk-mcp-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(discover_all_in(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tool_list_is_well_formed() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"open_sql_tab"));
        assert!(names.contains(&"list_connections"));
        for tool in t.as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(
                tool["inputSchema"]["properties"]["connection"].is_object(),
                "{} must accept a connection argument",
                tool["name"]
            );
        }
    }
}
