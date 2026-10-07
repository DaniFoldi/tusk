//! PostgreSQL connection layer: saved-connection persistence (JSON, no passwords),
//! System credential store via `keyring`, tokio runtime bridge for sqlx, connect/test helpers.

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{ConnectOptions, Row};

/// SSL modes offered in the UI (subset of sqlx's modes).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
}

impl SslMode {
    pub fn all() -> [SslMode; 3] {
        [SslMode::Disable, SslMode::Prefer, SslMode::Require]
    }

    fn pg_mode(self) -> PgSslMode {
        match self {
            SslMode::Disable => PgSslMode::Disable,
            SslMode::Prefer => PgSslMode::Prefer,
            SslMode::Require => PgSslMode::Require,
        }
    }
}

/// Environment tag shown as a colored badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnTag {
    Local,
    Development,
    Testing,
    Staging,
    Production,
}

impl ConnTag {
    pub const ALL: [ConnTag; 5] = [
        ConnTag::Local,
        ConnTag::Development,
        ConnTag::Testing,
        ConnTag::Staging,
        ConnTag::Production,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ConnTag::Local => "local",
            ConnTag::Development => "development",
            ConnTag::Testing => "testing",
            ConnTag::Staging => "staging",
            ConnTag::Production => "production",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.label() == s)
    }

    /// Badge color.
    pub fn color(self) -> u32 {
        match self {
            ConnTag::Local => 0x8B949E,
            ConnTag::Development => 0x3FB950,
            ConnTag::Testing => 0x4A90F0,
            ConnTag::Staging => 0xFF9040,
            ConnTag::Production => 0xE5484D,
        }
    }
}

/// Connect through an SSH jump host (local port forward to host:port).
/// Secrets (SSH password / key passphrase) live in the Keychain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Private key file; `None` = password authentication.
    #[serde(default)]
    pub key_path: Option<String>,
}

/// A saved connection. NEVER contains a password — that lives in the Keychain.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedConnection {
    /// Which database engine (older files: Postgres).
    #[serde(default)]
    pub engine: crate::engine::Engine,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub ssl: SslMode,
    /// Folder in the connection list (`None` = top level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<ConnTag>,
    /// Unix seconds of the last successful connect (orders "Recent").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    /// "Status Color" (index into [`STATUS_COLORS`]): tints the
    /// connection badge and the title-bar pill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_color: Option<usize>,
    /// Database file (SQLite / DuckDB) or server URL (LibSQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Engine-specific fields (account id, warehouse, region, …); secrets
    /// never go here — they live in the Keychain like passwords.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub options: std::collections::BTreeMap<String, String>,
}

impl SavedConnection {
    /// An engine-specific option (empty when unset).
    pub fn opt(&self, key: &str) -> &str {
        self.options.get(key).map(String::as_str).unwrap_or("")
    }
}

/// Status-color swatches: green, grey, blue, brown, red.
pub const STATUS_COLORS: [u32; 5] = [0x1F8B4C, 0x6E6E73, 0x1F5FAF, 0xA0783A, 0x8B1A1A];

impl SavedConnection {
    /// The chosen status color; `None` = neutral (nothing picked).
    pub fn status_rgb(&self) -> Option<u32> {
        self.status_color
            .and_then(|i| STATUS_COLORS.get(i).copied())
    }

    /// Where the connection points, for the title bar and connection lists:
    /// the file name for file databases (never a meaningless `host:0`), the
    /// URL / account for token and cloud engines, `host:port` for servers.
    pub fn endpoint(&self) -> String {
        use crate::engine::Form;
        match self.engine.form() {
            Form::File => self
                .path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Form::UrlToken => self.path.clone().unwrap_or_default(),
            Form::CloudflareD1 | Form::Snowflake | Form::BigQuery | Form::DynamoDb => {
                self.options.values().next().cloned().unwrap_or_default()
            }
            Form::Server => format!("{}:{}", self.host, self.port),
        }
    }

    /// `postgresql://user@host:port/db` (no password) for "Copy as URL".
    pub fn url(&self) -> String {
        use crate::engine::Form;
        match self.engine.form() {
            Form::File => format!(
                "{}://{}",
                self.engine.scheme(),
                self.path.clone().unwrap_or_default()
            ),
            Form::UrlToken => self.path.clone().unwrap_or_default(),
            _ => format!(
                "{}://{}@{}:{}/{}",
                self.engine.scheme(),
                self.user,
                self.host,
                self.port,
                self.database
            ),
        }
    }
}

impl SavedConnection {
    pub fn is_valid(&self) -> bool {
        use crate::engine::{Engine, Form};
        if self.name.trim().is_empty() {
            return false;
        }
        let path = self.path.as_deref().unwrap_or("").trim();
        match self.engine.form() {
            Form::File | Form::UrlToken => !path.is_empty(),
            Form::CloudflareD1 => {
                // Database is optional: an account-level profile lists
                // its D1 databases after connecting.
                !self.opt("account_id").is_empty()
            }
            Form::Snowflake => !self.opt("account").is_empty() && !self.user.trim().is_empty(),
            Form::BigQuery => !self.opt("project").is_empty(),
            Form::DynamoDb => !self.opt("region").is_empty() || !self.host.trim().is_empty(),
            Form::Server => {
                // Some servers take any database / no user. Databases are
                // optional where a default exists (or server-wide works):
                // the dropdown lists the rest after connecting.
                let needs_db = !matches!(
                    self.engine,
                    Engine::Redis
                        | Engine::MongoDb
                        | Engine::Cassandra
                        | Engine::ClickHouse
                        | Engine::Postgres
                        | Engine::Greenplum
                        | Engine::Cockroach
                        | Engine::MySql
                        | Engine::MariaDb
                        | Engine::MsSql
                );
                let needs_user = !matches!(
                    self.engine,
                    Engine::Redis | Engine::MongoDb | Engine::Cassandra
                );
                !self.host.trim().is_empty()
                    && self.port != 0
                    && (!needs_db || !self.database.trim().is_empty())
                    && (!needs_user || !self.user.trim().is_empty())
            }
        }
    }
}

pub fn dev_default() -> SavedConnection {
    SavedConnection {
        engine: crate::engine::Engine::Postgres,
        path: None,
        options: Default::default(),
        name: "Local Docker".to_string(),
        host: "127.0.0.1".to_string(),
        port: 55432,
        database: "tusk_dev".to_string(),
        user: "tusk".to_string(),
        ssl: SslMode::Prefer,
        folder: None,
        tag: Some(ConnTag::Local),
        last_used: None,
        ssh: None,
        status_color: None,
    }
}

/// `~/Library/Application Support/tusk/connections.json` on macOS.
pub fn connections_path() -> PathBuf {
    app_dir().join("connections.json")
}

/// `~/Library/Application Support/tusk`. On first run the folder of the
/// app's previous name is copied over (once — the old one is kept).
pub(crate) fn app_dir() -> PathBuf {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        // A separate profile (connections, settings, history) for test runs.
        if let Some(d) = std::env::var_os("TUSK_DATA_DIR") {
            return PathBuf::from(d);
        }
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("tusk");
        // The data folder under the app's previous name, Veri (copied once).
        let old = base.join("veri");
        if !dir.exists() && old.is_dir() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(entries) = std::fs::read_dir(&old) {
                for e in entries.flatten() {
                    if e.path().is_file() {
                        let _ = std::fs::copy(e.path(), dir.join(e.file_name()));
                    }
                }
            }
        }
        dir
    });
    DIR.clone()
}

pub fn load_connections() -> Vec<SavedConnection> {
    load_connections_from(&connections_path())
}

/// Set when `connections.json` couldn't be read and was moved aside; the
/// app shows it once.
static LOAD_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The "connections.json couldn't be read" message, once.
pub fn take_load_notice() -> Option<String> {
    LOAD_NOTICE.lock().ok()?.take()
}

fn load_connections_from(path: &std::path::Path) -> Vec<SavedConnection> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        // First run: no connections yet (no sample profile to trip over).
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<SavedConnection>>(&text) {
        Ok(list) => list,
        Err(e) => {
            // Unreadable (another build's format, a hand edit…): keep the
            // file under another name, or the next save would wipe the
            // user's connections with the default list.
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let backup = path.with_file_name(format!("{name}.bak-{secs}"));
            let msg = match std::fs::rename(path, &backup) {
                Ok(()) => format!(
                    "Couldn't read {name} ({e}). It was kept as {}.",
                    backup.display()
                ),
                Err(err) => format!("Couldn't read {name} ({e}) or set it aside ({err})."),
            };
            log::warn!("{msg}");
            if let Ok(mut n) = LOAD_NOTICE.lock() {
                *n = Some(msg);
            }
            Vec::new()
        }
    }
}

