//! "Import from Docker Compose": read a `compose.yaml` (plus its override
//! file, `.env` and `env_file`s), pick out the database services Tusk can
//! talk to (by image), and turn each into a profile — host port from the
//! published `ports`, user / password / database from the image's
//! environment variables. All of them land in one folder named after the
//! Compose project. Reviewed and imported through the migrate sheet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::db::{self, ConnTag, SavedConnection, SslMode};
use crate::engine::Engine;
use crate::migrate::{Plan, Source};

/// File names `docker compose` looks for, in its order of preference.
const FILE_NAMES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// The compose file for a picked path (a directory: the file inside it).
pub fn find_file(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    FILE_NAMES
        .iter()
        .map(|n| path.join(n))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no compose.yaml / docker-compose.yml in {}", path.display()))
}

/// `compose.yaml` → `compose.override.yaml` (merged on top, like Compose).
fn override_file(file: &Path) -> Option<PathBuf> {
    let stem = file.file_stem()?.to_str()?;
    let ext = file.extension()?.to_str()?;
    let p = file.with_file_name(format!("{stem}.override.{ext}"));
    p.is_file().then_some(p)
}

fn read_yaml(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut y: serde_norway::Value =
        serde_norway::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    // `<<: *defaults` merge keys.
    y.apply_merge().map_err(|e| e.to_string())?;
    serde_json::to_value(y).map_err(|e| e.to_string())
}

/// Compose-style merge: maps recursively, lists appended, scalars replaced.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(b), Value::Array(o)) => b.extend(o),
        (b, o) => *b = o,
    }
}

/// `KEY=value` lines of a dotenv file (comments, `export`, quotes handled).
fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = if v.len() >= 2
                && ((v.starts_with('"') && v.ends_with('"'))
                    || (v.starts_with('\'') && v.ends_with('\'')))
            {
                v[1..v.len() - 1].to_string()
            } else {
                // Unquoted: a ` #` starts a comment.
                v.split(" #").next().unwrap_or("").trim().to_string()
            };
            Some((k.trim().to_string(), v))
        })
        .collect()
}

