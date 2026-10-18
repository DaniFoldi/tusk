//! The AI panel (right side, status-bar sparkles / ⌘L): a chat with an ACP
//! agent that knows the open database through Tusk's MCP tools and hands
//! its SQL to the user in a query tab — running it stays with the user.
//! Child module of `app`.

use gpui_kit::component::Selectable as _;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::text::TextView;
use serde_json::{Value, json};

use super::*;
use crate::acp_registry::{self, AgentSpec};
use crate::agent::{
    AgentThread, ConfigKind, Entry, SessionSummary, Status, ThreadEvent, ToolContent,
};
use crate::mcp_bridge::{Host, ToolRequest};

const MIN_W: f32 = 320.;

/// Something sent to the chat from elsewhere: a row, a query, a table's DDL…
#[derive(Clone, Debug)]
pub struct Attachment {
    pub label: String,
    pub text: String,
    /// Code fence language for the agent (`sql`, `json`, …).
    pub lang: &'static str,
}

#[derive(Default)]
pub struct AiPanel {
    pub open: bool,
    pub width: f32,
    pub drag: Option<(f32, f32)>,
    pub thread: Option<Entity<AgentThread>>,
    scroller: Option<Entity<MessageScrollerState>>,
    input: Option<Entity<TextareaState>>,
    pub agents: std::rc::Rc<Vec<AgentSpec>>,
    sessions: Vec<SessionSummary>,
    /// The MCP bridge host of this window (see start_mcp_host); app code
    /// publishes the connection identity through it.
    pub(crate) host: Option<Host>,
    /// Highlighted row of the `/` command or `@` table list.
    pick: usize,
    plan_open: bool,
    /// The earlier-chats list instead of the conversation.
    history_open: bool,
    /// "Send to Chat" items waiting in the composer (sent as context).
    attachments: Vec<Attachment>,
    /// The session option whose picker is open, and the picker.
    open_picker: Option<(String, Entity<OptionPicker>)>,
    _subs: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

fn with_app(cx: &mut App, f: impl FnOnce(&mut TuskApp, &mut Context<TuskApp>)) {
    let view = cx.global::<TuskHandle>().0.clone();
    view.update(cx, f);
}

/// What the composer's popup offers for the text so far.
enum Suggest {
    Commands(Vec<(String, String)>),
    Tables(Vec<String>),
}

/// Line diff for tool-call previews: unchanged head / tail trimmed, the
/// middle shown as removed + added lines (with a little context).
fn diff_lines(old: &str, new: &str) -> Vec<(char, String)> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let ctx = 2;
    let mut out = Vec::new();
    for l in &a[pre.saturating_sub(ctx)..pre] {
        out.push((' ', l.to_string()));
    }
    for l in &a[pre..a.len() - suf] {
        out.push(('-', l.to_string()));
    }
    for l in &b[pre..b.len() - suf] {
        out.push(('+', l.to_string()));
    }
    for l in b[b.len() - suf..].iter().take(ctx) {
        out.push((' ', l.to_string()));
    }
    out
}

fn kind_icon(kind: &str) -> IconName {
    match kind {
        "read" => IconName::FileText,
        "edit" => IconName::FilePen,
        "delete" => IconName::Trash,
        "move" => IconName::FolderInput,
        "search" => IconName::Search,
        "execute" => IconName::SquareTerminal,
        "think" => IconName::Brain,
        "fetch" => IconName::Globe,
        "switch_mode" => IconName::ListTodo,
        _ => IconName::Wrench,
    }
}

/// `mcp__tusk__open_sql_tab` / `tusk/open_sql_tab` / … → the tool name.
fn tusk_tool(title: &str) -> Option<&'static str> {
    [
        "open_sql_tab",
        "replace_active_query",
        "get_active_query",
        "get_context",
        "list_tables",
        "describe_table",
    ]
    .into_iter()
    .find(|t| title.contains(t))
}

impl TuskApp {
    fn ai_width(&self) -> f32 {
        if self.ai.width < MIN_W {
            440.
        } else {
            self.ai.width
        }
    }

    /// ⌘L / the status-bar sparkles.
    pub(super) fn toggle_ai_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ai.open = !self.ai.open;
        if self.ai.open {
            // One right-side panel at a time.
            if self.row_panel.open {
                self.toggle_row_detail(cx);
            }
            self.ensure_ai(window, cx);
            if let Some(i) = &self.ai.input {
                i.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        cx.notify();
    }

    /// "Send to Chat": attach `text` to the composer. With the panel closed,
    /// a new chat starts (with the agent used last).
    pub fn send_to_chat(
        &mut self,
        label: impl Into<String>,
        text: impl Into<String>,
        lang: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fresh = !self.ai.open;
        if !self.ai.open {
            self.toggle_ai_panel(window, cx);
        } else {
            self.ensure_ai(window, cx);
        }
        if fresh
            && let Some(t) = &self.ai.thread
            && !t.read(cx).entries.is_empty()
        {
            t.update(cx, |t, cx| t.reset_chat(cx));
            self.ai.attachments.clear();
        }
        self.ai.history_open = false;
        self.ai.attachments.push(Attachment {
            label: label.into(),
            text: text.into(),
            lang,
        });
        if let Some(i) = &self.ai.input {
            i.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// ⇧⌘L / palette: send what is focused — the query (or its selection),
    /// the selected row, or the table's structure.
    pub(super) fn send_active_to_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Sql(t)) => {
                let ed = t.editor.read(cx);
                let sel = ed.selected_text().to_string();
                let (label, text) = if sel.trim().is_empty() {
                    (format!("Query \"{}\"", t.title), ed.text().to_string())
                } else {
                    (format!("Selection from \"{}\"", t.title), sel)
                };
                if text.trim().is_empty() {
                    self.toast_info("The query is empty.");
                    cx.notify();
                    return;
                }
                self.send_to_chat(label, text, "sql", window, cx);
            }
            Some(WorkspaceTab::Grid(g)) => {
                let (schema, name, kind) = (
                    g.table.schema.clone(),
                    g.table.name.clone(),
                    g.table.kind.clone(),
                );
                let row = (g.view == TabView::Data)
                    .then(|| {
                        let st = g.state.read(cx);
                        let r = st.selected_row().or(st.selected_cell().map(|(r, _)| r))?;
                        st.delegate().row_json(r)
                    })
                    .flatten();
                match row {
                    Some(json) => {
                        self.send_to_chat(format!("Row of {name}"), json, "json", window, cx)
                    }
                    None => self.send_table_to_chat(kind, schema, name, window, cx),
                }
            }
            None => {
                self.toggle_ai_panel(window, cx);
            }
        }
    }

    /// A table / view's CREATE statement as chat context.
    pub(super) fn send_table_to_chat(
        &mut self,
        kind: TableKind,
        schema: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let obj = match kind {
            TableKind::View => crate::objects::ObjKind::View,
            TableKind::MaterializedView => crate::objects::ObjKind::MatView,
            TableKind::Function => crate::objects::ObjKind::Function,
            TableKind::Table => crate::objects::ObjKind::Table,
        };
        let t = cx.spawn_in(window, async move |this, cx| {
            let ddl =
                crate::objects::script(&pool, obj, &schema, &name, crate::objects::Script::Create)
                    .await;
            let _ = this.update_in(cx, |this, window, cx| match ddl {
                Ok(ddl) => this.send_to_chat(format!("{schema}.{name}"), ddl, "sql", window, cx),
                Err(e) => {
                    this.toast(false, e);
                    cx.notify();
                }
            });
        });
        self.ai._tasks.push(t);
    }

    fn open_option_picker(
        &mut self,
        id: String,
        options: Vec<crate::agent::SelectOption>,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.ai.thread.as_ref().map(Entity::downgrade) else {
            return;
        };
        let picker =
            cx.new(|cx| OptionPicker::new(thread, id.clone(), options, current, window, cx));
        let focus = picker.read(cx).query.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        self.ai.open_picker = Some((id, picker));
        cx.notify();
    }