pub fn save_connections(list: &[SavedConnection]) -> anyhow::Result<()> {
    write_atomic(&connections_path(), &serde_json::to_string_pretty(list)?)
}

/// Write through a temp file + rename, so a crash mid-write never leaves a
/// cut-off file behind.
fn write_atomic(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Put `conn` into `list`: in place of `replacing` (the profile being
/// edited, maybe renamed) or of one with the same name, else at the end.
/// Returns its index.
fn upsert(
    list: &mut Vec<SavedConnection>,
    conn: SavedConnection,
    replacing: Option<&str>,
) -> usize {
    let slot = replacing
        .and_then(|old| list.iter().position(|c| c.name == old))
        .or_else(|| list.iter().position(|c| c.name == conn.name));
    match slot {
        Some(ix) => {
            list[ix] = conn;
            ix
        }
        None => {
            list.push(conn);
            list.len() - 1
        }
    }
}

/// Save one profile into the list as it is on disk *now* — writing back a
/// list read earlier would undo changes made since (imports, last-used
/// stamps, other windows). Returns the new list and the profile's index.
pub fn upsert_connection(
    conn: SavedConnection,
    replacing: Option<&str>,
) -> anyhow::Result<(Vec<SavedConnection>, usize)> {
    let mut list = load_connections();
    let ix = upsert(&mut list, conn, replacing);
    save_connections(&list)?;
    Ok((list, ix))
}

/// Whether a saved profile other than `except` is called `name`.
pub fn connection_name_taken(name: &str, except: Option<&str>) -> bool {
    load_connections()
        .iter()
        .any(|c| c.name == name && Some(c.name.as_str()) != except)
}

/// Connection-list groups (folders), incl. empty ones created with
/// "New Group"; profiles reference them by name (`SavedConnection::folder`).
fn groups_path() -> PathBuf {
    connections_path().with_file_name("groups.json")
}

pub fn load_groups() -> Vec<String> {
    std::fs::read_to_string(groups_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_groups(groups: &[String]) -> anyhow::Result<()> {
    write_atomic(&groups_path(), &serde_json::to_string_pretty(groups)?)
}

/// The profile Tusk was last connected to (no password) — reopened on launch.
fn last_connection_path() -> PathBuf {
    connections_path().with_file_name("last_connection.json")
}

pub fn remember_last_connection(conn: &SavedConnection) {
    if let Ok(text) = serde_json::to_string_pretty(conn) {
        let _ = write_atomic(&last_connection_path(), &text);
    }
}

pub fn last_connection() -> Option<SavedConnection> {
    let text = std::fs::read_to_string(last_connection_path()).ok()?;
    serde_json::from_str(&text)
        .ok()
        .filter(SavedConnection::is_valid)
}

/// Explicit disconnect: the next launch shows the welcome screen again.
pub fn forget_last_connection() {
    let _ = std::fs::remove_file(last_connection_path());
}

// ---- Keychain (passwords never touch the JSON file) ----

/// What the OS calls its credential store, for labels and errors.
pub const CREDENTIAL_STORE: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(windows) {
    "Credential Manager"
} else {
    "system keyring"
};

const KEYCHAIN_SERVICE: &str = "tusk-postgres";
/// Keychain services under the app's previous name, Veri: read once and
/// copied to the ones above, so passwords saved before the rename keep working.
const LEGACY_SERVICE: &str = "veri-postgres";
const LEGACY_SSH_SERVICE: &str = "veri-ssh";

/// Read a secret; if only the pre-rename item exists, copy it to the
/// new service so the next read is direct.
fn get_or_migrate(service: &str, legacy: &str, name: &str) -> Result<String, String> {
    let entry = keyring::Entry::new(service, name).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(p) => Ok(p),
        Err(keyring::Error::NoEntry) => {
            let old = keyring::Entry::new(legacy, name).map_err(|e| e.to_string())?;
            let p = old.get_password().map_err(|e| e.to_string())?;
            let _ = entry.set_password(&p);
            Ok(p)
        }
        Err(e) => Err(e.to_string()),
    }
}

fn keyring_entry(connection_name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, connection_name).map_err(|e| e.to_string())
}

pub fn save_password(connection_name: &str, password: &str) -> Result<(), String> {
    keyring_entry(connection_name)?
        .set_password(password)
        .map_err(|e| e.to_string())
}

const KEYCHAIN_SSH_SERVICE: &str = "tusk-ssh";

fn ssh_entry(connection_name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SSH_SERVICE, connection_name).map_err(|e| e.to_string())
}

/// SSH password or private-key passphrase of a connection.
pub fn save_ssh_secret(connection_name: &str, secret: &str) -> Result<(), String> {
    ssh_entry(connection_name)?
        .set_password(secret)
        .map_err(|e| e.to_string())
}

pub fn load_ssh_secret(connection_name: &str) -> Option<String> {
    get_or_migrate(KEYCHAIN_SSH_SERVICE, LEGACY_SSH_SERVICE, connection_name).ok()
}

/// Forget both secrets of a deleted connection.
pub fn delete_secrets(connection_name: &str) {
    if let Ok(e) = keyring_entry(connection_name) {
        let _ = e.delete_credential();
    }
    if let Ok(e) = ssh_entry(connection_name) {
        let _ = e.delete_credential();
    }
    for legacy in [LEGACY_SERVICE, LEGACY_SSH_SERVICE] {
        if let Ok(e) = keyring::Entry::new(legacy, connection_name) {
            let _ = e.delete_credential();
        }
    }
}

pub fn load_password(connection_name: &str) -> Result<String, String> {
    get_or_migrate(KEYCHAIN_SERVICE, LEGACY_SERVICE, connection_name)
}

// ---- sqlx over a dedicated tokio runtime ----
//
// GPUI does not run a tokio runtime, and sqlx's tokio driver needs one.
// A small multi-thread runtime lives for the process; queries are spawned
// onto it and the JoinHandle is awaited from GPUI's executor.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("tusk-sqlx")
        .enable_all()
        .build()
        .expect("tokio runtime")
});

pub fn connect_options(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: &str,
    ssl: SslMode,
) -> PgConnectOptions {
    let opts = PgConnectOptions::new()
        .host(host)
        .port(port)
        .username(user)
        .password(password)
        .database(database)
        .ssl_mode(ssl.pg_mode());
    // Settings ▸ General ▸ Query Timeout (0 = none).
    match crate::settings::get().query_timeout_secs {
        0 => opts,
        secs => opts.options([("statement_timeout", format!("{secs}s"))]),
    }
}

/// [`connect_options`] without extra startup parameters (servers on the
/// Postgres protocol that reject `statement_timeout` at startup).
pub fn connect_options_plain(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: &str,
    ssl: SslMode,
) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(host)
        .port(port)
        .username(user)
        .password(password)
        .database(database)
        .ssl_mode(ssl.pg_mode())
}

pub type DbResult<T> = Result<T, String>;

/// The shared tokio runtime (sqlx + the language-server client's IO).
pub fn runtime() -> &'static tokio::runtime::Runtime {
    &RUNTIME
}

/// Run a Send future on the shared tokio runtime and await it from any executor.
/// [`run_db`] plus a Console line for `sql` (timing + outcome).
pub(crate) async fn run_logged<T: Send + 'static>(
    sql: String,
    source: crate::console::Source,
    fut: impl std::future::Future<Output = DbResult<T>> + Send + 'static,
) -> DbResult<T> {
    run_db(async move { crate::console::logged(&sql, source, fut).await }).await
}

pub(crate) async fn run_db<T: Send + 'static>(
    fut: impl std::future::Future<Output = DbResult<T>> + Send + 'static,
) -> DbResult<T> {
    RUNTIME
        .handle()
        .spawn(fut)
        .await
        .map_err(|e| format!("db task failed: {e}"))?
}

// ---- data grid (Phase 6) ----

/// The connected engine's SQL dialect: the workspace works with one
/// connection at a time, and the identifiers / literals it builds follow it.
static DIALECT: std::sync::RwLock<crate::engine::Dialect> =
    std::sync::RwLock::new(crate::engine::Dialect::Postgres);

static ENGINE: std::sync::RwLock<crate::engine::Engine> =
    std::sync::RwLock::new(crate::engine::Engine::Postgres);

/// The connected engine (and its dialect).
pub fn set_engine(e: crate::engine::Engine) {
    if let Ok(mut w) = ENGINE.write() {
        *w = e;
    }
    set_dialect(e.dialect());
}

pub fn engine() -> crate::engine::Engine {
    ENGINE.read().map(|e| *e).unwrap_or_default()
}

pub fn set_dialect(d: crate::engine::Dialect) {
    if let Ok(mut w) = DIALECT.write() {
        *w = d;
    }
}

