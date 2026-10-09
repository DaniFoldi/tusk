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

/// The tools, as `tools/list` returns them.
pub fn tools() -> Value {
    json!([
        {
            "name": "open_sql_tab",
            "description": "Write a SQL query into a new query tab in Tusk for the user to review. \
                The query is NOT executed; the user runs it. Use this whenever you give the user SQL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL text." },
                    "title": { "type": "string", "description": "Short tab title (optional)." }
                },
                "required": ["sql"]
            }
        },
        {
            "name": "replace_active_query",
            "description": "Replace the text of the query tab the user has open (or open a new one) \
                with this SQL. Not executed.",
            "inputSchema": {
                "type": "object",
                "properties": { "sql": { "type": "string" } },
                "required": ["sql"]
            }
        },
        {
            "name": "get_active_query",
            "description": "The SQL text of the query tab the user has open, and its selection.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_context",
            "description": "The connection, database, current schema, the schemas, and what the user has open.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_tables",
            "description": "Tables, views and materialized views of a schema (default: the current one).",
            "inputSchema": {
                "type": "object",
                "properties": { "schema": { "type": "string" } }
            }
        },
        {
            "name": "describe_table",
            "description": "The CREATE statement of a table or view: columns, types, defaults, keys, \
                indexes, constraints.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "schema": { "type": "string" }
                },
                "required": ["table"]
            }
        }
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
    /// `<run>/mcp-<pid>.json` — socket + token, how `tusk mcp` finds us.
    instance: PathBuf,
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
        #[cfg(unix)]
        let (socket, listener) = {
            let socket = dir.join(format!("mcp-{}.sock", std::process::id()));
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
        // The instance file (socket + token) lets `tusk mcp` — an MCP
        // server started by *any* client — find this app. It sits beside
        // the socket, same 0600 / 0700 protection, so it widens nothing:
        // only this user could read the socket itself anyway.
        let instance = dir.join(format!("mcp-{}.json", std::process::id()));
        let _ = std::fs::write(
            &instance,
            json!({ "socket": socket.display().to_string(), "token": token }).to_string(),
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
            },
            rx,
        ))
    }

    /// The MCP server entry for `session/new`.
    pub fn server_spec(&self) -> Value {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tusk"));
        json!({
            "name": SERVER_NAME,
            "command": exe.display().to_string(),
            "args": [FLAG, self.socket.display().to_string()],
            "env": [{ "name": TOKEN_ENV, "value": self.token }]
        })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.instance);
    }
}

/// The newest running Tusk's endpoint, for `tusk mcp`: instance files in
/// the run dir, newest first, and the first socket that still answers
/// wins (a crashed Tusk leaves its file behind; the probe skips it).
pub fn discover() -> Option<(PathBuf, String)> {
    discover_in(&crate::acp_registry::data_dir().join("run"))
}

fn discover_in(dir: &Path) -> Option<(PathBuf, String)> {
    let mut cands: Vec<(std::time::SystemTime, PathBuf, String)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("mcp-") && n.ends_with(".json")
        })
        .filter_map(|e| {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            let socket = PathBuf::from(v["socket"].as_str()?);
            let token = v["token"].as_str()?.to_string();
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, socket, token))
        })
        .collect();
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, socket, token) in cands {
        if connect(&socket).is_ok() {
            return Some((socket, token));
        }
    }
    None
}

/// `tusk --mcp-bridge <socket>`: an ACP agent's MCP stdio server, its
/// endpoint handed to the agent in the session spec (token in its env).
pub fn run(socket: &Path) {
    let socket = socket.to_path_buf();
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    serve(move || Some((socket.clone(), token.clone())));
}

/// `tusk mcp`: the same MCP server for any client — Claude Desktop, Claude
/// Code, Cursor, opencode… — without the AI panel. The running app is found
/// per call (newest live instance wins), so app restarts are followed
/// without restarting the client. Calls before Tusk starts answer with a
/// pointer to start it.
pub fn run_standalone() {
    serve(|| discover());
}

/// The MCP stdio loop, relaying each tool call to the app behind `resolve`.
fn serve(mut resolve: impl FnMut() -> Option<(PathBuf, String)>) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut conn: Option<(Stream, BufReader<Stream>, String)> = None;
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
                    Inspect the database with get_context / list_tables / describe_table. \
                    Put SQL for the user into a tab with open_sql_tab; never run it yourself."
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
                let (ok, text) = call(&mut conn, &mut resolve, next, &name, args);
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
    resolve: &mut impl FnMut() -> Option<(PathBuf, String)>,
    id: u64,
    name: &str,
    args: Value,
) -> (bool, String) {
    // One reconnect: the app may have restarted (standalone mode
    // re-discovers, so the fresh instance is picked up).
    for _ in 0..2 {
        if conn.is_none() {
            let Some((socket, token)) = resolve() else {
                return (false, "Tusk is not running — start Tusk first.".into());
            };
            match connect(&socket) {
                Ok(s) => match s.try_clone() {
                    Ok(r) => *conn = Some((s, BufReader::new(r), token)),
                    Err(e) => return (false, e.to_string()),
                },
                Err(_) => return (false, "Tusk is not running — start Tusk first.".into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Bridge → app round trip over the real transport (Unix socket, or
    /// loopback TCP on Windows), the token check, and the instance file
    /// `tusk mcp` discovers. (One Host per test: the socket is named
    /// after the process, so a second parallel start() would collide.)
    #[test]
    fn bridge_reaches_host_and_checks_token() {
        let (host, mut rx) = Host::start().unwrap();
        crate::db::runtime().spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = req.reply.send(Ok(format!("hi {}", req.name)));
            }
        });
        let mut conn = None;
        let mut fixed = {
            let (sock, tok) = (host.socket.clone(), host.token.clone());
            move || Some((sock.clone(), tok.clone()))
        };
        let (ok, text) = call(&mut conn, &mut fixed, 1, "ping", json!({}));
        assert!(ok, "{text}");
        assert_eq!(text, "hi ping");
        let mut conn = None;
        let mut wrong = {
            let sock = host.socket.clone();
            move || Some((sock.clone(), "wrong".to_string()))
        };
        let (ok, _) = call(&mut conn, &mut wrong, 2, "ping", json!({}));
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

    /// `tusk mcp` discovery: the newest *live* instance wins — a crashed
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
                json!({ "socket": socket, "token": format!("{name}-tok") }).to_string(),
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

        let found = discover_in(&dir).expect("the live instance must be found");
        assert_eq!(found.0, live);
        assert_eq!(found.1, "mcp-111.json-tok");

        drop(listener);
        assert!(discover_in(&dir).is_none(), "no live socket, no instance");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovery_in_an_empty_dir_finds_nothing() {
        let dir = std::env::temp_dir().join(format!("tusk-mcp-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(discover_in(&dir).is_none());
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
        for tool in t.as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }
}