    /// The MCP bridge starts with the app, not with the AI panel: `tusk mcp`
    /// must find any running Tusk — welcome screen included. Tools answer
    /// "No database connection is open in Tusk." until one is.
    pub fn start_mcp_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai.host.is_some() {
            return;
        }
        match Host::start() {
            Ok((host, mut rx)) => {
                self.ai.host = Some(host);
                let t = cx.spawn_in(window, async move |this, cx| {
                    while let Some(req) = rx.recv().await {
                        if this
                            .update_in(cx, |this, window, cx| this.answer_tool(req, window, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                });
                self.ai._tasks.push(t);
            }
            Err(e) => self.toast(false, format!("AI tools unavailable: {e}")),
        }
    }

    fn ensure_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai.agents.is_empty() {
            self.ai.agents = self.agent_list(acp_registry::cached()).into();
            let t = cx.spawn(async move |this, cx| {
                let list = cx
                    .background_executor()
                    .spawn(async { acp_registry::refresh(false) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.ai.agents = this.agent_list(list).into();
                    cx.notify();
                });
            });
            self.ai._tasks.push(t);
        }
        if self.ai.input.is_none() {
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 10)
                    .submit_on_enter(true)
                    .placeholder("Ask about your data, or for a query…  (/ commands · @ tables)")
            });
            let sub =
                cx.subscribe_in(
                    &input,
                    window,
                    |this, _, ev: &InputEvent, window, cx| match ev {
                        InputEvent::PressEnter { shift: false, .. } => this.ai_submit(window, cx),
                        InputEvent::Change => {
                            this.ai.pick = 0;
                            cx.notify();
                        }
                        _ => {}
                    },
                );
            self.ai.input = Some(input);
            self.ai._subs.push(sub);
        }
        if self.ai.thread.is_none() {
            let id = crate::settings::get().agent.clone();
            let spec = self
                .ai
                .agents
                .iter()
                .find(|a| a.id == id)
                .or(self.ai.agents.first())
                .cloned();
            if let Some(spec) = spec {
                self.start_agent(spec, cx);
            }
        }
    }

    fn agent_list(&self, registry: Vec<AgentSpec>) -> Vec<AgentSpec> {
        let mut l = acp_registry::custom_specs(&crate::settings::get().agent_servers);
        l.extend(registry);
        l
    }

    fn start_agent(&mut self, spec: AgentSpec, cx: &mut Context<Self>) {
        let Some(mcp) = self.ai.host.as_ref().map(Host::server_spec) else {
            return;
        };
        let cwd = acp_registry::data_dir().join("workspace");
        let _ = std::fs::create_dir_all(&cwd);
        let thread = cx.new(|cx| AgentThread::new(spec, mcp, cwd, cx));
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let sub = cx.subscribe(&thread, |this, _, ev: &ThreadEvent, cx| match ev {
            ThreadEvent::Changed => this.sync_ai_scroller(cx),
        });
        // Changed events already notify (via the scroller sync); status /
        // option changes call `cx.notify()` on the thread, so observe those
        // only for the panel chrome.
        let obs = cx.observe(&thread, |this, _, cx| {
            if this.ai.open {
                cx.notify();
            }
        });
        self.ai._subs.push(sub);
        self.ai._subs.push(obs);
        self.ai.thread = Some(thread);
        self.ai.scroller = Some(scroller);
        self.ai.sessions.clear();
    }

    fn sync_ai_scroller(&mut self, cx: &mut Context<Self>) {
        let (Some(thread), Some(sc)) = (&self.ai.thread, &self.ai.scroller) else {
            return;
        };
        let n = thread.read(cx).entries.len();
        sc.update(cx, |s, cx| {
            let have = s.item_count();
            if n > have {
                s.append(n - have, cx);
            } else if n < have {
                s.reset(n, cx);
            }
            // Streaming grows the last rows only (a tool call can update a
            // few rows back): remeasure those, not the whole transcript.
            s.remeasure_items(n.saturating_sub(4)..n, cx);
        });
        cx.notify();
    }

    fn refresh_ai_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.ai.thread.clone() else {
            return;
        };
        let task = thread.update(cx, |t, cx| t.list_sessions(cx));
        let t = cx.spawn(async move |this, cx| {
            let list = task.await;
            let _ = this.update(cx, |this, cx| {
                this.ai.sessions = list;
                cx.notify();
            });
        });
        self.ai._tasks.push(t);
    }

    fn switch_agent(&mut self, spec: AgentSpec, cx: &mut Context<Self>) {
        let id = spec.id.clone();
        crate::settings::update(cx, |p| p.agent = id);
        self.ai.thread = None;
        self.start_agent(spec, cx);
        cx.notify();
    }

    fn new_ai_chat(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = &self.ai.thread {
            t.update(cx, |t, cx| t.reset_chat(cx));
        }
        cx.notify();
    }

    /// The database context sent with every prompt.
    fn ai_context(&self, cx: &App) -> Vec<Value> {
        let mut text = String::from(
            "You are the assistant inside Tusk, a PostgreSQL client. The user works with the \
             database below. Use the `tusk` MCP tools to look at it (get_context, list_tables, \
             describe_table, get_active_query). When the user wants SQL, write it with \
             `open_sql_tab` (or `replace_active_query` to change the query they have open) — \
             never run SQL yourself, the user reviews and runs it. Keep replies short.\n\n",
        );
        match &self.active_conn {
            Some((c, _)) => {
                text.push_str(&format!(
                    "Connection: {} — database `{}` as `{}` on {}:{}\nCurrent schema: `{}`\n",
                    c.name, c.database, c.user, c.host, c.port, self.current_schema
                ));
                if !self.objects.tables.is_empty() {
                    text.push_str(&format!(
                        "Tables in `{}`: {}\n",
                        self.current_schema,
                        self.objects.tables.join(", ")
                    ));
                }
            }
            None => text.push_str("No database is connected in Tusk right now.\n"),
        }
        match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Grid(g)) => {
                text.push_str(&format!(
                    "The user has table `{}.{}` open.\n",
                    g.table.schema, g.table.name
                ));
            }
            Some(WorkspaceTab::Sql(t)) => {
                let sql = t.editor.read(cx).text().to_string();
                let sql: String = sql.chars().take(4000).collect();
                if !sql.trim().is_empty() {
                    text.push_str(&format!(
                        "The user's open query tab \"{}\":\n```sql\n{sql}\n```\n",
                        t.title
                    ));
                }
            }
            None => {}
        }
        vec![json!({
            "type": "resource",
            "resource": { "uri": crate::agent::CONTEXT_URI, "mimeType": "text/markdown", "text": text }
        })]
    }

    fn ai_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(input), Some(thread)) = (self.ai.input.clone(), self.ai.thread.clone()) else {
            return;
        };
        // An open `/` or `@` list: Enter picks from it.
        if self.ai_suggest(cx).is_some() {
            self.ai_pick_confirm(window, cx);
            return;
        }
        let mut text = input.read(cx).value().trim().to_string();
        let attachments = std::mem::take(&mut self.ai.attachments);
        if (text.is_empty() && attachments.is_empty()) || thread.read(cx).status != Status::Ready {
            self.ai.attachments = attachments;
            return;
        }
        input.update(cx, |i, cx| i.set_value("", window, cx));
        let mut context = self.ai_context(cx);
        let labels: Vec<String> = attachments.iter().map(|a| a.label.clone()).collect();
        for (i, a) in attachments.iter().enumerate() {
            context.push(json!({
                "type": "resource",
                "resource": {
                    "uri": format!("tusk://attachment/{i}"),
                    "mimeType": if a.lang == "json" { "application/json" } else { "text/x-sql" },
                    "text": format!("{}:\n```{}\n{}\n```", a.label, a.lang, a.text)
                }
            }));
        }
        if text.is_empty() {
            text = "Take a look at the attached.".into();
        }
        let shown = if labels.is_empty() {
            text.clone()
        } else {
            format!(
                "{text}\n\n{}",
                labels
                    .iter()
                    .map(|l| format!("📎 {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        // `@table` mentions carry the table's DDL.
        let mentioned: Vec<String> = text
            .split_whitespace()
            .filter_map(|w| w.strip_prefix('@'))
            .map(|w| {
                w.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
                    .to_string()
            })
            .filter(|w| self.objects.tables.contains(w) || self.objects.views.contains(w))
            .collect();
        let pool = self.pool.clone();
        if mentioned.is_empty() || pool.is_none() {
            thread.update(cx, |t, cx| t.send_shown(text, shown, context, cx));
            return;
        }
        let pool = pool.unwrap_or_else(|| unreachable!());
        let schema = self.current_schema.clone();
        let views = self.objects.views.clone();
        let t = cx.spawn(async move |_, cx| {
            for name in mentioned {
                let kind = if views.contains(&name) {
                    crate::objects::ObjKind::View
                } else {
                    crate::objects::ObjKind::Table
                };
                if let Ok(ddl) = crate::objects::script(
                    &pool,
                    kind,
                    &schema,
                    &name,
                    crate::objects::Script::Create,
                )
                .await
                {
                    context.push(json!({
                        "type": "resource",
                        "resource": {
                            "uri": format!("tusk://table/{schema}/{name}"),
                            "mimeType": "text/x-sql",
                            "text": ddl
                        }
                    }));
                }
            }
            thread.update(cx, |t, cx| t.send_shown(text, shown, context, cx));
        });
        self.ai._tasks.push(t);
    }

    fn ai_suggest(&self, cx: &App) -> Option<Suggest> {
        let input = self.ai.input.as_ref()?;
        let text = input.read(cx).value().to_string();
        if let Some(q) = text.strip_prefix('/') {
            if q.contains(char::is_whitespace) {
                return None;
            }
            let thread = self.ai.thread.as_ref()?.read(cx);
            let q = q.to_lowercase();
            let l: Vec<(String, String)> = thread
                .commands
                .iter()
                .filter(|c| c.name.to_lowercase().contains(&q))
                .map(|c| (c.name.clone(), c.description.clone()))
                .collect();
            return (!l.is_empty()).then_some(Suggest::Commands(l));
        }
        // `@partial` at the end of the text.
        let word = text.rsplit(char::is_whitespace).next().unwrap_or_default();
        let q = word.strip_prefix('@')?.to_lowercase();
        let l: Vec<String> = self
            .objects
            .tables
            .iter()
            .chain(self.objects.views.iter())
            .filter(|t| t.to_lowercase().contains(&q) && t.to_lowercase() != q)
            .take(50)
            .cloned()
            .collect();
        (!l.is_empty()).then_some(Suggest::Tables(l))
    }

    pub(super) fn ai_pick_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = match self.ai_suggest(cx) {
            Some(Suggest::Commands(l)) => l.len(),
            Some(Suggest::Tables(l)) => l.len(),
            None => return,
        };
        self.ai.pick = (self.ai.pick as isize + delta).rem_euclid(n as isize) as usize;
        cx.notify();
    }

    pub(super) fn ai_pick_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.ai.input.clone() else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let new = match self.ai_suggest(cx) {
            Some(Suggest::Commands(l)) => l
                .get(self.ai.pick.min(l.len() - 1))
                .map(|(n, _)| format!("/{n} ")),
            Some(Suggest::Tables(l)) => l.get(self.ai.pick.min(l.len() - 1)).map(|name| {
                let cut = text.rfind('@').unwrap_or(text.len());
                format!("{}@{name} ", &text[..cut])
            }),
            None => None,
        };
        if let Some(new) = new {
            input.update(cx, |i, cx| {
                let end = new.len();
                i.set_value(new, window, cx);
                i.set_selected_range(end..end, cx);
            });
            self.ai.pick = 0;
            cx.notify();
        }
    }

    pub(super) fn ai_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = &self.ai.thread {
            t.update(cx, |t, cx| t.cancel(cx));
        }
    }

    /// A tool call from the agent's MCP bridge.
    fn answer_tool(&mut self, req: ToolRequest, window: &mut Window, cx: &mut Context<Self>) {
        let ToolRequest { name, args, reply } = req;
        let arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let connected = self.screen == AppScreen::Workspace && self.pool.is_some();
        match name.as_str() {
            "open_sql_tab" | "replace_active_query" => {
                let Some(sql) = arg("sql") else {
                    let _ = reply.send(Err("`sql` is required".into()));
                    return;
                };
                if !connected {
                    let _ = reply.send(Err("No database connection is open in Tusk.".into()));
                    return;
                }
                let replace = name == "replace_active_query"
                    && matches!(
                        self.active_tab.and_then(|ix| self.tabs.get(ix)),
                        Some(WorkspaceTab::Sql(_))
                    );
                if replace {
                    if let Some(WorkspaceTab::Sql(t)) =
                        self.active_tab.and_then(|ix| self.tabs.get(ix))
                    {
                        let editor = t.editor.clone();
                        editor.update(cx, |st, cx| {
                            let len = st.text().len();
                            st.set_selected_range(0..len, cx);
                            st.replace(sql.clone(), window, cx);
                        });
                    }
                } else {
                    self.open_sql_tab_with(Some(sql), window, cx);
                    if let (Some(title), Some(WorkspaceTab::Sql(t))) = (
                        arg("title"),
                        self.active_tab.and_then(|ix| self.tabs.get_mut(ix)),
                    ) {
                        let title: String = title.chars().take(40).collect();
                        if !title.trim().is_empty() {
                            t.title = title;
                        }
                    }
                }
                let title = match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
                    Some(WorkspaceTab::Sql(t)) => t.title.clone(),
                    _ => String::new(),
                };
                self.toast_info(format!(
                    "AI wrote a query to \"{title}\" — review and run it"
                ));
                cx.notify();
                let _ = reply.send(Ok(format!(
                    "The query is in the tab \"{title}\". It has NOT been run: the user reviews and runs it."
                )));
            }
            "get_active_query" => {
                let res = match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
                    Some(WorkspaceTab::Sql(t)) => {
                        let ed = t.editor.read(cx);
                        let sel = ed.selected_text().to_string();
                        let mut s = format!("Tab \"{}\":\n{}", t.title, ed.text());
                        if !sel.is_empty() {
                            s.push_str(&format!("\n\nSelected:\n{sel}"));
                        }
                        Ok(s)
                    }
                    _ => Err("No query tab is open.".into()),
                };
                let _ = reply.send(res);
            }
            "get_context" => {
                let mut s = String::new();
                match &self.active_conn {
                    Some((c, _)) => {
                        let who = if c.user.is_empty() {
                            String::new()
                        } else {
                            format!(" as `{}`", c.user)
                        };
                        s.push_str(&format!(
                            "Connection \"{}\": {} database `{}`{} on {}:{}\n",
                            c.name,
                            c.engine.label(),
                            c.database,
                            who,
                            c.host,
                            c.port
                        ));
                    }
                    None => s.push_str("Not connected to a database.\n"),
                }
                s.push_str(&format!(
                    "Current schema: {}\nSchemas: {}\n",
                    self.current_schema,
                    self.schemas.join(", ")
                ));
                match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
                    Some(WorkspaceTab::Grid(g)) => s.push_str(&format!(
                        "Open: table {}.{}\n",
                        g.table.schema, g.table.name
                    )),
                    Some(WorkspaceTab::Sql(t)) => {
                        s.push_str(&format!("Open: query tab \"{}\"\n", t.title))
                    }
                    None => {}
                }
                let _ = reply.send(Ok(s));
            }
            "list_tables" => {
                let Some(pool) = self.pool.clone() else {
                    let _ = reply.send(Err("Not connected to a database.".into()));
                    return;
                };
                let schema = arg("schema").unwrap_or_else(|| self.current_schema.clone());
                crate::db::runtime().spawn(async move {
                    let res = crate::db::fetch_objects(&pool, &schema).await.map(|o| {
                        format!(
                            "Schema {schema}\nTables: {}\nViews: {}\nMaterialized views: {}\nFunctions: {}",
                            o.tables.join(", "),
                            o.views.join(", "),
                            o.matviews.join(", "),
                            o.functions.join(", ")
                        )
                    });
                    let _ = reply.send(res);
                });
            }
            "describe_table" => {
                let (Some(pool), Some(table)) = (self.pool.clone(), arg("table")) else {
                    let _ = reply.send(Err("Not connected, or no `table` given.".into()));
                    return;
                };
                // `schema.table` works too.
                let (schema, table) = match (arg("schema"), table.split_once('.')) {
                    (Some(s), _) => (s, table),
                    (None, Some((s, t))) => (s.to_string(), t.to_string()),
                    (None, None) => (self.current_schema.clone(), table),
                };
                let kind = if self.objects.views.contains(&table) {
                    crate::objects::ObjKind::View
                } else if self.objects.matviews.contains(&table) {
                    crate::objects::ObjKind::MatView
                } else {
                    crate::objects::ObjKind::Table
                };
                crate::db::runtime().spawn(async move {
                    let res = crate::objects::script(
                        &pool,
                        kind,
                        &schema,
                        &table,
                        crate::objects::Script::Create,
                    )
                    .await;
                    let _ = reply.send(res);
                });
            }
            "run_sql" => {
                let (Some(sql), Some(pool)) = (arg("sql"), self.pool.clone()) else {
                    let _ = reply.send(Err("Not connected, or no `sql` given.".into()));
                    return;
                };
                let mode = crate::settings::get().agent_run();
                // What the settings let this run — Never refuses, Read-only
                // passes single SELECT-type statements, All runs anything.
                if let Err(e) = crate::mcp_bridge::agent_may_run(mode, pool.engine(), &sql) {
                    let _ = reply.send(Err(e));
                    return;
                }
                // Read-only runs on the server-enforced path: a READ ONLY
                // transaction where the protocol has one (pg / MySQL family).
                let server_readonly = mode == crate::settings::AgentRun::ReadOnly;
                let limit = arg("limit")
                    .and_then(|l| l.parse::<i64>().ok())
                    .unwrap_or(100)
                    .clamp(1, 500);
                let (hist_conn, hist_db) = self
                    .active_conn
                    .as_ref()
                    .map(|(c, _)| (c.name.clone(), c.database.clone()))
                    .unwrap_or_default();
                self.toast_info("AI ran a query — see the console");
                cx.notify();
                crate::db::runtime().spawn(async move {
                    let t0 = std::time::Instant::now();
                    let hist = |sql: &str, ok: bool| {
                        crate::console::add_history(crate::console::HistoryItem {
                            at: chrono::Local::now(),
                            sql: sql.trim().to_string(),
                            connection: hist_conn.clone(),
                            database: hist_db.clone(),
                            ms: t0.elapsed().as_millis(),
                            ok,
                        })
                    };
                    // All-mode mutations don't produce rows — they report
                    // rows affected, like the SQL editor's run path.
                    if crate::db::classify_statement(&sql) != crate::db::StmtKind::Query {
                        let out = crate::db::run_exec(&pool, &sql)
                            .await
                            .map(|n| format!("OK — {n} rows affected"));
                        hist(&sql, out.is_ok());
                        let _ = reply.send(out.map_err(|e| e.to_string()));
                        return;
                    }
                    let out = if server_readonly {
                        crate::db::run_query_rows_readonly(&pool, &sql, limit + 1).await
                    } else {
                        crate::db::run_query_rows(&pool, &sql, limit + 1).await
                    };
                    let out = match out {
                        Ok(rows) => {
                            hist(&sql, true);
                            let truncated = rows.len() as i64 > limit;
                            let mut rows = rows;
                            rows.truncate(limit as usize);
                            if rows.is_empty() {
                                Ok("0 rows".to_string())
                            } else {
                                let cols = crate::sql::infer_columns(&rows);
                                let mut table = vec![
                                    cols.iter()
                                        .map(|c| c.name.clone())
                                        .collect::<Vec<_>>()
                                        .join("\t"),
                                ];
                                for row in &rows {
                                    table.push(
                                        cols.iter()
                                            .map(|c| {
                                                let cell = crate::copy_as::plain(
                                                    row.get(c.name.as_str())
                                                        .unwrap_or(&serde_json::Value::Null),
                                                );
                                                // Keep the payload lean: cells capped,
                                                // tabs / newlines flattened.
                                                let mut cell: String =
                                                    cell.chars().take(300).collect();
                                                if cell.contains('\t') || cell.contains('\n') {
                                                    cell = cell.replace(['\t', '\n'], " ");
                                                }
                                                cell
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\t"),
                                    );
                                }
                                let mut s = format!(
                                    "{}{}\n\n{}",
                                    rows.len(),
                                    if truncated {
                                        " rows (truncated)"
                                    } else {
                                        " rows"
                                    },
                                    table.join("\n")
                                );
                                const MAX: usize = 64 * 1024;
                                if s.len() > MAX {
                                    let mut end = MAX;
                                    while end > 0 && !s.is_char_boundary(end) {
                                        end -= 1;
                                    }
                                    s.truncate(end);
                                    s.push_str("\n… (output truncated — narrow the query)");
                                }
                                Ok(s)
                            }
                        }
                        Err(e) => {
                            hist(&sql, false);
                            Err(e.to_string())
                        }
                    };
                    let _ = reply.send(out);
                });
            }
            other => {
                let _ = reply.send(Err(format!("Unknown tool {other}")));
            }
        }
    }

    /// Drag of the panel's left edge.
    pub(super) fn drag_ai_panel(
        &mut self,
        x: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((x0, w0)) = self.ai.drag else {
            return false;
        };
        let max = (f32::from(window.viewport_size().width) - 420.).max(MIN_W);
        self.ai.width = (w0 + (x0 - x)).clamp(MIN_W, max);
        cx.notify();
        true
    }

    /// Status bar, far right: the AI panel toggle.
    pub(super) fn render_ai_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, accent) = (t.muted_foreground, t.foreground, t.accent);
        let on = self.ai.open;
        div()
            .id("status-ai")
            .w(px(22.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(crate::theme::RADIUS_SM)
            .text_color(if on { accent } else { muted })
            .hover(|d| {
                d.bg(muted.opacity(0.12))
                    .text_color(if on { accent } else { fg })
            })
            // The sparkles fill their box: a size down to match the others.
            .child(Icon::new(IconName::Sparkles).size(px(11.)))
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new("AI Chat")
                    .key_binding(crate::kbd::tip("cmd-l"))
                    .build(window, cx)
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_ai_panel(window, cx)))
            .into_any_element()
    }

    pub(super) fn render_ai_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let (fg, muted, border, bg) = (t.foreground, t.muted_foreground, t.border, t.background);
        let Some(thread_e) = self.ai.thread.clone() else {
            return div()
                .w(px(self.ai_width()))
                .h_full()
                .border_l_1()
                .border_color(border)
                .bg(bg)
                .p_4()
                .text_size(px(crate::settings::ui_text()))
                .text_color(muted)
                .child("No agents available.")
                .into_any_element();
        };
        let thread = thread_e.read(cx);
        let status = thread.status.clone();
        let agent_name = thread.agent_name.clone();
        let title = thread.title.clone();
        let empty = thread.entries.is_empty();

        // ---- header: agent icon + chat title · new chat (agent menu) · history · close ----
        let agents = self.ai.agents.clone();
        let current_spec = thread.spec.clone();
        let new_menu = Button::new("ai-new")
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip("New Chat")
            .dropdown_menu(move |mut menu: PopupMenu, _, _| {
                menu = menu.max_h(px(460.)).scrollable(true).min_w(px(240.));
                let cur = current_spec.clone();
                menu = menu.item(
                    PopupMenuItem::new(format!("New Chat with {}", cur.name))
                        .icon(agent_icon(&cur))
                        .on_click(|_, _, cx| with_app(cx, |app, cx| app.new_ai_chat(cx))),
                );
                let (custom, reg): (Vec<_>, Vec<_>) = agents
                    .iter()
                    .cloned()
                    .partition(|a| a.id.starts_with("custom:"));
                let installed: Vec<_> = reg
                    .iter()
                    .filter(|a| acp_registry::installed(a))
                    .cloned()
                    .collect();
                let others: Vec<_> = reg
                    .iter()
                    .filter(|a| !acp_registry::installed(a))
                    .cloned()
                    .collect();
                for (label, list) in [
                    ("Custom Agents", custom),
                    ("Installed", installed),
                    ("ACP Registry", others),
                ] {
                    if list.is_empty() {
                        continue;
                    }
                    menu = menu.separator().label(label);
                    for a in list {
                        let checked = a.id == cur.id;
                        menu = menu.item(
                            PopupMenuItem::new(a.name.clone())
                                .icon(agent_icon(&a))
                                .checked(checked)
                                .on_click(move |_, _, cx| {
                                    let a = a.clone();
                                    with_app(cx, move |app, cx| app.switch_agent(a, cx));
                                }),
                        );
                    }
                }
                menu
            });
        let history_on = self.ai.history_open;
        let header_usage = thread.usage.clone().map(|(used, size, cost)| {
            let k = |n: u64| match n {
                1_000_000.. => format!("{:.1}M", n as f64 / 1_000_000.),
                1000.. => format!("{}k", n / 1000),
                _ => n.to_string(),
            };
            let mut s = if size > 0 {
                format!("{} / {}", k(used), k(size))
            } else {
                k(used)
            };
            if let Some(c) = cost.filter(|c| c != "$0.00") {
                s.push_str(&format!(" · {c}"));
            }
            div()
                .flex_none()
                .mr_1()
                .text_caption()
                .text_color(muted)
                .child(s)
        });
        let header = div()
            .h(px(crate::settings::bar_h() + 8.))
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .pl_3()
            .pr_2()
            .border_b_1()
            .border_color(border)
            .child(agent_icon(&thread.spec).size(px(14.)).text_color(muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .ml_1()
                    .truncate()
                    .text_size(px(crate::settings::ui_text()))
                    .text_color(fg)
                    .child(if history_on {
                        "History".to_string()
                    } else {
                        title
                            .clone()
                            .unwrap_or_else(|| format!("New Chat · {agent_name}"))
                    }),
            )
            .children(header_usage)
            .child(new_menu)
            .child(
                Button::new("ai-history")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().data(include_bytes!("../assets/icons/ui/history.svg")))
                    .selected(history_on)
                    .tooltip("Chat History")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ai.history_open = !this.ai.history_open;
                        if this.ai.history_open {
                            this.refresh_ai_sessions(cx);
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("ai-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close")
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_ai_panel(window, cx))),
            );

        // ---- body ----
        let history_view = history_on.then(|| {
            let sessions = self.ai.sessions.clone();
            let listable = thread.supports(&["sessionCapabilities", "list"]);
            if sessions.is_empty() {
                return centered(
                    if listable {
                        "No earlier chats yet."
                    } else {
                        "This agent doesn't keep a chat history."
                    },
                    muted,
                );
            }
            div()
                .id("ai-history-list")
                .size_full()
                .overflow_y_scroll()
                .py_1()
                .children(sessions.into_iter().enumerate().map(|(i, s)| {
                    let when = s.updated.as_deref().map(relative_time).unwrap_or_default();
                    div()
                        .id(("ai-session", i))
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded(crate::theme::RADIUS_SM)
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(crate::settings::ui_text()))
                        .hover(|d| d.bg(muted.opacity(0.12)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(fg)
                                .child(s.title.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_caption()
                                .text_color(muted)
                                .child(when),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ai.history_open = false;
                            if let Some(t) = &this.ai.thread {
                                let (id, title) = (s.id.clone(), s.title.clone());
                                t.update(cx, |t, cx| t.open_session(id, title, cx));
                            }
                            cx.notify();
                        }))
                }))
                .into_any_element()
        });
        let body: AnyElement = if let Some(h) = history_view {
            h
        } else {
            match &status {
            Status::Starting(msg) if empty => centered(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(IconName::LoaderCircle).size(px(14.)).text_color(muted))
                    .child(msg.clone()),
                muted,
            ),
            Status::Failed(msg) => {
                let msg = msg.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .child(
                        div()
                            .text_size(px(crate::settings::ui_text()))
                            .text_color(t.red)
                            .child(format!("{agent_name} couldn't start")),
                    )
                    .child(
                        div()
                            .id("ai-fail-detail")
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .text_caption()
                            .font_family(crate::settings::table_font())
                            .text_color(muted)
                            .child(msg),
                    )
                    .child(
                        Button::new("ai-retry")
                            .outline()
                            .small()
                            .label("Retry")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(t) = &this.ai.thread {
                                    t.update(cx, |t, cx| t.retry(cx));
                                }
                            })),
                    )
                    .into_any_element()
            }
            Status::AuthRequired => {
                let methods = thread.auth_methods.clone();
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .child(
                        div()
                            .text_size(px(crate::settings::ui_text()))
                            .text_color(fg)
                            .child(format!("Sign in to {agent_name}")),
                    )
                    .children(methods.into_iter().enumerate().map(|(i, m)| {
                        let desc = m.description.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                Button::new(("ai-auth", i))
                                    .outline()
                                    .small()
                                    .icon(IconName::LogIn)
                                    .label(m.name.clone())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let m = m.clone();
                                        if let Some(t) = &this.ai.thread {
                                            t.update(cx, |t, cx| t.authenticate(&m, cx));
                                        }
                                    })),
                            )
                            .when(!desc.is_empty(), |d| d.child(div().text_caption().text_color(muted).child(desc)))
                    }))
                    .child(
                        Button::new("ai-auth-retry")
                            .ghost()
                            .small()
                            .label("Retry")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(t) = &this.ai.thread {
                                    t.update(cx, |t, cx| t.retry(cx));
                                }
                            })),
                    )
                    .into_any_element()
            }
            _ if empty => centered(
                div()
                    .max_w(px(300.))
                    .text_center()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(agent_icon(&thread.spec).size(px(22.)).text_color(muted))
                    .child(div().text_color(fg).child(format!("Chat with {agent_name}")))
                    .child(
                        "It can read your database's structure and write queries into a tab. Running them stays with you.",
                    ),
                muted,
            ),
            _ => {
                let Some(sc) = self.ai.scroller.clone() else { return div().into_any_element() };
                let th = thread_e.clone();
                MessageScroller::new("ai-messages", sc, move |ix, window, cx| render_entry(&th, ix, window, cx))
                    .jump_button(true)
                    .with_content_style(StyleRefinement::default().py(px(10.)))
                    // Rows space themselves (tight between tool calls).
                    .with_row_style(StyleRefinement::default().pb(px(0.)))
                    .size_full()
                    .into_any_element()
            }
        }
        };

        // ---- plan ----
        let plan = thread.plan.clone();
        let plan_el = (!plan.is_empty()).then(|| {
            let done = plan.iter().filter(|p| p.status == "completed").count();
            let open = self.ai.plan_open;
            let current = plan
                .iter()
                .find(|p| p.status == "in_progress")
                .or(plan.iter().find(|p| p.status != "completed"))
                .map(|p| p.content.clone())
                .unwrap_or_default();
            div()
                .flex_none()
                .border_t_1()
                .border_color(border)
                .px_3()
                .py_1()
                .child(
                    div()
                        .id("ai-plan")
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_caption()
                        .text_color(muted)
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(px(12.)),
                        )
                        .child(Icon::new(IconName::ListTodo).size(px(12.)))
                        .child(format!("Plan {done}/{}", plan.len()))
                        .when(!open, |d| {
                            d.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(fg)
                                    .child(current),
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ai.plan_open = !this.ai.plan_open;
                            cx.notify();
                        })),
                )
                .when(open, |d| {
                    d.children(plan.into_iter().map(|p| {
                        let (icon, color) = match p.status.as_str() {
                            "completed" => (IconName::CircleCheck, t.green),
                            "in_progress" => (IconName::LoaderCircle, t.accent),
                            _ => (IconName::CircleDashed, muted),
                        };
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .py(px(2.))
                            .text_caption()
                            .child(Icon::new(icon).size(px(12.)).text_color(color))
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(if p.status == "completed" { muted } else { fg })
                                    .when(p.status == "completed", |d| d.line_through())
                                    .child(p.content),
                            )
                    }))
                })
        });

        // ---- composer ----
        let busy = status == Status::Busy;
        let ready = status == Status::Ready;
        let suggest = self.ai_suggest(cx);
        let pick = self.ai.pick;
        let popup = suggest.map(|s| {
            let rows: Vec<(String, String)> = match s {
                Suggest::Commands(l) => l.into_iter().map(|(n, d)| (format!("/{n}"), d)).collect(),
                Suggest::Tables(l) => l
                    .into_iter()
                    .map(|n| (format!("@{n}"), String::new()))
                    .collect(),
            };
            let pick = pick.min(rows.len().saturating_sub(1));
            div()
                .id("ai-suggest")
                .absolute()
                .bottom_full()
                .left(px(8.))
                .right(px(8.))
                .mb_1()
                .max_h(px(240.))
                .overflow_y_scroll()
                .py_1()
                .rounded(crate::theme::RADIUS_MD)
                .border_1()
                .border_color(border)
                .bg(t.popover)
                .shadow_lg()
                .occlude()
                .children(rows.into_iter().enumerate().map(|(i, (name, desc))| {
                    div()
                        .id(("ai-suggest-row", i))
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded(crate::theme::RADIUS_SM)
                        .flex()
                        .gap_2()
                        .text_size(px(crate::settings::ui_text()))
                        .when(i == pick, |d| d.bg(t.tokens.table_active))
                        .hover(|d| d.bg(muted.opacity(0.12)))
                        .child(
                            div()
                                .flex_none()
                                .text_color(fg)
                                .font_family(crate::settings::table_font())
                                .child(name),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(muted)
                                .child(desc),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.ai.pick = i;
                                this.ai_pick_confirm(window, cx);
                            }),
                        )
                }))
        });
        let mut option_btns: Vec<AnyElement> = Vec::new();
        // The model picker sits on the right, next to send.
        let mut model_btn: Option<AnyElement> = None;
        let has_mode_option = thread
            .config
            .iter()
            .any(|o| o.category.as_deref() == Some("mode") || o.id == "mode");
        if let (Some((cur, modes)), false) = (thread.modes.clone(), has_mode_option) {
            let name = modes
                .iter()
                .find(|m| m.value == cur)
                .map(|m| m.name.clone())
                .unwrap_or(cur.clone());
            option_btns.push(
                Button::new("ai-mode")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ShieldCheck)
                    .dropdown_caret(true)
                    .label(name)
                    .dropdown_menu(move |mut menu: PopupMenu, _, _| {
                        for m in modes.clone() {
                            let id = m.value.clone();
                            menu = menu.item(
                                PopupMenuItem::new(m.name.clone())
                                    .checked(m.value == cur)
                                    .on_click(move |_, _, cx| {
                                        let id = id.clone();
                                        with_app(cx, move |app, cx| {
                                            if let Some(t) = &app.ai.thread {
                                                t.update(cx, |t, cx| t.set_mode(id, true, cx));
                                            }
                                        });
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element(),
            );
        }
        for (i, o) in thread.config.clone().into_iter().enumerate() {
            let id = o.id.clone();
            match o.kind {
                ConfigKind::Select { current, options } => {
                    let label = options
                        .iter()
                        .find(|x| x.value == current)
                        .map(|x| x.name.clone())
                        .unwrap_or(current.clone());
                    let tip: SharedString = o.name.clone().into();
                    let is_model = o.category.as_deref() == Some("model") || o.id == "model";
                    let icon = match o.category.as_deref() {
                        Some("mode") => IconName::ShieldCheck,
                        Some("model") => IconName::Cpu,
                        Some("thought_level") => IconName::Brain,
                        _ if o.id == "mode" => IconName::ShieldCheck,
                        _ if o.id == "model" => IconName::Cpu,
                        _ if o.id.contains("effort")
                            || o.id.contains("thought")
                            || o.id.contains("reason") =>
                        {
                            IconName::Brain
                        }
                        _ => IconName::SlidersHorizontal,
                    };
                    let trigger = Button::new(("ai-opt", i))
                        .ghost()
                        .xsmall()
                        .icon(icon)
                        .dropdown_caret(true)
                        .label(label)
                        .tooltip(tip);
                    let open = self
                        .ai
                        .open_picker
                        .as_ref()
                        .filter(|(pid, _)| *pid == id)
                        .map(|(_, p)| p.clone());
                    let is_open = open.is_some();
                    let (oid, opts, cur) = (id.clone(), options.clone(), current.clone());
                    let btn = Popover::new(("ai-opt-pop", i))
                        .anchor(Anchor::BottomLeft)
                        .open(is_open)
                        .on_open_change(move |open, window, cx| {
                            let (oid, opts, cur) = (oid.clone(), opts.clone(), cur.clone());
                            let view = cx.global::<TuskHandle>().0.clone();
                            let open = *open;
                            view.update(cx, |app, cx| {
                                if open {
                                    app.open_option_picker(oid, opts, cur, window, cx);
                                } else {
                                    app.ai.open_picker = None;
                                    cx.notify();
                                }
                            });
                        })
                        .trigger(trigger)
                        .content(move |_, _, _| match &open {
                            Some(p) => p.clone().into_any_element(),
                            None => div().into_any_element(),
                        })
                        .into_any_element();
                    if is_model {
                        model_btn = Some(btn);
                    } else {
                        option_btns.push(btn);
                    }
                }
                ConfigKind::Boolean(on) => {
                    option_btns.push(
                        Button::new(("ai-opt", i))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Zap)
                            .label(o.name.clone())
                            .selected(on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(t) = &this.ai.thread {
                                    let id = id.clone();
                                    t.update(cx, |t, cx| t.set_config(&id, json!(!on), true, cx));
                                }
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        let send_btn = if busy {
            Button::new("ai-stop")
                .ghost()
                .xsmall()
                .icon(IconName::CircleStop)
                .tooltip("Stop")
                .on_click(cx.listener(|this, _, _, cx| this.ai_cancel(cx)))
        } else {
            Button::new("ai-send")
                .ghost()
                .xsmall()
                .icon(IconName::ArrowUp)
                .tooltip("Send")
                .disabled(!ready)
                .on_click(cx.listener(|this, _, window, cx| this.ai_submit(window, cx)))
        };
        let working = busy.then(|| {
            div()
                .flex_none()
                .px_3()
                .pb_1()
                .flex()
                .items_center()
                .gap_2()
                .text_caption()
                .text_color(muted)
                .child(
                    gpui_kit::component::shimmer::ShimmerText::new("Working…")
                        .id("ai-working")
                        .highlight_color(fg),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("ai-stop-link")
                        .cursor_pointer()
                        .hover(|d| d.text_color(fg))
                        .child("Stop")
                        .on_click(cx.listener(|this, _, _, cx| this.ai_cancel(cx))),
                )
        });
        let composer = self.ai.input.clone().map(|input| {
            div()
                .relative()
                .flex_none()
                .m_2()
                .rounded(crate::theme::RADIUS_LG)
                .border_1()
                .border_color(border)
                .bg(t.colors.input.opacity(0.3))
                .key_context("AiComposer")
                .on_action(cx.listener(|this, _: &AiPickUp, _, cx| this.ai_pick_move(-1, cx)))
                .on_action(cx.listener(|this, _: &AiPickDown, _, cx| this.ai_pick_move(1, cx)))
                .on_action(cx.listener(|this, _: &AiPickConfirm, window, cx| {
                    this.ai_pick_confirm(window, cx)
                }))
                .on_action(cx.listener(|this, _: &AiEscape, _, cx| {
                    let busy = this
                        .ai
                        .thread
                        .as_ref()
                        .is_some_and(|t| t.read(cx).status == Status::Busy);
                    if busy {
                        this.ai_cancel(cx);
                    }
                }))
                .children(popup)
                .when(!self.ai.attachments.is_empty(), |d| {
                    d.child(div().flex().flex_wrap().gap_1().px_2().pt_2().children(
                        self.ai.attachments.iter().enumerate().map(|(i, a)| {
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .max_w(px(260.))
                                .pl_2()
                                .pr_1()
                                .py(px(1.))
                                .rounded(crate::theme::RADIUS_SM)
                                .border_1()
                                .border_color(border)
                                .text_caption()
                                .text_color(fg)
                                .child(
                                    Icon::new(if a.lang == "json" {
                                        IconName::Table2
                                    } else {
                                        IconName::FileText
                                    })
                                    .size(px(11.))
                                    .text_color(muted),
                                )
                                .child(div().min_w_0().truncate().child(a.label.clone()))
                                .child(
                                    div()
                                        .id(("ai-att-x", i))
                                        .text_color(muted)
                                        .hover(|d| d.text_color(fg))
                                        .child(Icon::new(IconName::Close).size(px(10.)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if i < this.ai.attachments.len() {
                                                this.ai.attachments.remove(i);
                                            }
                                            cx.notify();
                                        })),
                                )
                        }),
                    ))
                })
                .child(
                    div()
                        .px_1()
                        .pt_1()
                        .child(Textarea::new(&input).appearance(false).bordered(false)),
                )
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap_1()
                        .px_1()
                        .pb_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_wrap()
                                .gap_1()
                                .children(option_btns),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap_1()
                                .children(model_btn)
                                .child(send_btn),
                        ),
                )
        });
        let _ = window;
        div()
            .relative()
            .flex_none()
            .w(px(self.ai_width()))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(border)
            .bg(bg)
            .occlude()
            .child(
                div()
                    .id("ai-edge")
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(5.))
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, _| {
                            this.ai.drag = Some((f32::from(e.position.x), this.ai_width()));
                        }),
                    ),
            )
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .children(plan_el)
            .children(working)
            .children(composer)
            .into_any_element()
    }
}

/// The agent's registry icon (monochrome SVG), else a generic one.
fn agent_icon(spec: &AgentSpec) -> Icon {
    type Cache = std::collections::HashMap<String, Option<std::sync::Arc<[u8]>>>;
    static CACHE: std::sync::LazyLock<std::sync::Mutex<Cache>> =
        std::sync::LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap();
    let bytes = match cache.get(&spec.id) {
        Some(b) => b.clone(),
        None => {
            let path = spec.icon.clone().unwrap_or_else(|| {
                acp_registry::data_dir()
                    .join("icons")
                    .join(format!("{}.svg", spec.id))
            });
            let b: Option<std::sync::Arc<[u8]>> = std::fs::read(path)
                .ok()
                .filter(|b| b.windows(4).any(|w| w == b"<svg"))
                .map(Into::into);
            cache.insert(spec.id.clone(), b.clone());
            b
        }
    };
    match bytes {
        Some(b) => Icon::default().data(&b),
        None => Icon::new(IconName::Bot),
    }
}

/// `2026-09-24T10:00:00Z` → "3h ago" / "Sep 21".
fn relative_time(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return ts.get(..10).unwrap_or(ts).to_string();
    };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        86400..604800 => format!("{}d ago", secs / 86400),
        _ => t.format("%b %-d").to_string(),
    }
}