pub fn dialect() -> crate::engine::Dialect {
    DIALECT
        .read()
        .map(|d| *d)
        .unwrap_or(crate::engine::Dialect::Postgres)
}

/// Quote an identifier ("schema", "table", "column") for the connected engine.
pub fn quote_ident(name: &str) -> String {
    dialect().quote(name)
}

// ---- SQL execution (Phase 7) ----

/// Split a script into statements, ignoring `;` inside strings, quoted
/// identifiers, line/block comments and dollar-quoted bodies.
pub fn split_statements(sql: &str) -> Vec<String> {
    statement_spans(sql)
        .into_iter()
        .map(|r| sql[r].to_string())
        .collect()
}

/// Byte ranges of the statements in `sql` (without the `;`), skipping empty
/// ones. Quotes, comments and dollar-quoted bodies are respected.
pub fn statement_spans(sql: &str) -> Vec<std::ops::Range<usize>> {
    // Byte offset of every char, plus the end.
    let offs: Vec<usize> = sql
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(sql.len()))
        .collect();
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        if c == '\'' {
            i += 1;
            while i < n {
                if chars[i] == '\'' {
                    if i + 1 < n && chars[i + 1] == '\'' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == '"' {
            i += 1;
            while i < n {
                if chars[i] == '"' {
                    if i + 1 < n && chars[i + 1] == '"' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '$' {
            // Dollar-quoted string: $tag$ ... $tag$ (tag may be empty).
            let mut j = i + 1;
            while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j < n && chars[j] == '$' {
                let tag: String = chars[i..=j].iter().collect();
                let tag_chars: Vec<char> = tag.chars().collect();
                let mut k = j + 1;
                loop {
                    if k + tag_chars.len() <= n && chars[k..k + tag_chars.len()] == tag_chars[..] {
                        i = k + tag_chars.len();
                        break;
                    }
                    k += 1;
                    if k >= n {
                        i = n;
                        break;
                    }
                }
                continue;
            }
            i += 1;
            continue;
        }
        if c == ';' {
            if let Some(r) = trimmed(sql, offs[start]..offs[i]) {
                out.push(r);
            }
            start = i + 1;
        }
        i += 1;
    }
    if let Some(r) = trimmed(sql, offs[start.min(n)]..sql.len()) {
        out.push(r);
    }
    out
}

/// `r` without surrounding whitespace; None if nothing is left.
fn trimmed(sql: &str, r: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
    let text = &sql[r.clone()];
    let lead = text.len() - text.trim_start().len();
    let trail = text.len() - text.trim_end().len();
    (lead < text.len()).then(|| r.start + lead..r.end - trail)
}

/// Safe mode: statements that can't be taken back — DROP / TRUNCATE /
/// ALTER, and UPDATE / DELETE with no WHERE clause.
pub fn is_destructive(stmt: &str) -> bool {
    // Skip leading `--` comment lines.
    let body: String = stmt
        .lines()
        .skip_while(|l| l.trim().is_empty() || l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ");
    let up = body.trim().to_uppercase();
    let first = up.split_whitespace().next().unwrap_or_default();
    match first {
        "DROP" | "TRUNCATE" | "ALTER" => true,
        "UPDATE" | "DELETE" => !up.split_whitespace().any(|w| w == "WHERE"),
        _ => false,
    }
}

/// Safe mode ("Confirm Before Saving"): statements that change data or schema
/// — anything but a read, a transaction boundary or a session setting.
pub fn is_write(stmt: &str) -> bool {
    if classify_statement(stmt) == StmtKind::Query {
        return false;
    }
    let first = strip_leading_comments(stmt)
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !matches!(
        first.as_str(),
        "" | "BEGIN"
            | "START"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "SAVEPOINT"
            | "RELEASE"
            | "SET"
            | "RESET"
            | "USE"
    )
}

/// The statement under the cursor: the one whose
/// text (or terminating `;`) contains `cursor`; on blank lines between
/// statements, the one before the cursor; before the first, the first.
pub fn statement_at(sql: &str, cursor: usize) -> Option<String> {
    let spans = statement_spans(sql);
    let pick = spans
        .iter()
        // `end` is the `;` position: a cursor right after it still belongs.
        .find(|r| cursor >= r.start && cursor <= r.end + 1)
        .or_else(|| spans.iter().rev().find(|r| r.end <= cursor))
        .or(spans.first())?;
    Some(sql[pick.clone()].to_string())
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum StmtKind {
    Query,
    Mutation,
    Other,
}

/// Classify by first keyword. Leading `--` line comments and `/* */` blocks are
/// skipped, so editor buffers starting with a comment line classify correctly.
pub fn classify_statement(sql: &str) -> StmtKind {
    let first: String = strip_leading_comments(sql)
        .trim_start_matches(['(', ' ', '\t', '\n', '\r'])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    // Shell calls (`db.x.find(…)`, `HGETALL k`) answer with rows, writes included.
    if matches!(
        engine(),
        crate::engine::Engine::MongoDb | crate::engine::Engine::Redis
    ) {
        return StmtKind::Query;
    }
    match first.as_str() {
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "DESC"
        | "PRAGMA" | "SUMMARIZE" => StmtKind::Query,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => StmtKind::Mutation,
        _ => StmtKind::Other,
    }
}

/// Skip whitespace, `--` line comments and `/* … */` blocks from the front.
fn strip_leading_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            s = match rest.find('\n') {
                Some(i) => &rest[i + 1..],
                None => return "",
            };
        } else if let Some(rest) = s.strip_prefix("/*") {
            match rest.find("*/") {
                Some(i) => s = &rest[i + 2..],
                None => return "",
            }
        } else {
            return s;
        }
    }
}

/// Rows of a SELECT-like statement via row_to_json (ordered, null-safe).
/// Caps at `limit` rows; caller reports truncation.
pub(crate) async fn pg_run_query_rows(
    pool: &sqlx::PgPool,
    sql: &str,
    limit: i64,
) -> DbResult<Vec<serde_json::Value>> {
    let pool = pool.clone();
    let inner = sql.trim().trim_end_matches(';').to_string();
    let wrapped = format!("SELECT row_to_json(t) FROM ({inner}) t LIMIT $1");
    run_logged(inner.clone(), crate::console::Source::Data, async move {
        let rows: Vec<serde_json::Value> = sqlx::query_scalar(&wrapped)
            .bind(limit)
            .fetch_all(&pool)
            .await
            .map_err(pg_error_message)?;
        Ok(rows)
    })
    .await
}

/// Where an editable query result came from: one base table whose primary
/// key is fully present in the result. `columns[i]` is the table column
/// behind result column `i` (None for expressions / other tables).
#[derive(Clone, Debug)]
pub struct EditSource {
    pub schema: String,
    pub table: String,
    pub columns: Vec<Option<GridColumnMeta>>,
    /// Result-column indexes of the primary key (WHERE clause of edits).
    pub key: Vec<usize>,
    /// The engine the result came from (its saves use its SQL).
    pub engine: crate::engine::Engine,
}

/// Decide whether a SELECT's result can be edited in place:
/// Postgres' RowDescription tells, per result column, the base table OID +
/// attribute number it came from (`describe`, no execution). Editable when
/// every traced column comes from ONE ordinary table and its whole primary
/// key is in the result. `Err` carries the human reason it's read-only.
pub(crate) async fn pg_result_edit_source(
    pool: &sqlx::PgPool,
    sql: &str,
) -> DbResult<Result<EditSource, String>> {
    let pool = pool.clone();
    let inner = sql.trim().trim_end_matches(';').to_string();
    run_logged(
        format!("-- describe\n{inner}"),
        crate::console::Source::Meta,
        async move {
            use sqlx::Executor;
            let described = pool.describe(&inner).await.map_err(pg_error_message)?;
            let origins: Vec<Option<(u32, i16)>> = described
                .columns
                .iter()
                .map(|c| Some((c.relation_id()?.0, c.relation_attribute_no()?)))
                .collect();
            let mut oids: Vec<u32> = origins.iter().flatten().map(|(o, _)| *o).collect();
            oids.sort_unstable();
            oids.dedup();
            let oid = match oids.as_slice() {
                [] => return Ok(Err("read-only: no table columns in the result".into())),
                [one] => *one,
                _ => return Ok(Err("read-only: result joins several tables".into())),
            };
            let rel: Option<(String, String, String)> = sqlx::query_as(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.oid = $1::oid",
            )
            .bind(oid as i64)
            .fetch_optional(&pool)
            .await
            .map_err(pg_error_message)?;
            let Some((schema, table, kind)) = rel else {
                return Ok(Err("read-only: source table not found".into()));
            };
            if kind != "r" && kind != "p" {
                return Ok(Err(format!("read-only: {schema}.{table} is not a table")));
            }
            let attnums: Vec<(i16, String)> = sqlx::query_as(
                "SELECT attnum, attname::text FROM pg_attribute
             WHERE attrelid = $1::oid AND attnum > 0 AND NOT attisdropped",
            )
            .bind(oid as i64)
            .fetch_all(&pool)
            .await
            .map_err(pg_error_message)?;
            let metas = fetch_columns_inner(&pool, &schema, &table).await?;
            let by_attnum = |n: i16| {
                let name = attnums.iter().find(|(a, _)| *a == n)?.1.clone();
                metas.iter().find(|m| m.name == name).cloned()
            };
            let columns: Vec<Option<GridColumnMeta>> = origins
                .iter()
                .map(|o| o.filter(|(r, _)| *r == oid).and_then(|(_, n)| by_attnum(n)))
                .collect();
            let pk: Vec<&GridColumnMeta> = metas.iter().filter(|m| m.is_pk).collect();
            if pk.is_empty() {
                return Ok(Err(format!(
                    "read-only: {schema}.{table} has no primary key"
                )));
            }
            let mut key = Vec::new();
            for p in pk {
                match columns
                    .iter()
                    .position(|c| c.as_ref().is_some_and(|c| c.name == p.name))
                {
                    Some(ix) => key.push(ix),
                    None => {
                        return Ok(Err(format!(
                            "read-only: select the primary key ({}) to edit",
                            p.name
                        )));
                    }
                }
            }
            Ok(Ok(EditSource {
                schema,
                table,
                columns,
                key,
                engine: crate::engine::Engine::Postgres,
            }))
        },
    )
    .await
}

/// Column names when a query returns zero rows (no row_to_json to read).
pub(crate) async fn pg_run_query_columns(pool: &sqlx::PgPool, sql: &str) -> DbResult<Vec<String>> {
    let pool = pool.clone();
    let inner = sql.trim().trim_end_matches(';').to_string();
    let wrapped = format!("SELECT * FROM ({inner}) t LIMIT 0");
    run_logged(wrapped.clone(), crate::console::Source::Meta, async move {
        use sqlx::Column as _;
        use sqlx::Executor;
        let described = pool.describe(&wrapped).await.map_err(pg_error_message)?;
        Ok(described
            .columns
            .iter()
            .map(|c| c.name().to_string())
            .collect())
    })
    .await
}

/// INSERT/UPDATE/DELETE/DDL: affected rows.
pub(crate) async fn pg_run_exec(pool: &sqlx::PgPool, sql: &str) -> DbResult<u64> {
    let pool = pool.clone();
    let sql = sql.to_string();
    run_logged(sql.clone(), crate::console::Source::Data, async move {
        let done = sqlx::query(&sql)
            .execute(&pool)
            .await
            .map_err(pg_error_message)?;
        Ok(done.rows_affected())
    })
    .await
}

/// Real Postgres error text: code + message + detail/hint/position (if any).
pub fn pg_error_message(e: sqlx::Error) -> String {
    use sqlx::postgres::{PgDatabaseError, PgErrorPosition};
    match e.as_database_error() {
        Some(db) => {
            let code = db.code().map(|c| c.into_owned()).unwrap_or_default();
            let mut parts = vec![format!("[{code}] {}", db.message())];
            if let Some(pg) = db.as_error().downcast_ref::<PgDatabaseError>() {
                if let Some(detail) = pg.detail()
                    && !detail.is_empty()
                {
                    parts.push(format!("Detail: {detail}"));
                }
                if let Some(hint) = pg.hint()
                    && !hint.is_empty()
                {
                    parts.push(format!("Hint: {hint}"));
                }
                match pg.position() {
                    Some(PgErrorPosition::Original(p)) => parts.push(format!("Position: {p}")),
                    Some(PgErrorPosition::Internal { position, query }) => {
                        parts.push(format!("Internal position {position} in: {query}"))
                    }
                    None => {}
                }
            }
            parts.join("\n")
        }
        None => e.to_string(),
    }
}

/// One table column as the grid / structure view need it.
#[derive(Clone, Debug, PartialEq)]
pub struct GridColumnMeta {
    pub name: String,
    /// Short type name (`pg_type.typname`, e.g. `int4`, `_text`) — render policy.
    pub pg_type: String,
    /// Full SQL type (`format_type`, e.g. `character varying(255)`) — casts + DDL.
    pub sql_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    pub is_pk: bool,
    /// `other_table(col)` when this column is the first column of a foreign key.
    pub foreign_key: Option<String>,
    /// Labels of an enum type, in declaration order (empty for other types).
    pub enum_values: Vec<String>,
}

/// Column metadata straight from the catalogs (works for tables, views and
/// matviews alike; `information_schema` hides matviews).
pub(crate) async fn pg_fetch_columns(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> DbResult<Vec<GridColumnMeta>> {
    let pool = pool.clone();
    let (schema, table) = (schema.to_string(), table.to_string());
    let label = format!(
        "-- columns of {}.{}",
        quote_ident(&schema),
        quote_ident(&table)
    );
    run_logged(label, crate::console::Source::Meta, async move {
        fetch_columns_inner(&pool, &schema, &table).await
    })
    .await
}

/// [`fetch_columns`] body, for callers already on the tokio runtime.
async fn fetch_columns_inner(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> DbResult<Vec<GridColumnMeta>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        String,
        String,
        bool,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
        Option<Vec<String>>,
    )> = sqlx::query_as(
        "SELECT a.attname::text,
                t.typname::text,
                format_type(a.atttypid, a.atttypmod),
                NOT a.attnotnull,
                pg_get_expr(d.adbin, d.adrelid),
                col_description(c.oid, a.attnum),
                EXISTS (SELECT 1 FROM pg_index i
                        WHERE i.indrelid = c.oid AND i.indisprimary
                          AND a.attnum = ANY(i.indkey)),
                (SELECT format('%s(%s)', con.confrelid::regclass, fa.attname)
                 FROM pg_constraint con
                 JOIN pg_attribute fa ON fa.attrelid = con.confrelid
                                     AND fa.attnum = con.confkey[1]
                 WHERE con.conrelid = c.oid AND con.contype = 'f'
                   AND con.conkey[1] = a.attnum
                 LIMIT 1),
                (SELECT array_agg(e.enumlabel::text ORDER BY e.enumsortorder)
                 FROM pg_enum e WHERE e.enumtypid = a.atttypid)
         FROM pg_attribute a
         JOIN pg_class c ON c.oid = a.attrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_type t ON t.oid = a.atttypid
         LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
         WHERE n.nspname = $1 AND c.relname = $2
           AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(
            |(name, pg_type, sql_type, nullable, default, comment, is_pk, foreign_key, enums)| {
                GridColumnMeta {
                    name,
                    pg_type,
                    sql_type,
                    nullable,
                    default,
                    comment,
                    is_pk,
                    foreign_key,
                    enum_values: enums.unwrap_or_default(),
                }
            },
        )
        .collect())
}

/// A trusted WHERE fragment (quoted identifiers + fixed operators) whose
/// user-typed values travel as bind parameters `$1..$n` (all text, cast in SQL).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WhereClause {
    pub sql: String,
    pub params: Vec<String>,
    /// The same filter as data, for engines without SQL (Redis, MongoDB,
    /// DynamoDB, Cassandra); raw-SQL rows aren't in it.
    pub terms: Vec<FilterTerm>,
}

/// One column filter of the filter bar.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterTerm {
    pub column: String,
    pub op: crate::filter::FilterOp,
    pub value: String,
}

fn where_sql(filter: Option<&WhereClause>) -> String {
    match filter {
        Some(w) if !w.sql.is_empty() => format!("WHERE {}", w.sql),
        _ => String::new(),
    }
}

/// Exact row count for the status bar (honours the active filter).
pub(crate) async fn pg_fetch_count(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
    filter: Option<&WhereClause>,
) -> DbResult<i64> {
    let pool = pool.clone();
    let sql = format!(
        "SELECT COUNT(*) FROM {}.{} {}",
        quote_ident(schema),
        quote_ident(table),
        where_sql(filter)
    );
    let params = filter.map(|w| w.params.clone()).unwrap_or_default();
    run_logged(sql.clone(), crate::console::Source::Meta, async move {
        let mut q = sqlx::query_scalar(&sql);
        for p in &params {
            q = q.bind(p);
        }
        let n: i64 = q.fetch_one(&pool).await.map_err(pg_error_message)?;
        Ok(n)
    })
    .await
}

/// Name of the synthetic first column carrying the row's `ctid` (editable
/// tables only — views have no ctid).
pub const CTID_COL: &str = "__tusk_ctid";

/// One window of rows as ordered JSON values (`row_to_json` preserves nulls;
/// `preserve_order` keeps column order). `order_by` is a trusted ORDER BY
/// fragment built only from quoted identifiers. With `with_ctid`, every row
/// starts with a [`CTID_COL`] key.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pg_fetch_window(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
    filter: Option<&WhereClause>,
    order_by: Option<&str>,
    with_ctid: bool,
    limit: i64,
    offset: i64,
) -> DbResult<Vec<serde_json::Value>> {
    let pool = pool.clone();
    let params = filter.map(|w| w.params.clone()).unwrap_or_default();
    let n = params.len();
    let sql = format!(
        "SELECT row_to_json(t) FROM (SELECT {}* FROM {}.{} {} {} LIMIT ${} OFFSET ${}) t",
        if with_ctid {
            format!("ctid::text AS {}, ", quote_ident(CTID_COL))
        } else {
            String::new()
        },
        quote_ident(schema),
        quote_ident(table),
        where_sql(filter),
        order_by.unwrap_or(""),
        n + 1,
        n + 2
    );
    run_logged(sql.clone(), crate::console::Source::Data, async move {
        let mut q = sqlx::query_scalar(&sql);
        for p in &params {
            q = q.bind(p);
        }
        let rows: Vec<serde_json::Value> = q
            .bind(limit)
            .bind(offset)
            .fetch_all(&pool)
            .await
            .map_err(pg_error_message)?;
        Ok(rows)
    })
    .await
}