/// `${VAR}`, `${VAR:-default}`, `${VAR-default}`, `${VAR:+alt}`,
/// `${VAR:?err}`, `$VAR` and `$$` against `vars`.
fn interpolate(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match b.get(i + 1) {
            Some(b'$') => {
                out.push('$');
                i += 2;
            }
            Some(b'{') => {
                // Find the matching brace (defaults may nest `${…}`).
                let mut depth = 0;
                let mut end = None;
                for (j, c) in s[i + 1..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(i + 1 + j);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(end) = end else {
                    out.push_str(&s[i..]);
                    break;
                };
                out.push_str(&expand(&s[i + 2..end], vars));
                i = end + 1;
            }
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                let name_end = s[i + 1..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .map_or(s.len(), |n| i + 1 + n);
                out.push_str(vars.get(&s[i + 1..name_end]).map_or("", String::as_str));
                i = name_end;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

/// The inside of one `${…}`.
fn expand(expr: &str, vars: &HashMap<String, String>) -> String {
    let name_end = expr
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(expr.len());
    let (name, rest) = expr.split_at(name_end);
    let val = vars.get(name);
    let set = val.is_some_and(|v| !v.is_empty());
    let (op, arg) = if let Some(a) = rest.strip_prefix(":-") {
        (":-", a)
    } else if let Some(a) = rest.strip_prefix(":+") {
        (":+", a)
    } else if let Some(a) = rest.strip_prefix(":?") {
        (":?", a)
    } else if let Some(a) = rest.strip_prefix('-') {
        ("-", a)
    } else if let Some(a) = rest.strip_prefix('+') {
        ("+", a)
    } else if let Some(a) = rest.strip_prefix('?') {
        ("?", a)
    } else {
        ("", "")
    };
    let current = val.cloned().unwrap_or_default();
    match op {
        ":-" if !set => interpolate(arg, vars),
        "-" if val.is_none() => interpolate(arg, vars),
        ":+" => {
            if set {
                interpolate(arg, vars)
            } else {
                String::new()
            }
        }
        "+" => {
            if val.is_some() {
                interpolate(arg, vars)
            } else {
                String::new()
            }
        }
        _ => current,
    }
}

/// Interpolate every string in the tree (keys stay as written).
fn interpolate_tree(v: &mut Value, vars: &HashMap<String, String>) {
    match v {
        Value::String(s) => *s = interpolate(s, vars),
        Value::Array(a) => a.iter_mut().for_each(|x| interpolate_tree(x, vars)),
        Value::Object(o) => o.values_mut().for_each(|x| interpolate_tree(x, vars)),
        _ => {}
    }
}

/// A scalar as text (`5432`, `true`, `"x"`); null / maps → `None`.
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The container's environment: `env_file`s, then `environment` (map or
/// `KEY=value` list; a bare `KEY` takes the value from `vars`).
fn service_env(svc: &Value, dir: &Path, vars: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let files: Vec<&Value> = match svc.get("env_file") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) => vec![v],
        None => Vec::new(),
    };
    for f in files {
        let path = match f {
            Value::String(s) => s.clone(),
            Value::Object(o) => o.get("path").and_then(scalar).unwrap_or_default(),
            _ => continue,
        };
        if let Ok(text) = std::fs::read_to_string(dir.join(&path)) {
            env.extend(parse_env_file(&text));
        }
    }
    match svc.get("environment") {
        Some(Value::Object(o)) => {
            for (k, v) in o {
                let v = scalar(v).or_else(|| vars.get(k).cloned());
                if let Some(v) = v {
                    env.insert(k.clone(), v);
                }
            }
        }
        Some(Value::Array(a)) => {
            for item in a.iter().filter_map(Value::as_str) {
                match item.split_once('=') {
                    Some((k, v)) => {
                        env.insert(k.to_string(), v.to_string());
                    }
                    None => {
                        if let Some(v) = vars.get(item) {
                            env.insert(item.to_string(), v.clone());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    env
}

/// One published port: host interface, host port, container port.
#[derive(Debug, PartialEq)]
struct Port {
    host_ip: Option<String>,
    published: Option<u16>,
    target: u16,
}

/// `a-b` → (a, b); `a` → (a, a).
fn range(s: &str) -> Option<(u16, u16)> {
    match s.split_once('-') {
        Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
        None => {
            let p = s.trim().parse().ok()?;
            Some((p, p))
        }
    }
}

/// Short (`"127.0.0.1:15432:5432/tcp"`) and long (`{target, published}`)
/// port syntax; ranges expand to one entry per port. UDP is dropped.
fn parse_ports(v: &Value) -> Vec<Port> {
    let mut out = Vec::new();
    for item in v.as_array().into_iter().flatten() {
        match item {
            Value::Object(o) => {
                if o.get("protocol")
                    .and_then(Value::as_str)
                    .is_some_and(|p| p != "tcp")
                {
                    continue;
                }
                let Some(target) = o
                    .get("target")
                    .and_then(scalar)
                    .and_then(|t| t.parse().ok())
                else {
                    continue;
                };
                let published = o
                    .get("published")
                    .and_then(scalar)
                    .and_then(|p| range(&p))
                    .map(|r| r.0);
                let host_ip = o.get("host_ip").and_then(scalar);
                out.push(Port {
                    host_ip,
                    published,
                    target,
                });
            }
            other => {
                let Some(s) = scalar(other) else { continue };
                let (spec, proto) = s.split_once('/').unwrap_or((&s, "tcp"));
                if proto != "tcp" {
                    continue;
                }
                let (rest, container) = match spec.rsplit_once(':') {
                    Some((r, c)) => (Some(r), c),
                    None => (None, spec),
                };
                let Some((c0, c1)) = range(container) else {
                    continue;
                };
                let (host_ip, host) = match rest {
                    None => (None, None),
                    Some(r) => match r.rsplit_once(':') {
                        Some((ip, h)) => (Some(ip.trim_matches(['[', ']']).to_string()), Some(h)),
                        None => (None, Some(r)),
                    },
                };
                let host = host.filter(|h| !h.is_empty()).and_then(range);
                for (n, target) in (c0..=c1).enumerate() {
                    out.push(Port {
                        host_ip: host_ip.clone(),
                        published: host.map(|(h0, _)| h0 + n as u16),
                        target,
                    });
                }
            }
        }
    }
    out
}

/// The engine behind an image (`postgres:16`, `bitnami/mysql`,
/// `mcr.microsoft.com/mssql/server:2022-latest` …); `None` = not a database
/// Tusk speaks to (web apps, admin UIs like `mongo-express`, …).
fn engine_of(image: &str) -> Option<Engine> {
    let image = image.to_lowercase();
    // Drop the digest and tag (a `:` after the last `/` is a tag, not a port).
    let image = image.split('@').next().unwrap_or("");
    let path = match image.rfind(':') {
        Some(i) if !image[i..].contains('/') => &image[..i],
        _ => image,
    };
    let repo = path.rsplit('/').next().unwrap_or(path);
    Some(match repo {
        r if r.starts_with("postgres")
            || r.starts_with("timescaledb")
            || r.starts_with("postgis")
            || matches!(r, "pgvector" | "paradedb" | "citus" | "pgvecto-rs") =>
        {
            Engine::Postgres
        }
        "cockroach" => Engine::Cockroach,
        r if r.contains("greenplum") => Engine::Greenplum,
        "vertica-ce" | "vertica" => Engine::Vertica,
        "mysql" | "mysql-server" | "percona" | "percona-server" => Engine::MySql,
        "mariadb" => Engine::MariaDb,
        "azure-sql-edge" => Engine::MsSql,
        _ if path.contains("mssql") => Engine::MsSql,
        r if r.starts_with("oracle-")
            || path.starts_with("container-registry.oracle.com/database/") =>
        {
            Engine::Oracle
        }
        r if r.starts_with("clickhouse") => Engine::ClickHouse,
        "redis" | "redis-stack" | "redis-stack-server" | "keydb" | "dragonfly" | "garnet" => {
            Engine::Redis
        }
        "valkey" => Engine::Valkey,
        "mongo"
        | "mongodb"
        | "mongodb-community-server"
        | "mongodb-enterprise-server"
        | "percona-server-mongodb" => Engine::MongoDb,
        "cassandra" | "scylla" => Engine::Cassandra,
        "dynamodb-local" => Engine::DynamoDb,
        "libsql-server" | "sqld" => Engine::LibSql,
        _ => return None,
    })
}

/// The port the server listens on inside the container.
fn container_port(engine: Engine, image: &str, command: &[String]) -> u16 {
    // `redis-server --port 6380`, `sqld --http-listen-addr 0.0.0.0:8081`
    let flag = |name: &str| {
        command
            .iter()
            .position(|a| a == name)
            .and_then(|i| command.get(i + 1))
            .and_then(|v| v.rsplit(':').next()?.parse().ok())
    };
    match engine {
        Engine::Redis | Engine::Valkey => flag("--port").unwrap_or(6379),
        Engine::ClickHouse => 8123,
        Engine::DynamoDb => flag("-port").unwrap_or(8000),
        Engine::LibSql => flag("--http-listen-addr").unwrap_or(8080),
        Engine::MySql | Engine::MariaDb => flag("--port").unwrap_or(3306),
        _ if image.contains("bitnami/cassandra") => 9042,
        e => e.default_port(),
    }
}

/// `command:` as words (a string is split on whitespace, like Compose).
fn command_words(svc: &Value) -> Vec<String> {
    match svc.get("command") {
        Some(Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(scalar).collect(),
        _ => Vec::new(),
    }
}

/// User, password, database (and SSL) from the image's conventions.
struct Creds {
    user: String,
    password: String,
    database: String,
    ssl: SslMode,
}

fn creds(engine: Engine, image: &str, env: &HashMap<String, String>, command: &[String]) -> Creds {
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| env.get(*k).filter(|v| !v.is_empty()).cloned())
            .unwrap_or_default()
    };
    let or = |s: String, d: &str| if s.is_empty() { d.to_string() } else { s };
    let bitnami = image.contains("bitnami/");
    let mut c = Creds {
        user: String::new(),
        password: String::new(),
        database: String::new(),
        ssl: SslMode::Prefer,
    };
    match engine {
        Engine::Postgres | Engine::Greenplum => {
            c.user = or(get(&["POSTGRES_USER", "POSTGRESQL_USERNAME"]), "postgres");
            c.password = get(&["POSTGRES_PASSWORD", "POSTGRESQL_PASSWORD"]);
            let db = get(&["POSTGRES_DB", "POSTGRESQL_DATABASE"]);
            // The official image names the default database after the user.
            c.database = or(db, if bitnami { "postgres" } else { &c.user });
            c.ssl = SslMode::Disable;
        }
        Engine::Cockroach => {
            c.user = or(get(&["COCKROACH_USER"]), "root");
            c.password = get(&["COCKROACH_PASSWORD"]);
            c.database = or(get(&["COCKROACH_DATABASE"]), "defaultdb");
            if command.iter().any(|a| a == "--insecure") {
                c.ssl = SslMode::Disable;
            }
        }
        Engine::Vertica => {
            c.user = or(get(&["APP_DB_USER"]), "dbadmin");
            c.password = get(&["APP_DB_PASSWORD"]);
            c.database = or(get(&["VERTICA_DB_NAME"]), "VMart");
            c.ssl = SslMode::Disable;
        }
        Engine::MySql | Engine::MariaDb => {
            let user = get(&["MARIADB_USER", "MYSQL_USER"]);
            if user.is_empty() {
                c.user = "root".into();
                c.password = get(&["MARIADB_ROOT_PASSWORD", "MYSQL_ROOT_PASSWORD"]);
            } else {
                c.user = user;
                c.password = get(&["MARIADB_PASSWORD", "MYSQL_PASSWORD"]);
            }
            c.database = or(get(&["MARIADB_DATABASE", "MYSQL_DATABASE"]), "mysql");
        }
        Engine::MsSql => {
            c.user = "sa".into();
            c.password = get(&["MSSQL_SA_PASSWORD", "SA_PASSWORD"]);
            c.database = "master".into();
        }
        Engine::Oracle => {
            let app = get(&["APP_USER"]);
            if !app.is_empty() {
                c.user = app;
                c.password = get(&["APP_USER_PASSWORD"]);
            } else {
                c.user = "system".into();
                c.password = get(&["ORACLE_PASSWORD", "ORACLE_PWD"]);
            }
            let pdb = if image.contains("xe") || image.contains("express") {
                "XEPDB1"
            } else if image.contains("free") {
                "FREEPDB1"
            } else {
                "ORCLPDB1"
            };
            c.database = or(get(&["ORACLE_DATABASE", "ORACLE_PDB"]), pdb);
        }
        Engine::ClickHouse => {
            c.user = or(
                get(&["CLICKHOUSE_USER", "CLICKHOUSE_ADMIN_USER"]),
                "default",
            );
            c.password = get(&["CLICKHOUSE_PASSWORD", "CLICKHOUSE_ADMIN_PASSWORD"]);
            c.database = or(get(&["CLICKHOUSE_DB"]), "default");
        }
        Engine::Redis | Engine::Valkey => {
            let flag = command
                .iter()
                .position(|a| a == "--requirepass")
                .and_then(|i| command.get(i + 1))
                .cloned();
            c.password = flag.unwrap_or_else(|| {
                get(&["REDIS_PASSWORD", "VALKEY_PASSWORD", "REDIS_ARGS_PASSWORD"])
            });
            c.database = "0".into();
        }
        Engine::MongoDb => {
            c.user = get(&[
                "MONGO_INITDB_ROOT_USERNAME",
                "MONGODB_INITDB_ROOT_USERNAME",
                "MONGODB_ROOT_USER",
            ]);
            c.password = get(&[
                "MONGO_INITDB_ROOT_PASSWORD",
                "MONGODB_INITDB_ROOT_PASSWORD",
                "MONGODB_ROOT_PASSWORD",
            ]);
            if bitnami && c.user.is_empty() && !c.password.is_empty() {
                c.user = "root".into();
            }
            c.database = get(&["MONGO_INITDB_DATABASE", "MONGODB_DATABASE"]);
        }
        Engine::Cassandra => {
            // Bitnami turns authentication on (cassandra / cassandra).
            let (du, dp) = if bitnami {
                ("cassandra", "cassandra")
            } else {
                ("", "")
            };
            c.user = or(get(&["CASSANDRA_USER"]), du);
            c.password = or(get(&["CASSANDRA_PASSWORD"]), dp);
            c.database = get(&["CASSANDRA_KEYSPACE"]);
        }
        // DynamoDB Local accepts any key pair (the secret goes in the Keychain).
        Engine::DynamoDb => c.password = "local".into(),
        _ => {}
    }
    c
}

/// The project name: top-level `name:`, `COMPOSE_PROJECT_NAME`, or the
/// directory the file is in.
fn project_name(doc: &Value, dir: &Path, vars: &HashMap<String, String>) -> String {
    doc.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| vars.get("COMPOSE_PROJECT_NAME").cloned())
        .filter(|n| !n.trim().is_empty())
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| "compose".into())
}

/// A service definition, with where its relative paths (`env_file`)
/// resolve and the variables its file was interpolated with.
struct Service {
    name: String,
    def: Value,
    dir: PathBuf,
    vars: HashMap<String, String>,
}

/// Load a compose file (+ override), interpolated, and its services —
/// then those of the files it `include:`s. `parent`: the including
/// file's variables (they win over this directory's `.env`).
fn load(
    file: &Path,
    parent: Option<&HashMap<String, String>>,
    depth: u8,
    out: &mut Vec<Service>,
) -> Result<Value, String> {
    let dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut doc = read_yaml(file)?;
    if let Some(o) = override_file(file) {
        merge(&mut doc, read_yaml(&o)?);
    }
    // Shell environment wins over `.env`, as in Compose.
    let mut vars: HashMap<String, String> = std::fs::read_to_string(dir.join(".env"))
        .map(|t| parse_env_file(&t).into_iter().collect())
        .unwrap_or_default();
    match parent {
        Some(p) => vars.extend(p.iter().map(|(k, v)| (k.clone(), v.clone()))),
        None => vars.extend(std::env::vars()),
    }
    interpolate_tree(&mut doc, &vars);
    for (name, def) in doc
        .get("services")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        out.push(Service {
            name: name.clone(),
            def: def.clone(),
            dir: dir.clone(),
            vars: vars.clone(),
        });
    }
    // `include: [a.yaml, {path: b.yaml}, {path: [c.yaml, c.override.yaml]}]`
    if depth < 8 {
        for inc in doc
            .get("include")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let path = match inc {
                Value::String(p) => Some(p.as_str()),
                Value::Object(o) => match o.get("path") {
                    Some(Value::String(p)) => Some(p.as_str()),
                    Some(Value::Array(a)) => a.first().and_then(Value::as_str),
                    _ => None,
                },
                _ => None,
            };
            if let Some(path) = path {
                load(&dir.join(path), Some(&vars), depth + 1, out)?;
            }
        }
    }
    Ok(doc)
}

/// Read a compose file into an import plan: one profile per supported
/// database service (in file order), all in the project's folder.
/// `existing`: (name, folder) of saved profiles — already imported
/// services are skipped. The plan's id per profile is its password.
pub fn plan(file: &Path, existing: &[(String, Option<String>)]) -> Result<Plan, String> {
    let dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut services = Vec::new();
    let doc = load(file, None, 0, &mut services)?;
    if services.is_empty() {
        return Err("no `services:` in the compose file".into());
    }
    let project = project_name(&doc, &dir, &services[0].vars);
    let taken = |name: &str| existing.iter().any(|(n, _)| n == name);
    let mut plan = Plan {
        source: Source::Compose(project.clone()),
        groups: vec![project.clone()],
        ..Default::default()
    };
    for Service {
        name: service,
        def: svc,
        dir,
        vars,
    } in &services
    {
        let Some(image) = svc.get("image").and_then(Value::as_str) else {
            plan.skipped.push(format!("{service} (no image)"));
            continue;
        };
        let Some(engine) = engine_of(image) else {
            continue;
        };
        let command = command_words(svc);
        let target = container_port(engine, &image.to_lowercase(), &command);
        let host_network = svc.get("network_mode").and_then(Value::as_str) == Some("host");
        let ports = svc.get("ports").map(parse_ports).unwrap_or_default();
        let published: Vec<&Port> = ports.iter().filter(|p| p.published.is_some()).collect();
        // The engine's port; else the only published one (a custom port).
        let port = published
            .iter()
            .find(|p| p.target == target)
            .or_else(|| (published.len() == 1).then(|| &published[0]))
            .copied();
        let (host, port) = match (port, host_network) {
            (Some(p), _) => {
                let host = match p.host_ip.as_deref() {
                    None | Some("" | "0.0.0.0" | "::") => "127.0.0.1".to_string(),
                    Some(ip) => ip.to_string(),
                };
                (host, p.published.unwrap_or(target))
            }
            (None, true) => ("127.0.0.1".to_string(), target),
            (None, false) => {
                plan.skipped
                    .push(format!("{service} (port {target} not published)"));
                continue;
            }
        };

        let env = service_env(svc, dir, vars);
        let c = creds(engine, &image.to_lowercase(), &env, &command);
        // The service name, unless another folder already has it.
        let in_folder = |n: &str| {
            existing
                .iter()
                .any(|(en, f)| en == n && f.as_deref() == Some(&project))
        };
        let name = if in_folder(service) || in_folder(&format!("{project}-{service}")) {
            plan.skipped.push(format!("{service} (already imported)"));
            continue;
        } else if taken(service) {
            format!("{project}-{service}")
        } else {
            service.clone()
        };
        if taken(&name) {
            plan.skipped.push(format!("{service} (name {name} taken)"));
            continue;
        }

        let mut options = std::collections::BTreeMap::new();
        let mut path = None;
        match engine {
            Engine::DynamoDb => {
                options.insert("region".into(), "us-east-1".into());
                options.insert("endpoint".into(), format!("http://{host}:{port}"));
                options.insert("access_key".into(), "local".into());
            }
            Engine::LibSql => path = Some(format!("http://{host}:{port}")),
            _ => {}
        }
        let conn = SavedConnection {
            engine,
            name,
            host,
            port,
            database: c.database,
            user: c.user,
            ssl: c.ssl,
            folder: Some(project.clone()),
            tag: Some(ConnTag::Local),
            last_used: None,
            ssh: None,
            status_color: None,
            path,
            options,
        };
        plan.connections.push((conn, c.password));
    }
    Ok(plan)
}

/// Import one planned profile, its password into the Keychain. Returns
/// whether a password came along.
pub fn import_one(conn: &SavedConnection, password: &str) -> Result<bool, String> {
    let with_pw = !password.is_empty() && db::save_password(&conn.name, password).is_ok();
    crate::migrate::save_profile(conn)?;
    Ok(with_pw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn interpolation() {
        let v = vars(&[("A", "1"), ("E", "")]);
        assert_eq!(interpolate("${A}-$A-$$A", &v), "1-1-$A");
        assert_eq!(interpolate("${B:-x}", &v), "x");
        assert_eq!(interpolate("${E:-x}/${E-x}", &v), "x/");
        assert_eq!(interpolate("${B:-${A}}", &v), "1");
        assert_eq!(interpolate("${A:+yes}${B:+no}", &v), "yes");
        assert_eq!(interpolate("${B:?missing}", &v), "");
    }

    #[test]
    fn ports() {
        let v: Value = serde_json::json!([
            "5432",
            "15432:5432",
            "127.0.0.1:3307:3306/tcp",
            "[::1]:6380:6379",
            "9000-9001:8000-8001",
            "53:53/udp",
            6379,
            {"target": 27017, "published": "27018", "host_ip": "0.0.0.0"}
        ]);
        let p = parse_ports(&v);
        let t = |target: u16| p.iter().filter(|x| x.target == target).collect::<Vec<_>>();
        assert_eq!(t(5432)[0].published, None);
        assert_eq!(t(5432)[1].published, Some(15432));
        assert_eq!(t(3306)[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(t(3306)[0].published, Some(3307));
        assert_eq!(t(6379)[0].host_ip.as_deref(), Some("::1"));
        assert_eq!(t(8001)[0].published, Some(9001));
        assert!(t(53).is_empty());
        assert_eq!(t(27017)[0].published, Some(27018));
    }

    #[test]
    fn engines_by_image() {
        assert_eq!(engine_of("postgres:16-alpine"), Some(Engine::Postgres));
        assert_eq!(engine_of("postgis/postgis:16-3.4"), Some(Engine::Postgres));
        assert_eq!(
            engine_of("timescale/timescaledb-ha:pg16"),
            Some(Engine::Postgres)
        );
        assert_eq!(
            engine_of("bitnami/postgresql:latest"),
            Some(Engine::Postgres)
        );
        assert_eq!(engine_of("localhost:5000/mysql:8"), Some(Engine::MySql));
        assert_eq!(
            engine_of("mcr.microsoft.com/mssql/server:2022-latest"),
            Some(Engine::MsSql)
        );
        assert_eq!(engine_of("gvenzl/oracle-free:slim"), Some(Engine::Oracle));
        assert_eq!(
            engine_of("clickhouse/clickhouse-server"),
            Some(Engine::ClickHouse)
        );
        assert_eq!(engine_of("valkey/valkey:8"), Some(Engine::Valkey));
        assert_eq!(engine_of("mongo:7"), Some(Engine::MongoDb));
        assert_eq!(engine_of("mongo-express"), None);
        assert_eq!(engine_of("amazon/dynamodb-local"), Some(Engine::DynamoDb));
        assert_eq!(engine_of("nginx:alpine"), None);
    }

    #[test]
    fn plan_from_file() {
        let dir = std::env::temp_dir().join(format!("tusk-compose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "PG_PORT=15432\nDB_PASS='s3cret'\n").unwrap();
        std::fs::write(
            dir.join("compose.yaml"),
            r#"
name: shop
x-pg: &pg
  image: postgres:16
  environment:
    POSTGRES_USER: app
    POSTGRES_PASSWORD: ${DB_PASS}
services:
  db:
    <<: *pg
    ports: ["${PG_PORT:-5432}:5432"]
  cache:
    image: redis:7
    command: redis-server --requirepass hunter2
    ports: ["6379:6379"]
  mysql:
    image: mysql:8
    environment:
      - MYSQL_ROOT_PASSWORD=root
      - MYSQL_DATABASE=shop
    ports: ["3306"]
  web:
    image: nginx
    ports: ["8080:80"]
"#,
        )
        .unwrap();
        let existing = vec![("db".to_string(), None)];
        let plan = plan(&dir.join("compose.yaml"), &existing).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(plan.groups, vec!["shop".to_string()]);
        assert_eq!(plan.connections.len(), 2);
        let (pg, pw) = &plan.connections[0];
        assert_eq!(pg.name, "shop-db"); // "db" is taken outside the folder
        assert_eq!(
            (pg.engine, pg.port, pg.user.as_str(), pg.database.as_str()),
            (Engine::Postgres, 15432, "app", "app")
        );
        assert_eq!(pw, "s3cret");
        assert_eq!(pg.folder.as_deref(), Some("shop"));
        let (redis, pw) = &plan.connections[1];
        assert_eq!(
            (redis.engine, redis.port, pw.as_str()),
            (Engine::Redis, 6379, "hunter2")
        );
        assert!(plan.skipped.iter().any(|s| s.starts_with("mysql")));
    }
}