fn centered(child: impl IntoElement, muted: Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(crate::settings::ui_text()))
        .text_color(muted)
        .child(child)
        .into_any_element()
}

/// One chat entry (the message list renders rows lazily).
fn render_entry(
    thread: &Entity<AgentThread>,
    ix: usize,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    // Colors only: a whole-theme clone per visible row per frame is costly.
    let t = Palette::of(cx);
    let (fg, muted, border) = (t.foreground, t.muted_foreground, t.border);
    let th = thread.read(cx);
    let busy = th.status == Status::Busy;
    let last = ix + 1 == th.entries.len();
    // Tight between consecutive tool calls / thoughts, roomier around text.
    let next_is_step = matches!(
        th.entries.get(ix + 1),
        Some(Entry::Tool(_) | Entry::Thought { .. })
    );
    let this_is_step = matches!(
        th.entries.get(ix),
        Some(Entry::Tool(_) | Entry::Thought { .. })
    );
    let gap = if last {
        0.
    } else if this_is_step && next_is_step {
        4.
    } else {
        12.
    };
    let row = div().w_full().pb(px(gap));
    let Some(entry) = th.entries.get(ix) else {
        return row.into_any_element();
    };
    let ui = px(crate::settings::ui_text());
    match entry {
        Entry::User { text } => row
            .child(
                div()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(muted.opacity(0.06))
                    .px_3()
                    .py_2()
                    .text_size(ui)
                    .text_color(fg)
                    // A resumed chat replays the prompt with Tusk's context.
                    .child(crate::agent::strip_injected_context(text)),
            )
            .into_any_element(),
        Entry::Agent { md, .. } => row
            .text_size(ui)
            .child(
                TextView::new(md)
                    .selectable(true)
                    .stream_fade(true)
                    .style(text_style(&t))
                    .code_block_actions(|code, _, _| {
                        let sql = code.code().to_string();
                        let is_sql = code.lang().is_none_or(|l| {
                            let l = l.to_lowercase();
                            l == "sql" || l == "postgresql" || l == "pgsql" || l == "psql"
                        });
                        div().when(is_sql, move |d| {
                            d.child(
                                Button::new("code-open-tab")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::ArrowUpRight)
                                    .tooltip("Open in a query tab")
                                    .on_click(move |_, window, cx| {
                                        let sql = sql.clone();
                                        let view = cx.global::<TuskHandle>().0.clone();
                                        view.update(cx, |app, cx| {
                                            if app.pool.is_some() {
                                                app.open_sql_tab_with(Some(sql), window, cx);
                                            }
                                        });
                                    }),
                            )
                        })
                    }),
            )
            .into_any_element(),
        Entry::Thought { md, expanded } => {
            let expanded = *expanded;
            let thinking = busy && last;
            let th_e = thread.clone();
            row.child(
                div()
                    .id(("ai-thought", ix))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_caption()
                    .text_color(muted)
                    .hover(|d| d.text_color(fg))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.)),
                    )
                    .child(Icon::new(IconName::Brain).size(px(12.)))
                    .child(if thinking {
                        gpui_kit::component::shimmer::ShimmerText::new("Thinking")
                            .id(("ai-thinking", ix))
                            .highlight_color(fg)
                            .into_any_element()
                    } else {
                        "Thought".into_any_element()
                    })
                    .on_click(move |_, _, cx| th_e.update(cx, |t, cx| t.toggle_entry(ix, cx))),
            )
            .when(expanded, |d| {
                d.child(
                    div()
                        .mt_1()
                        .pl_3()
                        .border_l_1()
                        .border_color(border)
                        .text_caption()
                        .text_color(muted)
                        .child(TextView::new(md).selectable(true).style(text_style(&t))),
                )
            })
            .into_any_element()
        }
        Entry::Notice { text, error } => row
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_caption()
                    .text_color(if *error { t.red } else { muted })
                    .child(
                        Icon::new(if *error {
                            IconName::CircleX
                        } else {
                            IconName::Info
                        })
                        .size(px(12.)),
                    )
                    .child(div().flex_1().child(text.clone())),
            )
            .into_any_element(),
        Entry::Tool(tc) => {
            let (status_icon, status_color) = match tc.status.as_str() {
                "completed" => (Some(IconName::Check), t.green),
                "failed" => (Some(IconName::CircleX), t.red),
                _ => (Some(IconName::LoaderCircle), muted),
            };
            let tusk = tusk_tool(&tc.title);
            let title = match tusk {
                Some("open_sql_tab") => "Wrote a query to a new tab".to_string(),
                Some("replace_active_query") => "Updated the open query".to_string(),
                Some("get_context") => "Looked at the connection".to_string(),
                Some("list_tables") => "Listed the tables".to_string(),
                Some("describe_table") => format!(
                    "Read the structure of {}",
                    tc.raw_input["table"].as_str().unwrap_or("a table")
                ),
                Some("get_active_query") => "Read the open query".to_string(),
                _ => tc.title.clone(),
            };
            let expanded = tc.expanded || tc.permission.is_some();
            let th_e = thread.clone();
            let tool_id = tc.id.clone();
            let mut body: Vec<AnyElement> = Vec::new();
            if expanded {
                if let Some(sql) = tc.raw_input["sql"].as_str().filter(|_| tusk.is_some()) {
                    body.push(code_box(sql, &t));
                }
                for c in &tc.content {
                    match c {
                        ToolContent::Text(md) => body.push(
                            div()
                                .text_caption()
                                .text_color(muted)
                                .child(TextView::new(md).selectable(true))
                                .into_any_element(),
                        ),
                        ToolContent::Diff { path, old, new } => {
                            let lines = diff_lines(old.as_deref().unwrap_or(""), new);
                            body.push(
                                div()
                                    .rounded(crate::theme::RADIUS_MD)
                                    .border_1()
                                    .border_color(border)
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .text_caption()
                                            .text_color(muted)
                                            .border_b_1()
                                            .border_color(border)
                                            .child(path.clone()),
                                    )
                                    .children(lines.into_iter().take(200).map(|(k, l)| {
                                        let (bgc, fgc) = match k {
                                            '+' => (t.green.opacity(0.12), fg),
                                            '-' => (t.red.opacity(0.12), fg),
                                            _ => (gpui::transparent_black(), muted),
                                        };
                                        div()
                                            .px_2()
                                            .bg(bgc)
                                            .text_color(fgc)
                                            .text_caption()
                                            .font_family(crate::settings::table_font())
                                            .child(format!("{k} {l}"))
                                    }))
                                    .into_any_element(),
                            );
                        }
                        ToolContent::Terminal(id) => {
                            if let Some(term) = th.terminals.get(id) {
                                let out = term.output.lock().unwrap().clone();
                                let tail: String = {
                                    let lines: Vec<&str> = out.lines().collect();
                                    lines[lines.len().saturating_sub(40)..].join("\n")
                                };
                                let exit = term.exit.lock().unwrap().clone();
                                body.push(
                                    div()
                                        .rounded(crate::theme::RADIUS_MD)
                                        .border_1()
                                        .border_color(border)
                                        .p_2()
                                        .text_caption()
                                        .font_family(crate::settings::table_font())
                                        .child(
                                            div()
                                                .text_color(fg)
                                                .child(format!("$ {}", term.command)),
                                        )
                                        .child(div().text_color(muted).child(tail))
                                        .children(exit.map(|(code, sig)| {
                                            div().text_color(muted).child(match (code, sig) {
                                                (Some(c), _) => format!("exit {c}"),
                                                (None, Some(s)) => s,
                                                _ => "exited".into(),
                                            })
                                        }))
                                        .into_any_element(),
                                );
                            }
                        }
                    }
                }
                if tc.content.is_empty() && tusk.is_none() && !tc.raw_input.is_null() {
                    let raw = serde_json::to_string_pretty(&tc.raw_input).unwrap_or_default();
                    body.push(code_box(&raw.chars().take(3000).collect::<String>(), &t));
                }
                if let Some(p) = &tc.permission {
                    let buttons = p.options.iter().enumerate().map(|(i, o)| {
                        let (th_e, tool_id, opt) = (thread.clone(), tool_id.clone(), o.id.clone());
                        let b = Button::new(("ai-perm", i)).xsmall().label(o.name.clone());
                        let b = if o.kind.starts_with("allow") {
                            b.outline()
                        } else {
                            b.ghost()
                        };
                        b.on_click(move |_, _, cx| {
                            th_e.update(cx, |t, cx| t.answer_permission(&tool_id, &opt, cx));
                        })
                    });
                    body.push(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .pt_1()
                            .children(buttons)
                            .into_any_element(),
                    );
                }
            }
            row.child(
                div()
                    .rounded(crate::theme::RADIUS_MD)
                    .border_1()
                    .border_color(if tc.permission.is_some() {
                        t.yellow.opacity(0.6)
                    } else {
                        border
                    })
                    .child(
                        div()
                            .id(("ai-tool", ix))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .text_caption()
                            .text_color(muted)
                            .hover(|d| d.text_color(fg))
                            .child(
                                Icon::new(if tusk.is_some() {
                                    IconName::Database
                                } else {
                                    kind_icon(&tc.kind)
                                })
                                .size(px(12.)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .children(
                                status_icon
                                    .map(|i| Icon::new(i).size(px(12.)).text_color(status_color)),
                            )
                            .on_click(move |_, _, cx| {
                                th_e.update(cx, |t, cx| t.toggle_entry(ix, cx))
                            }),
                    )
                    .when(!body.is_empty(), |d| {
                        d.child(div().flex().flex_col().gap_1().px_2().pb_2().children(body))
                    }),
            )
            .into_any_element()
        }
    }
}