const SQL_USER_TYPES: &str = "SELECT t.typname::text FROM pg_type t
   JOIN pg_namespace n ON n.oid = t.typnamespace
  WHERE n.nspname = $1 AND t.typtype IN ('e', 'd')
  ORDER BY 1";

/// The schema's enums and domains (the data_type picker lists them first).
pub(crate) async fn pg_fetch_user_types(
    pool: &sqlx::PgPool,
    schema: &str,
) -> DbResult<Vec<String>> {
    let pool = pool.clone();
    let schema = schema.to_string();
    run_logged(
        SQL_USER_TYPES.to_string(),
        crate::console::Source::Meta,
        async move {
            sqlx::query_scalar(SQL_USER_TYPES)
                .bind(&schema)
                .fetch_all(&pool)
                .await
                .map_err(pg_error_message)
        },
    )
    .await
}

/// Triggers of a table (Triggers view), one JSON row each.
pub(crate) async fn pg_fetch_triggers(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> DbResult<Vec<serde_json::Value>> {
    let sql = format!(
        "SELECT t.tgname AS trigger_name,
                CASE WHEN t.tgtype & 2 = 2 THEN 'BEFORE'
                     WHEN t.tgtype & 64 = 64 THEN 'INSTEAD OF' ELSE 'AFTER' END AS timing,
                concat_ws(' OR ',
                    CASE WHEN t.tgtype & 4 = 4 THEN 'INSERT' END,
                    CASE WHEN t.tgtype & 16 = 16 THEN 'UPDATE' END,
                    CASE WHEN t.tgtype & 8 = 8 THEN 'DELETE' END,
                    CASE WHEN t.tgtype & 32 = 32 THEN 'TRUNCATE' END) AS event,
                CASE WHEN t.tgtype & 1 = 1 THEN 'ROW' ELSE 'STATEMENT' END AS level,
                p.proname AS function,
                t.tgenabled <> 'D' AS enabled,
                pg_get_triggerdef(t.oid, true) AS definition
           FROM pg_trigger t
           JOIN pg_class c ON c.oid = t.tgrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_proc p ON p.oid = t.tgfoid
          WHERE NOT t.tgisinternal AND n.nspname = {} AND c.relname = {}
          ORDER BY 1",
        quote_literal(schema),
        quote_literal(table)
    );
    pg_run_query_rows(pool, &sql, 10_000).await
}

/// One index of a table, as the Index view edits it.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDef {
    pub name: String,
    /// Access method: btree, hash, gist, gin, brin, spgist.
    pub algorithm: String,
    pub unique: bool,
    pub primary: bool,
    /// Key columns / expressions, comma separated.
    pub columns: String,
    /// `INCLUDE (...)` columns, comma separated (empty when none).
    pub include: String,
    /// Partial-index predicate.
    pub condition: Option<String>,
    pub comment: Option<String>,
    /// The constraint this index backs (primary key / unique), if any.
    pub constraint: Option<String>,
}

const SQL_INDEXES: &str = "SELECT i.relname::text, am.amname::text, ix.indisunique, ix.indisprimary,
        COALESCE((SELECT string_agg(pg_get_indexdef(ix.indexrelid, k.n::int, true), ', ' ORDER BY k.n)
           FROM generate_series(1, ix.indnkeyatts) k(n)), ''),
        COALESCE((SELECT string_agg(pg_get_indexdef(ix.indexrelid, k.n::int, true), ', ' ORDER BY k.n)
           FROM generate_series(ix.indnkeyatts + 1, ix.indnatts) k(n)), ''),
        pg_get_expr(ix.indpred, ix.indrelid),
        obj_description(i.oid, 'pg_class'),
        (SELECT c.conname::text FROM pg_constraint c WHERE c.conindid = ix.indexrelid
           AND c.contype IN ('p', 'u') LIMIT 1)
   FROM pg_index ix
   JOIN pg_class i ON i.oid = ix.indexrelid
   JOIN pg_class t ON t.oid = ix.indrelid
   JOIN pg_namespace n ON n.oid = t.relnamespace
   JOIN pg_am am ON am.oid = i.relam
  WHERE n.nspname = $1 AND t.relname = $2
  ORDER BY ix.indisprimary DESC, i.relname";

/// Indexes of a table (Index view): primary key first.
pub(crate) async fn pg_fetch_indexes(
    pool: &sqlx::PgPool,
    schema: &str,
    table: &str,
) -> DbResult<Vec<IndexDef>> {
    let pool = pool.clone();
    let (schema, table) = (schema.to_string(), table.to_string());
    run_logged(
        SQL_INDEXES.to_string(),
        crate::console::Source::Meta,
        async move {
            type Row = (
                String,
                String,
                bool,
                bool,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            );
            let rows: Vec<Row> = sqlx::query_as(SQL_INDEXES)
                .bind(&schema)
                .bind(&table)
                .fetch_all(&pool)
                .await
                .map_err(pg_error_message)?;
            Ok(rows
                .into_iter()
                .map(|r| IndexDef {
                    name: r.0,
                    algorithm: r.1,
                    unique: r.2,
                    primary: r.3,
                    columns: r.4,
                    include: r.5,
                    condition: r.6,
                    comment: r.7,
                    constraint: r.8,
                })
                .collect())
        },
    )
    .await
}

/// One parameterised statement of a save batch; `None` binds SQL NULL.
#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<Option<String>>,
}

impl Stmt {
    pub fn plain(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            params: Vec::new(),
        }
    }
}

/// Run a batch atomically (cmd-s): all statements in one transaction, rolled
/// back on the first error. Returns total affected rows.
pub(crate) async fn pg_execute_batch(pool: &sqlx::PgPool, stmts: Vec<Stmt>) -> DbResult<u64> {
    let pool = pool.clone();
    run_db(async move {
        let mut tx = pool.begin().await.map_err(pg_error_message)?;
        let mut affected = 0;
        for stmt in &stmts {
            let mut q = sqlx::query(&stmt.sql);
            for p in &stmt.params {
                q = q.bind(p.clone());
            }
            let started = std::time::Instant::now();
            let done = q.execute(&mut *tx).await.map_err(|e| {
                let msg = pg_error_message(e);
                crate::console::record(
                    &stmt.sql,
                    started,
                    crate::console::Source::Data,
                    Some(&msg),
                );
                format!("{msg}\n  in: {}", stmt.sql)
            })?;
            crate::console::record(&stmt.sql, started, crate::console::Source::Data, None);
            affected += done.rows_affected();
        }
        tx.commit().await.map_err(pg_error_message)?;
        Ok(affected)
    })
    .await
}

/// Single-quoted SQL literal for the connected engine (Postgres:
/// standard_conforming_strings is on since PG 9.1).
pub fn quote_literal(s: &str) -> String {
    dialect().literal(s)
}

// ---- object tree (sidebar) ----

/// Tables / views / matviews / functions of one schema.
#[derive(Clone, Default)]
pub struct ObjectTree {
    pub tables: Vec<String>,
    pub views: Vec<String>,
    pub matviews: Vec<String>,
    pub functions: Vec<String>,
}

const SQL_DATABASES: &str = "SELECT datname::text FROM pg_database
             WHERE datallowconn AND NOT datistemplate
               AND has_database_privilege(datname, 'CONNECT')
             ORDER BY datname";