/// The few theme colors chat rows use.
#[derive(Clone, Copy)]
struct Palette {
    foreground: Hsla,
    muted_foreground: Hsla,
    border: Hsla,
    red: Hsla,
    green: Hsla,
    yellow: Hsla,
}

impl Palette {
    fn of(cx: &App) -> Self {
        let t = cx.theme();
        Self {
            foreground: t.foreground,
            muted_foreground: t.muted_foreground,
            border: t.border,
            red: t.red,
            green: t.green,
            yellow: t.yellow,
        }
    }
}

/// Chat Markdown: inline code on a quiet chip instead of the accent.
fn text_style(t: &Palette) -> gpui_kit::component::text::TextViewStyle {
    gpui_kit::component::text::TextViewStyle::default()
        .paragraph_gap(rems(0.6))
        .inline_code(HighlightStyle {
            background_color: Some(t.muted_foreground.opacity(0.14)),
            ..Default::default()
        })
}

fn code_box(text: &str, t: &Palette) -> AnyElement {
    div()
        .rounded(crate::theme::RADIUS_MD)
        .bg(t.muted_foreground.opacity(0.08))
        .px_2()
        .py_1()
        .text_caption()
        .font_family(crate::settings::table_font())
        .text_color(t.foreground)
        .child(text.to_string())
        .into_any_element()
}