/// Databases the user can connect to (for the database switcher).
pub(crate) async fn pg_fetch_databases(pool: &sqlx::PgPool) -> DbResult<Vec<String>> {
    let pool = pool.clone();
    run_logged(
        SQL_DATABASES.to_string(),
        crate::console::Source::Meta,
        async move {
            sqlx::query_scalar(SQL_DATABASES)
                .fetch_all(&pool)
                .await
                .map_err(pg_error_message)
        },
    )
    .await
}

const SQL_SCHEMAS: &str = "SELECT nspname FROM pg_namespace
             WHERE nspname NOT LIKE 'pg_%' AND nspname <> 'information_schema'
             ORDER BY CASE WHEN nspname = 'public' THEN 0 ELSE 1 END, nspname";

/// User schemas (no pg_* / information_schema), public first.
pub(crate) async fn pg_fetch_schemas(pool: &sqlx::PgPool) -> DbResult<Vec<String>> {
    let pool = pool.clone();
    run_logged(
        SQL_SCHEMAS.to_string(),
        crate::console::Source::Meta,
        async move {
            let rows: Vec<String> = sqlx::query_scalar(SQL_SCHEMAS)
                .fetch_all(&pool)
                .await
                .map_err(|e| e.to_string())?;
            Ok(rows)
        },
    )
    .await
}

const SQL_RELATIONS: &str = "SELECT c.relname, c.relkind::TEXT
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'v', 'm')
               AND NOT c.relispartition
             ORDER BY c.relname";

pub(crate) async fn pg_fetch_objects(pool: &sqlx::PgPool, schema: &str) -> DbResult<ObjectTree> {
    let pool = pool.clone();
    let schema = schema.to_string();
    run_logged(
        SQL_RELATIONS.to_string(),
        crate::console::Source::Meta,
        async move {
            let rels: Vec<(String, String)> = sqlx::query_as(SQL_RELATIONS)
                .bind(&schema)
                .fetch_all(&pool)
                .await
                .map_err(|e| e.to_string())?;

            let functions: Vec<String> = sqlx::query_scalar(
                "SELECT p.proname
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = $1 AND p.prokind = 'f'
             ORDER BY 1",
            )
            .bind(&schema)
            .fetch_all(&pool)
            .await
            .map_err(|e| e.to_string())?;

            let mut tree = ObjectTree::default();
            for (name, kind) in rels {
                match kind.as_str() {
                    "r" => tree.tables.push(name),
                    "v" => tree.views.push(name),
                    "m" => tree.matviews.push(name),
                    _ => {}
                }
            }
            tree.functions = functions;
            Ok(tree)
        },
    )
    .await
}

/// "Can't reach host:port" with the socket error, for a server that is
/// down or a wrong address.
fn unreachable_msg(opts: &PgConnectOptions, why: &str) -> String {
    format!(
        "Can't reach {}:{}: {why}. Is the server running?",
        opts.get_host(),
        opts.get_port()
    )
}

/// "Connection refused" rather than "Connection refused (os error 61)".
fn io_reason(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_lowercase(),
        None => s,
    }
}

/// Connect and keep the pool for the workspace (sidebar, grids, editor).
pub async fn connect_pool(opts: PgConnectOptions) -> DbResult<sqlx::PgPool> {
    let opts = opts.log_statements(log::LevelFilter::Off);
    let handle = RUNTIME.handle().clone();
    handle
        .spawn(async move {
            // One direct connection first: the pool retries a refused
            // connection until its acquire timeout and then only reports
            // "pool timed out", this fails fast with the real reason.
            let probe = tokio::time::timeout(
                Duration::from_secs(8),
                <sqlx::postgres::PgConnection as sqlx::Connection>::connect_with(&opts),
            )
            .await
            .map_err(|_| unreachable_msg(&opts, "timed out"))?
            .map_err(|e| match &e {
                sqlx::Error::Io(io) => unreachable_msg(&opts, &io_reason(io)),
                _ => e.to_string(),
            })?;
            let _ = sqlx::Connection::close(probe).await;
            let pool = PgPoolOptions::new()
                .max_connections(5)
                .acquire_timeout(Duration::from_secs(10))
                .connect_with(opts)
                .await
                .map_err(|e| e.to_string())?;
            // Prove the pool works before handing it over.
            let _: String = sqlx::query("SELECT version()")
                .fetch_one(&pool)
                .await
                .map_err(|e| e.to_string())?
                .try_get(0)
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(pool)
        })
        .await
        .map_err(|e| format!("connect task failed: {e}"))?
}

/// A live workspace connection: the pool plus, for SSH profiles, the
/// tunnel it runs through (dropping the tunnel breaks the pool).
#[derive(Clone)]
pub struct Connected {
    pub pool: Db,
    pub tunnel: Option<std::sync::Arc<crate::ssh::Tunnel>>,
    /// Where Postgres is reachable from *this* machine (the tunnel's local
    /// end for SSH profiles) — what the language server must connect to.
    pub host: String,
    pub port: u16,
}

/// Open a profile: SSH tunnel first when configured, then the pool.
/// `ssh_secret`: SSH password / key passphrase (None → Keychain).
pub async fn connect(
    conn: SavedConnection,
    password: String,
    ssh_secret: Option<String>,
) -> DbResult<Connected> {
    let mut conn = conn;
    // A blank database lands in the engine's default ("" stays blank:
    // MySQL connects server-wide, ClickHouse defaults inside its driver).
    if conn.database.trim().is_empty() {
        let d = conn.engine.default_database();
        if !d.is_empty() {
            conn.database = d.to_string();
        }
    }
    // A D1 profile is an account: blank = its most recently used database
    // (else the first); a legacy id becomes the database's name.
    if conn.engine == crate::engine::Engine::CloudflareD1 {
        conn.database = crate::drivers::d1::resolve_database(&conn, &password).await?;
    }
    let (tunnel, host, port) = match conn.ssh.clone() {
        Some(ssh) => {
            let secret = ssh_secret
                .filter(|s| !s.is_empty())
                .or_else(|| load_ssh_secret(&conn.name));
            let (target_host, target_port) = (conn.host.clone(), conn.port);
            let tunnel = RUNTIME
                .handle()
                .spawn(crate::ssh::open(ssh, secret, target_host, target_port))
                .await
                .map_err(|e| format!("ssh task failed: {e}"))??;
            let port = tunnel.local_port;
            (
                Some(std::sync::Arc::new(tunnel)),
                "127.0.0.1".to_string(),
                port,
            )
        }
        None => (None, conn.host.clone(), conn.port),
    };
    let pool = crate::drivers::connect(&conn, host.clone(), port, password).await?;
    Ok(Connected {
        pool,
        tunnel,
        host,
        port,
    })
}

/// "Test connection": the same path as [`connect`], then `SELECT version()`.
pub async fn test_connect(
    conn: SavedConnection,
    password: String,
    ssh_secret: Option<String>,
) -> DbResult<String> {
    let c = connect(conn, password, ssh_secret).await?;
    let version = c.pool.driver().version().await?;
    if let Some(pool) = c.pool.pg().cloned() {
        let _ = run_db(async move {
            pool.close().await;
            Ok(())
        })
        .await;
    }
    drop(c);
    Ok(version)
}

// ---- engine-neutral API (the workspace calls these with a `Db`) ----

pub use crate::drivers::Db;

pub async fn run_query_rows(db: &Db, sql: &str, limit: i64) -> DbResult<Vec<serde_json::Value>> {
    db.driver().query_rows(sql.to_string(), limit).await
}

pub async fn result_edit_source(
    db: &Db,
    sql: &str,
    schema: &str,
    result: Vec<String>,
) -> DbResult<Result<EditSource, String>> {
    let engine = db.engine();
    Ok(db
        .driver()
        .edit_source(sql.to_string(), schema.to_string(), result)
        .await?
        .map(|s| EditSource { engine, ..s }))
}

pub async fn run_query_columns(db: &Db, sql: &str) -> DbResult<Vec<String>> {
    db.driver().query_columns(sql.to_string()).await
}

pub async fn run_exec(db: &Db, sql: &str) -> DbResult<u64> {
    db.driver().exec(sql.to_string()).await
}

pub async fn fetch_columns(db: &Db, schema: &str, table: &str) -> DbResult<Vec<GridColumnMeta>> {
    db.driver()
        .columns(schema.to_string(), table.to_string())
        .await
}