/// A session option's values (models can run into the hundreds): a search
/// box over a virtual list — only the visible rows are built.
pub struct OptionPicker {
    thread: WeakEntity<AgentThread>,
    id: String,
    options: Vec<crate::agent::SelectOption>,
    current: String,
    query: Entity<InputState>,
    matches: Vec<usize>,
    pick: usize,
    scroll: UniformListScrollHandle,
    _sub: Subscription,
}

impl OptionPicker {
    fn new(
        thread: WeakEntity<AgentThread>,
        id: String,
        options: Vec<crate::agent::SelectOption>,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));
        let _sub = cx.subscribe_in(
            &query,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => this.refilter(cx),
                InputEvent::PressEnter { .. } => this.confirm(None, window, cx),
                _ => {}
            },
        );
        let pick = options.iter().position(|o| o.value == current).unwrap_or(0);
        let scroll = UniformListScrollHandle::new();
        scroll.scroll_to_item(pick, ScrollStrategy::Center);
        Self {
            matches: (0..options.len()).collect(),
            thread,
            id,
            options,
            current,
            query,
            pick,
            scroll,
            _sub,
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.query.read(cx).value().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        self.matches = (0..self.options.len())
            .filter(|&i| {
                let o = &self.options[i];
                let hay = format!(
                    "{} {} {}",
                    o.name,
                    o.value,
                    o.group.as_deref().unwrap_or("")
                )
                .to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect();
        self.pick = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.pick = (self.pick as isize + delta).rem_euclid(n as isize) as usize;
        self.scroll
            .scroll_to_item(self.pick, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn confirm(&mut self, row: Option<usize>, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(&ix) = self.matches.get(row.unwrap_or(self.pick)) else {
            return;
        };
        let value = self.options[ix].value.clone();
        let id = self.id.clone();
        if let Some(t) = self.thread.upgrade() {
            t.update(cx, |t, cx| t.set_config(&id, json!(value), true, cx));
        }
        let view = cx.global::<TuskHandle>().0.clone();
        view.update(cx, |app, cx| {
            app.ai.open_picker = None;
            cx.notify();
        });
    }
}

impl Render for OptionPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let (fg, muted) = (t.foreground, t.muted_foreground);
        let row_h = crate::settings::row_h();
        let n = self.matches.len();
        let this = cx.entity();
        let list =
            uniform_list("ai-option-list", n, move |range, _, cx| {
                let p = this.read(cx);
                range
                    .map(|r| {
                        let o = &p.options[p.matches[r]];
                        let selected = o.value == p.current;
                        let this = this.clone();
                        div()
                            .id(("ai-option", r))
                            .w_full()
                            .h(px(row_h))
                            .px_2()
                            .mx_1()
                            .rounded(crate::theme::RADIUS_SM)
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(crate::settings::ui_text()))
                            .when(r == p.pick, |d| d.bg(t.tokens.table_active))
                            .hover(|d| d.bg(muted.opacity(0.12)))
                            .child(div().w(px(12.)).flex_none().when(selected, |d| {
                                d.child(Icon::new(IconName::Check).size(px(12.)).text_color(fg))
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(fg)
                                    .child(o.name.clone()),
                            )
                            .children(o.group.clone().map(|g| {
                                div().flex_none().text_caption().text_color(muted).child(g)
                            }))
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                this.update(cx, |p, cx| p.confirm(Some(r), window, cx));
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .h(px((n.clamp(1, 12)) as f32 * row_h));
        div()
            .w(px(320.))
            .flex()
            .flex_col()
            .gap_1()
            .key_context("AiOptionPicker")
            .on_action(cx.listener(|this, _: &AiPickUp, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &AiPickDown, _, cx| this.step(1, cx)))
            .child(Input::new(&self.query).small())
            .child(if n == 0 {
                div()
                    .p_2()
                    .text_caption()
                    .text_color(muted)
                    .child("No match")
                    .into_any_element()
            } else {
                list.into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::diff_lines;

    #[test]
    fn diff_trims_common_lines() {
        let d = diff_lines("a\nb\nc\nd", "a\nb\nX\nd");
        assert_eq!(
            d,
            vec![
                (' ', "a".into()),
                (' ', "b".into()),
                ('-', "c".into()),
                ('+', "X".into()),
                (' ', "d".into())
            ]
        );
        assert_eq!(diff_lines("", "new"), vec![('+', "new".to_string())]);
    }
}