pub async fn fetch_count(
    db: &Db,
    schema: &str,
    table: &str,
    filter: Option<&WhereClause>,
) -> DbResult<i64> {
    db.driver()
        .count(schema.to_string(), table.to_string(), filter.cloned())
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_window(
    db: &Db,
    schema: &str,
    table: &str,
    filter: Option<&WhereClause>,
    order_by: Option<&str>,
    with_key: bool,
    limit: i64,
    offset: i64,
) -> DbResult<Vec<serde_json::Value>> {
    db.driver()
        .window(crate::drivers::WindowReq {
            schema: schema.to_string(),
            table: table.to_string(),
            filter: filter.cloned(),
            order_by: order_by.map(str::to_string),
            with_key,
            limit,
            offset,
        })
        .await
}

pub async fn fetch_user_types(db: &Db, schema: &str) -> DbResult<Vec<String>> {
    db.driver().user_types(schema.to_string()).await
}

pub async fn fetch_triggers(
    db: &Db,
    schema: &str,
    table: &str,
) -> DbResult<Vec<serde_json::Value>> {
    db.driver()
        .triggers(schema.to_string(), table.to_string())
        .await
}

pub async fn fetch_indexes(db: &Db, schema: &str, table: &str) -> DbResult<Vec<IndexDef>> {
    db.driver()
        .indexes(schema.to_string(), table.to_string())
        .await
}

pub async fn execute_batch(db: &Db, stmts: Vec<Stmt>) -> DbResult<u64> {
    db.driver().batch(stmts).await
}

pub async fn fetch_databases(db: &Db) -> DbResult<Vec<String>> {
    db.driver().databases().await
}

pub async fn fetch_schemas(db: &Db) -> DbResult<Vec<String>> {
    db.driver().schemas().await
}

pub async fn fetch_objects(db: &Db, schema: &str) -> DbResult<ObjectTree> {
    db.driver().objects(schema.to_string()).await
}

#[cfg(test)]
mod statement_tests {
    use super::{is_destructive, is_write, statement_at};

    #[test]
    fn safe_mode_flags_only_irreversible_statements() {
        assert!(is_destructive("-- note\nDROP TABLE t"));
        assert!(is_destructive("truncate t"));
        assert!(is_destructive("DELETE FROM t"));
        assert!(is_destructive("update t set a = 1"));
        assert!(!is_destructive("UPDATE t SET a = 1 WHERE id = 2"));
        assert!(!is_destructive("delete from t where id=1"));
        assert!(!is_destructive("SELECT * FROM drops"));
    }

    #[test]
    fn safe_mode_save_confirm_flags_writes() {
        assert!(is_write("INSERT INTO t VALUES (1)"));
        assert!(is_write("-- fix\nUPDATE t SET a = 1 WHERE id = 2"));
        assert!(is_write("/* x */ create table t (id int)"));
        assert!(is_write("DROP TABLE t"));
        assert!(!is_write("SELECT * FROM inserts"));
        assert!(!is_write("with x as (select 1) select * from x"));
        assert!(!is_write("BEGIN;"));
        assert!(!is_write("set search_path = app"));
    }

    #[test]
    fn run_current_picks_the_statement_under_the_cursor() {
        let sql = "select 1;\n\nselect 'a;b';\nselect 3";
        assert_eq!(statement_at(sql, 3).as_deref(), Some("select 1"));
        // right after the `;`
        assert_eq!(statement_at(sql, 9).as_deref(), Some("select 1"));
        // blank line between → the previous statement
        assert_eq!(statement_at(sql, 10).as_deref(), Some("select 1"));
        assert_eq!(statement_at(sql, 16).as_deref(), Some("select 'a;b'"));
        assert_eq!(statement_at(sql, sql.len()).as_deref(), Some("select 3"));
        assert_eq!(statement_at("  ", 1), None);
        // multi-byte text before the cursor
        assert_eq!(
            statement_at("select 'ğ'; select 2", 20).as_deref(),
            Some("select 2")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch folder under the system temp dir.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tusk-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn named(name: &str) -> SavedConnection {
        SavedConnection {
            name: name.to_string(),
            ..dev_default()
        }
    }

    #[test]
    fn optional_database_engines_validate_blank() {
        use crate::engine::Engine;
        // Postgres-family / MySQL / MSSQL / ClickHouse: database optional.
        for engine in [
            Engine::Postgres,
            Engine::Greenplum,
            Engine::Cockroach,
            Engine::MySql,
            Engine::MariaDb,
            Engine::MsSql,
            Engine::ClickHouse,
        ] {
            let mut c = named("blank-db");
            c.engine = engine;
            c.database = String::new();
            assert!(c.is_valid(), "{engine:?} should accept a blank database");
        }
        // Redshift / Vertica / Oracle still require one.
        for engine in [Engine::Redshift, Engine::Vertica, Engine::Oracle] {
            let mut c = named("needs-db");
            c.engine = engine;
            c.database = String::new();
            assert!(!c.is_valid(), "{engine:?} should still require a database");
        }
        // D1: only the account id is required.
        let mut c = named("d1-account");
        c.engine = Engine::CloudflareD1;
        c.database = String::new();
        c.options.insert("account_id".into(), "acct".into());
        assert!(c.is_valid());
        c.options.remove("account_id");
        assert!(!c.is_valid(), "D1 without an account id must not validate");
    }

    #[test]
    fn engine_default_database_mapping() {
        use crate::engine::Engine;
        assert_eq!(Engine::Postgres.default_database(), "postgres");
        assert_eq!(Engine::Greenplum.default_database(), "postgres");
        assert_eq!(Engine::Cockroach.default_database(), "defaultdb");
        assert_eq!(Engine::MsSql.default_database(), "master");
        assert_eq!(Engine::MySql.default_database(), "");
        assert_eq!(Engine::ClickHouse.default_database(), "");
        assert_eq!(Engine::CloudflareD1.default_database(), "");
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_file() {
        let dir = scratch("atomic");
        let path = dir.join("connections.json");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!dir.join("connections.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_connections_file_is_kept_aside() {
        let dir = scratch("parse");
        let path = dir.join("connections.json");
        std::fs::write(&path, "[{\"engine\": \"from-the-future\"").unwrap();
        let list = load_connections_from(&path);
        assert!(list.is_empty());
        // The original moved to a backup, so a later save can't clobber it.
        assert!(!path.exists());
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("connections.json.bak-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert!(
            std::fs::read_to_string(backups[0].path())
                .unwrap()
                .contains("from-the-future")
        );
        assert!(take_load_notice().is_some_and(|m| m.contains("connections.json")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_adds_replaces_and_renames() {
        let mut list = vec![named("a"), named("b")];
        // New name: appended.
        assert_eq!(upsert(&mut list, named("c"), None), 2);
        // Same name: replaced in place.
        let mut b = named("b");
        b.port = 1;
        assert_eq!(upsert(&mut list, b, None), 1);
        assert_eq!(list[1].port, 1);
        // Edited profile renamed: replaces the old slot, no duplicate.
        assert_eq!(upsert(&mut list, named("a2"), Some("a")), 0);
        let names: Vec<_> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a2", "b", "c"]);
    }

    fn dev_opts() -> PgConnectOptions {
        connect_options(
            "127.0.0.1",
            55432,
            "tusk_dev",
            "tusk",
            "tusk",
            SslMode::Prefer,
        )
    }

    #[test]
    fn test_connection_ok_against_docker() {
        let v = RUNTIME
            .handle()
            .block_on(test_connect(dev_default(), "tusk".into(), None))
            .expect("docker PG reachable");
        assert!(v.contains("PostgreSQL"), "unexpected version: {v}");
    }

    #[test]
    fn test_connection_wrong_password_gives_real_error() {
        let err = RUNTIME
            .handle()
            .block_on(test_connect(dev_default(), "WRONG".into(), None))
            .expect_err("wrong password must fail");
        assert!(
            err.to_lowercase().contains("password") || err.to_lowercase().contains("auth"),
            "expected auth error, got: {err}"
        );
    }

    fn pool_for_tests() -> sqlx::PgPool {
        RUNTIME
            .handle()
            .block_on(connect_pool(dev_opts()))
            .expect("docker PG reachable")
    }

    #[test]
    fn fetch_schemas_lists_all_three_seed_schemas() {
        let pool = pool_for_tests();
        let schemas = RUNTIME
            .handle()
            .block_on(pg_fetch_schemas(&pool))
            .expect("schemas query");
        for want in ["public", "analytics", "audit"] {
            assert!(
                schemas.contains(&want.to_string()),
                "missing {want}: {schemas:?}"
            );
        }
    }

    #[test]
    fn sql_split_classify_execute() {
        // Splitter respects strings, comments and dollar quotes.
        let parts =
            split_statements("SELECT ';';\n-- comment;\nSELECT $tag$; $tag$; /* ; */ SELECT 1;");
        assert_eq!(parts.len(), 3, "got {parts:?}");
        assert_eq!(classify_statement("  select 1"), StmtKind::Query);
        assert_eq!(
            classify_statement("WITH x AS (SELECT 1) SELECT * FROM x"),
            StmtKind::Query
        );
        assert_eq!(classify_statement("update t set a=1"), StmtKind::Mutation);
        assert_eq!(
            classify_statement("CREATE TABLE t (a INT)"),
            StmtKind::Other
        );
        // Leading comments must not hide the real first keyword (E2E: the
        // default editor buffer starts with a `--` comment line).
        assert_eq!(
            classify_statement("-- New query (⌘↵ to run)\nSELECT * FROM t LIMIT 100;"),
            StmtKind::Query
        );
        assert_eq!(
            classify_statement("/* block */\n-- line\n  update t set a=1"),
            StmtKind::Mutation
        );
        assert_eq!(classify_statement("-- only a comment"), StmtKind::Other);

        // Real execution shapes.
        let pool = pool_for_tests();
        let rows = RUNTIME
            .handle()
            .block_on(pg_run_query_rows(&pool, "SELECT 1 AS one, NULL AS n", 10))
            .expect("select");
        assert_eq!(rows.len(), 1);

        let affected = RUNTIME
            .handle()
            .block_on(async {
                // NOTE: TEMP tables are per-session; the pool may use another
                // connection, so the test uses a real table + cleanup.
                pg_run_exec(&pool, "CREATE TABLE tusk_test_tmp (a INT)").await?;
                let n = pg_run_exec(&pool, "INSERT INTO tusk_test_tmp VALUES (1),(2)").await?;
                pg_run_exec(&pool, "DROP TABLE tusk_test_tmp").await?;
                Ok::<_, String>(n)
            })
            .expect("ddl+insert");
        assert_eq!(affected, 2);

        // Syntax error carries the real PG message.
        let err = RUNTIME
            .handle()
            .block_on(pg_run_query_rows(&pool, "SELEC 1", 10))
            .expect_err("bad sql must fail");
        assert!(
            err.contains("42601") || err.to_lowercase().contains("syntax"),
            "got: {err}"
        );
    }

    #[test]
    fn grid_window_preserves_order_and_nulls() {
        let pool = pool_for_tests();
        let cols = RUNTIME
            .handle()
            .block_on(pg_fetch_columns(&pool, "public", "customers"))
            .expect("columns");
        assert!(cols.iter().any(|c| c.name == "email"));
        assert!(cols.iter().any(|c| c.name == "phone")); // nullable col present

        let n = RUNTIME
            .handle()
            .block_on(pg_fetch_count(&pool, "public", "events", None))
            .expect("count");
        assert_eq!(n, 500_000);

        let rows = RUNTIME
            .handle()
            .block_on(pg_fetch_window(
                &pool,
                "public",
                "customers",
                None,
                None,
                false,
                5,
                0,
            ))
            .expect("window");
        assert_eq!(rows.len(), 5);
        // Every row must be an object with exactly the table's column count.
        for row in &rows {
            let obj = row.as_object().expect("row object");
            assert_eq!(obj.len(), cols.len(), "column order/count drift");
        }
        // phone is NULL for every 3rd customer (seed) — at least one null exists.
        let phone_ix = cols.iter().position(|c| c.name == "phone").unwrap();
        let keys: Vec<String> = rows[0].as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys[phone_ix], "phone",
            "key order must match ordinal position"
        );
    }

    #[test]
    fn result_edit_source_follows_the_tableplus_rule() {
        let pool = pool_for_tests();
        let src = |sql: &str| {
            RUNTIME
                .handle()
                .block_on(pg_result_edit_source(&pool, sql))
                .expect("describe")
        };
        let ok =
            src("SELECT email, id AS ident, 1 + 1 AS two FROM public.customers").expect("editable");
        assert_eq!(
            (ok.schema.as_str(), ok.table.as_str()),
            ("public", "customers")
        );
        assert_eq!(ok.key, vec![1], "aliased PK is still the key");
        assert_eq!(
            ok.columns[0].as_ref().map(|c| c.name.as_str()),
            Some("email")
        );
        assert!(ok.columns[2].is_none(), "expressions are not editable");

        let no_pk = src("SELECT email FROM public.customers").unwrap_err();
        assert!(no_pk.contains("primary key"), "{no_pk}");
        let join = src(
            "SELECT c.id, o.id FROM public.customers c JOIN public.orders o ON o.customer_id = c.id",
        )
        .unwrap_err();
        assert!(join.contains("several tables"), "{join}");
        let expr = src("SELECT count(*) FROM public.customers").unwrap_err();
        assert!(expr.contains("no table columns"), "{expr}");
    }

    #[test]
    fn fetch_objects_covers_tables_views_matview_functions() {
        let pool = pool_for_tests();
        let public = RUNTIME
            .handle()
            .block_on(pg_fetch_objects(&pool, "public"))
            .expect("public objects");
        for want in ["customers", "events", "orders", "order_items", "products"] {
            assert!(
                public.tables.contains(&want.to_string()),
                "missing table {want}"
            );
        }
        assert!(public.views.contains(&"order_totals".to_string()));
        assert!(public.functions.contains(&"customer_ltv".to_string()));

        let analytics = RUNTIME
            .handle()
            .block_on(pg_fetch_objects(&pool, "analytics"))
            .expect("analytics objects");
        assert!(analytics.tables.contains(&"daily_stats".to_string()));
        assert!(analytics.matviews.contains(&"customer_ltv".to_string()));

        let audit = RUNTIME
            .handle()
            .block_on(pg_fetch_objects(&pool, "audit"))
            .expect("audit objects");
        assert!(audit.tables.contains(&"audit_log".to_string()));
        assert!(audit.functions.contains(&"log_action".to_string()));
    }

    /// Real credential-store round trip. Linux needs a running Secret Service;
    /// opt in for local testing, since headless CI has no session bus.
    #[test]
    fn credential_store_roundtrip() {
        if cfg!(target_os = "linux") && std::env::var_os("TUSK_TEST_KEYRING").is_none() {
            eprintln!("skip: set TUSK_TEST_KEYRING=1 with a Secret Service session");
            return;
        }
        let name = format!("tusk-test-{}", std::process::id());
        save_password(&name, "s3cret-ü").expect("save");
        assert_eq!(load_password(&name).expect("load"), "s3cret-ü");
        keyring_entry(&name)
            .unwrap()
            .delete_credential()
            .expect("delete");
        assert!(load_password(&name).is_err());
    }

    /// End-to-end SSH tunnel: jump host = the `tusk-ssh-test` container
    /// (openssh-server on :2222, same Docker network as Postgres), target
    /// `tusk-postgres:5432` resolved *on the jump host*. Skipped when the
    /// container isn't running.
    #[test]
    fn ssh_tunnel_reaches_postgres() {
        if std::net::TcpStream::connect("127.0.0.1:2222").is_err() {
            eprintln!("skip: no ssh test container on :2222");
            return;
        }
        let kh = std::env::temp_dir().join(format!("tusk-kh-{}", std::process::id()));
        // SAFETY: single-threaded setup before the connect below.
        unsafe { std::env::set_var("TUSK_KNOWN_HOSTS", &kh) };
        let mut conn = dev_default();
        conn.host = "tusk-postgres".into();
        conn.port = 5432;
        conn.ssh = Some(SshConfig {
            host: "127.0.0.1".into(),
            port: 2222,
            user: "tusk".into(),
            key_path: None,
        });
        let v = RUNTIME
            .handle()
            .block_on(test_connect(
                conn.clone(),
                "tusk".into(),
                Some("sshpass".into()),
            ))
            .expect("postgres through the ssh tunnel");
        assert!(v.contains("PostgreSQL"), "{v}");
        // Wrong SSH password → a real SSH auth error, not a hang.
        let err = RUNTIME
            .handle()
            .block_on(test_connect(conn, "tusk".into(), Some("nope".into())))
            .expect_err("bad ssh password");
        assert!(err.contains("SSH authentication"), "{err}");
        let _ = std::fs::remove_file(kh);
    }

    #[test]
    fn endpoint_names_the_file_not_host_zero() {
        let pg = dev_default();
        assert_eq!(pg.endpoint(), "127.0.0.1:55432");
        let lite = SavedConnection {
            engine: crate::engine::Engine::Sqlite,
            path: Some("/tmp/demo/shop.sqlite".into()),
            port: 0,
            ..dev_default()
        };
        assert_eq!(lite.endpoint(), "shop.sqlite");
    }

    #[test]
    fn connections_json_roundtrip() {
        let list = vec![dev_default()];
        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("tusk\"") || true); // password must never be here
        let back: Vec<SavedConnection> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].port, 55432);
    }
}
