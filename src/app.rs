//! App shell: Connection vs Workspace state machine + layout skeleton.
//!
//! - `AppState::Connection`: baglanti yok — ortada baglanti paneli (form Phase 4'te).
//! - `AppState::Workspace`: sol sidebar (240px, 180..400 arasi suruklenebilir) +
//!   ana alan (tab bar + icerik) + alt durum cubugu.

use crate::theme::TextCaption as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::TitleBar;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu, PopupMenuItem};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_resizable, resizable_panel};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::actions::*;
use crate::conn::ConnectionForm;
use crate::db::{self, ObjectTree, SavedConnection, Stmt};
use crate::filter::{FilterBar, FilterRow};
use crate::grid::{self, GridDelegate};
use crate::icons::DbIcon;
use crate::palette::{PaletteMode, PaletteOverlay};
use crate::structure::{self, StructureDelegate};

#[path = "app_agent.rs"]
mod agent_panel;
#[path = "app_conns.rs"]
mod conns;
#[path = "app_ddl.rs"]
mod ddl_view;
#[path = "app_edit.rs"]
mod edit;
#[path = "app_editor.rs"]
mod editor_cmds;
#[path = "app_files.rs"]
mod files;
#[path = "app_migrate.rs"]
pub mod migrate_sheet;
#[path = "app_nav.rs"]
mod nav;
#[path = "app_objects.rs"]
mod objects_menu;
#[path = "app_panels.rs"]
mod panels;
#[path = "app_rowdetail.rs"]
mod rowdetail;
#[path = "app_tools.rs"]
mod tools;
#[path = "app_tree.rs"]
mod tree;
#[path = "app_users.rs"]
mod users_view;

/// Global handle so `&mut App`-only callbacks (palette confirm/cancel,
/// menu items) can reach the app view. Set once in `main`.
#[derive(Clone)]
pub struct TuskHandle(pub Entity<TuskApp>);
impl Global for TuskHandle {}

#[allow(dead_code)] // palette confirm/cancel callbacks use this via cx.global
pub fn tusk_handle(cx: &App) -> Entity<TuskApp> {
    cx.global::<TuskHandle>().0.clone()
}

#[derive(Clone, Copy, PartialEq)]
pub enum AppScreen {
    Connection,
    Workspace,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    Function,
}

impl TableKind {
    pub fn label(&self) -> &'static str {
        match self {
            TableKind::Table => "table",
            TableKind::View => "view",
            TableKind::MaterializedView => "matview",
            TableKind::Function => "function",
        }
    }

    /// DDL keyword for DROP / ALTER … RENAME.
    fn ddl_keyword(&self) -> &'static str {
        match self {
            TableKind::Table => "TABLE",
            TableKind::View => "VIEW",
            TableKind::MaterializedView => "MATERIALIZED VIEW",
            TableKind::Function => "FUNCTION",
        }
    }

    fn icon(&self) -> gpui_kit::Img {
        match self {
            TableKind::Table => DbIcon::Table.icon(),
            TableKind::View | TableKind::MaterializedView => DbIcon::View.icon(),
            TableKind::Function => DbIcon::Sql.icon(),
        }
    }
}

/// Which object list the sidebar shows — switched from the panel
/// icons at the left of the status bar.
#[derive(Clone, Copy, PartialEq)]
pub enum SidebarPanel {
    Tables,
    Views,
    Functions,
}

impl SidebarPanel {
    const ALL: [SidebarPanel; 3] = [
        SidebarPanel::Tables,
        SidebarPanel::Views,
        SidebarPanel::Functions,
    ];

    fn title(self) -> &'static str {
        match self {
            SidebarPanel::Tables => "Tables",
            SidebarPanel::Views => "Views",
            SidebarPanel::Functions => "Functions",
        }
    }

    /// Monochrome Lucide line icon for the status-bar toggles
    /// (tinted via text color's panel buttons).
    fn mono_icon(self) -> Icon {
        let bytes: &[u8] = match self {
            SidebarPanel::Tables => include_bytes!("../assets/icons/ui/table.svg"),
            SidebarPanel::Views => include_bytes!("../assets/icons/ui/eye.svg"),
            SidebarPanel::Functions => include_bytes!("../assets/icons/ui/function.svg"),
        };
        Icon::default().data(bytes)
    }
}

/// Inline rename editor in the sidebar (right-click → Rename).
struct RenameState {
    kind: TableKind,
    name: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

/// Data grid or Structure view of a table tab (bottom-bar switch).
#[derive(Clone, Copy, PartialEq)]
pub enum TabView {
    Data,
    Structure,
    Index,
    Triggers,
    Ddl,
}

#[derive(Clone)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
}

/// An open grid tab: the table identity + its live DataTable state.
pub struct DataTab {
    pub table: TableRef,
    pub state: Entity<TableState<GridDelegate>>,
    pub view: TabView,
    pub structure: Option<Entity<TableState<StructureDelegate>>>,
    pub filters: FilterBar,
    subs: Vec<Subscription>,
    /// New Table being designed: its name field (⌘S creates it).
    pub draft: Option<Entity<InputState>>,
    /// Structure / Index header "Name" field of an existing table (a
    /// different name is renamed on ⌘S).
    pub rename: Option<Entity<InputState>>,
    /// Index tab: the table's indexes (editable grid) once opened.
    pub indexes: Option<Entity<TableState<crate::indexes::IndexDelegate>>>,
    pub index_count: Option<usize>,
    /// Triggers view grid (read-only) once opened, and its row count.
    pub triggers: Option<Entity<TableState<crate::sql::QueryDelegate>>>,
    pub trigger_count: Option<usize>,
    /// DDL view: the table's CREATE statement (read-only editor).
    pub ddl: Option<Entity<gpui_kit::component::input::EditorState>>,
    /// Page popover fields (Limit / Offset).
    pub page_limit_input: Entity<InputState>,
    pub page_offset_input: Entity<InputState>,
}

pub enum WorkspaceTab {
    Grid(DataTab),
    Sql(crate::sql::SqlTab),
}

impl WorkspaceTab {
    fn label(&self) -> String {
        match self {
            WorkspaceTab::Grid(t) => format!("{}.{}", t.table.schema, t.table.name),
            WorkspaceTab::Sql(t) => t.title.clone(),
        }
    }
}

/// Pending sidebar changes (drops, renames) as an undo snapshot.
type SidebarPending = (Vec<(TableKind, String)>, Vec<((TableKind, String), String)>);

pub struct TuskApp {
    screen: AppScreen,
    status_line: String,
    /// Toasts waiting for the next frame (pushed where a `Window` is at hand).
    toasts: Vec<(Option<bool>, String)>,
    form: ConnectionForm,
    pool: Option<crate::db::Db>,
    /// SSH tunnel of the active connection (kept alive with the pool).
    tunnel: Option<std::sync::Arc<crate::ssh::Tunnel>>,
    /// SQL language server for the active connection (completions, diagnostics).
    lsp: Option<std::sync::Arc<crate::lsp::LspClient>>,
    /// Catalog completion for engines without a language server.
    completer: Option<crate::complete::SchemaCompletion>,
    active_name: String,
    /// Right-side status label, e.g. "PostgreSQL 17 · name @ host:port".
    /// None while disconnected (right side stays empty).
    server_label: Option<String>,
    focus: FocusHandle,
    /// Floating command palette / quick-open.
    pub palette: Option<PaletteOverlay>,
    /// The saved-connections manager (groups, tags, search) is open.
    pub conn_manager: bool,
    /// Set on connect: the next render empties the connection search, which
    /// the manager and the connections-tree sidebar share.
    clear_conn_search: bool,
    /// Connections tree: the connected connection's objects are folded away
    /// (clicking its row toggles this; a new connection starts unfolded).
    pub(super) active_conn_folded: bool,
    /// Set when the connection manager opens: the next render focuses its search.
    pub(super) focus_conn_search: bool,
    /// Runs parallel to the currently rendered command items.
    pub palette_runs: Vec<crate::palette::RunFn>,
    /// Known tables for quick-open (filled in Phase 5).
    pub tables: Vec<TableRef>,
    /// Table picked from quick-open before the grid exists (Phase 6 consumes).
    pub pending_table: Option<TableRef>,
    // ---- sidebar (Phase 5) ----
    filter: Entity<InputState>,
    schemas: Vec<String>,
    current_schema: String,
    objects: ObjectTree,
    objects_loading: bool,
    selected_object: Option<(TableKind, String)>,
    /// The active tab's table when `selected_object` was last synced to it:
    /// switching / closing tabs moves the sidebar highlight along.
    synced_tab: Option<(TableKind, String)>,
    sidebar_panel: SidebarPanel,
    sidebar_open: bool,
    sidebar_focus: FocusHandle,
    /// Objects marked for DROP (red until ⌘S).
    pending_drops: Vec<(TableKind, String)>,
    /// Pending renames old → new (orange until ⌘S).
    pending_renames: Vec<((TableKind, String), String)>,
    /// Undo history of the two lists above.
    sidebar_history: crate::undo::History<SidebarPending>,
    /// Safe mode's "Confirm Before Saving" was answered for this ⌘S.
    save_confirmed: bool,
    /// "Migrate from TablePlus" sheet, while open.
    pub(crate) migrate: Option<migrate_sheet::MigrateSheet>,
    /// Dragging the SQL editor split: (pointer y at start, height at start).
    split_drag: Option<(f32, f32)>,
    /// Console / Problems panel under the tabs (status-bar buttons).
    bottom_panel: Option<panels::BottomPanel>,
    bottom_h: f32,
    bottom_drag: Option<(f32, f32)>,
    /// Row detail panel on the right (the selected row as a form).
    row_panel: rowdetail::RowDetailState,
    /// The AI chat panel (right side).
    ai: agent_panel::AiPanel,
    /// ⌥⌘P sheet: the pending SQL of the active tab.
    preview: Option<Entity<gpui_kit::component::input::EditorState>>,
    /// ⌘. sheet: the server's sessions.
    processes: Option<Entity<TableState<crate::sql::QueryDelegate>>>,
    /// Tools ▸ Search in Database sheet.
    db_search: Option<tools::DbSearch>,
    /// Tools ▸ User Management sheet.
    users: Option<users_view::UserMgmt>,
    /// View ▸ Toggle Query Results Pane: the SQL editor takes the whole tab.
    results_hidden: bool,
    /// ⇧⌘D: two tabs side by side (left, right) and which pane has focus.
    split: Option<(usize, usize)>,
    split_focus: usize,
    /// Slide animation: bumped per open/close; `bottom_closing` while it slides out.
    bottom_anim: usize,
    bottom_closing: bool,
    /// Console filter: None = everything, else data / meta statements.
    console_filter: Option<crate::console::Source>,
    console_seen: u64,
    history_search: Entity<InputState>,
    console_scroll: panels::ConsoleScroll,
    /// Editor settings the open SQL tabs were last given (line numbers,
    /// soft wrap, tab size) — Settings changes apply to them live.
    editor_prefs: (bool, bool, u32),
    /// Filtered Console / History lists, rebuilt only when the log or the
    /// filter changes (not on every frame).
    console_cache: std::cell::RefCell<Option<panels::ConsoleCache>>,
    history_cache: std::cell::RefCell<Option<panels::HistoryCache>>,
    /// Latest diagnostics per SQL document (the Problems panel).
    problems: Vec<(String, Vec<lsp_types::Diagnostic>)>,
    renaming: Option<RenameState>,
    filter_open: bool,
    /// The connected profile + its password (memory only) — reconnecting to
    /// another database reuses them.
    active_conn: Option<(SavedConnection, String)>,
    /// Where Postgres is reachable from here (SSH tunnel's local end) — for
    /// pg_dump / pg_restore / psql.
    reach: Option<(String, u16)>,
    databases: Vec<String>,
    saving: bool,
    // ---- grid tabs (Phase 6) ----
    /// Folders collapsed in the welcome connection list.
    conn_pick: conns::FolderPick,
    /// Sidebar folders of the current connection/database/schema.
    obj_groups: Vec<crate::objects::ObjectGroup>,
    obj_groups_collapsed: std::collections::HashSet<String>,
    obj_group_renaming: Option<(String, Entity<InputState>, Subscription)>,
    /// "Search for connection…" on the welcome list.
    conn_search: Entity<InputState>,
    /// Explicit (possibly empty) groups — `groups.json`.
    groups: Vec<String>,
    /// Inline group-name editor: (current name, input).
    group_renaming: Option<(String, Entity<InputState>, Subscription)>,
    tabs: Vec<WorkspaceTab>,
    active_tab: Option<usize>,
    /// Tab activation history for the ← → buttons.
    nav_back: Vec<usize>,
    nav_fwd: Vec<usize>,
}

impl TuskApp {
    pub fn new(
        form: ConnectionForm,
        filter: Entity<InputState>,
        conn_search: Entity<InputState>,
        history_search: Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::watch_console(cx);
        Self {
            screen: AppScreen::Connection,
            status_line: "Not connected".to_string(),
            // A connections.json that couldn't be read was set aside at load.
            toasts: db::take_load_notice()
                .map(|m| (Some(false), m))
                .into_iter()
                .collect(),
            form,
            pool: None,
            tunnel: None,
            lsp: None,
            completer: None,
            active_name: String::new(),
            server_label: None,
            focus: cx.focus_handle(),
            palette: None,
            conn_manager: false,
            clear_conn_search: false,
            active_conn_folded: false,
            focus_conn_search: false,
            palette_runs: Vec::new(),
            tables: Vec::new(),
            pending_table: None,
            filter,
            schemas: Vec::new(),
            current_schema: "public".to_string(),
            objects: ObjectTree::default(),
            objects_loading: false,
            selected_object: None,
            synced_tab: None,
            sidebar_panel: SidebarPanel::Tables,
            sidebar_open: true,
            sidebar_focus: cx.focus_handle(),
            pending_drops: Vec::new(),
            pending_renames: Vec::new(),
            sidebar_history: Default::default(),
            save_confirmed: false,
            split_drag: None,
            bottom_panel: None,
            bottom_h: 220.,
            bottom_drag: None,
            row_panel: Default::default(),
            ai: Default::default(),
            preview: None,
            processes: None,
            db_search: None,
            users: None,
            results_hidden: false,
            split: None,
            split_focus: 0,
            bottom_anim: 0,
            bottom_closing: false,
            console_filter: None,
            console_seen: 0,
            history_search,
            console_scroll: Default::default(),
            editor_prefs: {
                let p = crate::settings::get();
                (p.editor_line_numbers, p.editor_soft_wrap, p.editor_tab_size)
            },
            console_cache: Default::default(),
            history_cache: Default::default(),
            problems: Vec::new(),
            migrate: None,
            renaming: None,
            filter_open: false,
            active_conn: None,
            reach: None,
            databases: Vec::new(),
            saving: false,
            conn_pick: Default::default(),
            obj_groups: Vec::new(),
            obj_groups_collapsed: Default::default(),
            obj_group_renaming: None,
            conn_search,
            groups: db::load_groups(),
            group_renaming: None,
            tabs: Vec::new(),
            active_tab: None,
            nav_back: Vec::new(),
            nav_fwd: Vec::new(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Title bar. Left: `⛁ database ▾ / ◫ schema ▾` (like an editor's
    /// `project ⎇ branch`) — each opens the kit dropdown to switch database /
    /// schema. Right: the connection pill `● Local Docker  host:port ▾`
    /// (switch connection, new connection, disconnect).
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let (fg, muted) = (t.colors.foreground, t.colors.muted_foreground);
        if self.screen != AppScreen::Workspace {
            return TitleBar::new()
                .child(
                    div()
                        .text_sm()
                        .font_family(crate::settings::ui_font())
                        .text_color(muted)
                        .child("No connection"),
                )
                .child(div().pr_2().children(
                    crate::updater::ready(cx).map(|v| Self::render_update_button(v, cx)),
                ));
        }
        let crumb = |id: &'static str, bytes: &'static [u8], text: String| {
            Button::new(id).ghost().small().child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .font_family(crate::settings::ui_font())
                    .text_color(muted)
                    .child(Icon::default().data(bytes).size(px(13.)))
                    .child(text)
                    .child(Icon::new(IconName::ChevronDown).size(px(11.))),
            )
        };
        let current_db = self.current_database();
        let dbs = self.databases.clone();
        // Long lists: the dropdown shows the recent ones (the current
        // database first) and a search entry — ⌘K opens the searchable
        // palette. Short lists just show everything.
        let mut recent: Vec<String> = crate::settings::recent_databases(&self.active_name)
            .into_iter()
            .filter(|d| dbs.contains(d))
            .take(10)
            .collect();
        if !current_db.is_empty() && !recent.contains(&current_db) {
            recent.insert(0, current_db.clone());
        }
        recent.truncate(10);
        let long = dbs.len() > 10;
        let db_menu = crumb(
            "title-db",
            include_bytes!("../assets/icons/ui/database.svg"),
            if current_db.is_empty() {
                "Select database".to_string()
            } else {
                current_db.clone()
            },
        )
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.max_h(px(360.)).scrollable(true);
            if long {
                menu = menu
                    .item(
                        PopupMenuItem::new("Search Databases…").on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenDatabase), cx);
                        }),
                    )
                    .separator();
            }
            let list: &Vec<String> = if long { &recent } else { &dbs };
            for name in list {
                let go = name.clone();
                menu = menu.item(
                    PopupMenuItem::new(name.clone())
                        .checked(*name == current_db)
                        .on_click(move |_, window, cx| {
                            let view = cx.global::<TuskHandle>().0.clone();
                            view.update(cx, |this, cx| {
                                this.switch_database(go.clone(), window, cx)
                            });
                        }),
                );
            }
            menu
        });
        let current_schema = self.current_schema.clone();
        let schemas = self.schemas.clone();
        let schema_menu = crumb(
            "title-schema",
            include_bytes!("../assets/icons/ui/layers.svg"),
            current_schema.clone(),
        )
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.max_h(px(360.)).scrollable(true);
            for name in &schemas {
                let go = name.clone();
                menu = menu.item(
                    PopupMenuItem::new(name.clone())
                        .checked(*name == current_schema)
                        .on_click(move |_, _, cx| {
                            let view = cx.global::<TuskHandle>().0.clone();
                            view.update(cx, |this, cx| this.switch_schema(go.clone(), cx));
                        }),
                );
            }
            menu
        });

        // Connection pill (right).
        let endpoint = self
            .active_conn
            .as_ref()
            .map(|(c, _)| match &c.ssh {
                Some(ssh) => format!("{} via ssh {}", c.endpoint(), ssh.host),
                None => c.endpoint(),
            })
            .unwrap_or_default();
        let saved = self.form.saved.clone();
        let active = self.active_name.clone();
        // the connection's status color tints its toolbar pill.
        let status: gpui::Hsla = self
            .active_conn
            .as_ref()
            .and_then(|(c, _)| c.status_rgb())
            .map_or(muted.opacity(0.6), |c| rgb(c).into());
        let pill = Button::new("title-conn")
            .ghost()
            .small()
            .bg(status.opacity(0.18))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .font_family(crate::settings::ui_font())
                    .child(div().size(px(7.)).rounded_full().bg(status))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(fg)
                            .child(active.clone()),
                    )
                    .child(div().text_caption().text_color(muted).child(endpoint))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(px(11.))
                            .text_color(muted),
                    ),
            )
            .dropdown_menu_with_anchor(gpui_kit::Anchor::TopRight, move |menu, _, _| {
                let mut menu = menu;
                for (ix, conn) in saved.iter().enumerate() {
                    menu = menu.item(
                        PopupMenuItem::new(conn.name.clone())
                            .checked(conn.name == active)
                            .on_click(move |_, window, cx| {
                                let view = cx.global::<TuskHandle>().0.clone();
                                view.update(cx, |this, cx| {
                                    this.switch_connection_guarded(ix, window, cx)
                                });
                            }),
                    );
                }
                menu.separator()
                    .item(
                        PopupMenuItem::new("New Connection…").on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(NewConnection), cx);
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Backup Database…").on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(BackupDatabase), cx);
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Restore Database…").on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(RestoreDatabase), cx);
                        }),
                    )
                    .separator()
                    .item(PopupMenuItem::new("Disconnect").on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(Disconnect), cx);
                    }))
            });

        // One picker where a database and a schema are the same thing.
        let single_crumb = self
            .pool
            .as_ref()
            .is_some_and(|p| p.engine().databases_are_schemas());
        // On two-level engines, a lone schema ("main" on D1, a db with only
        // "public") doesn't get a crumb.
        let show_schema = single_crumb || self.schemas.len() > 1;
        TitleBar::new()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .when(!single_crumb, |d| d.child(db_menu))
                    .when(show_schema, |d| {
                        d.when(!single_crumb, |d| {
                            d.child(div().text_color(muted.opacity(0.5)).child("/"))
                        })
                        .child(schema_menu)
                    }),
            )
            .child(
                div()
                    .pr_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(crate::updater::ready(cx).map(|v| Self::render_update_button(v, cx)))
                    .child(pill),
            )
    }

    /// Title bar, right (left of the connection): a downloaded update waiting
    /// for a restart.
    pub(crate) fn render_update_button(version: String, cx: &App) -> AnyElement {
        let t = cx.theme();
        let accent = t.accent;
        let tip = if version.is_empty() {
            "Install the update and relaunch".to_string()
        } else {
            format!("Tusk {version} is ready: install and relaunch")
        };
        div()
            .id("status-update")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(20.))
            .rounded(crate::theme::RADIUS_SM)
            .text_caption()
            .font_family(crate::settings::ui_font())
            .font_weight(FontWeight::MEDIUM)
            .text_color(accent)
            .bg(accent.opacity(0.14))
            .hover(|d| d.bg(accent.opacity(0.24)))
            .child(Icon::new(IconName::ArrowDownToLine).size(px(12.)))
            .child("Restart to Update")
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_click(|_, _, _| crate::updater::restart_to_update())
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let toggles = self.render_panel_toggles(cx);
        let workspace = self.screen == AppScreen::Workspace;
        // Lint counts only matter while a query tab is open.
        let has_query = self.tabs.iter().any(|t| matches!(t, WorkspaceTab::Sql(_)));
        let problems = (workspace && has_query).then(|| self.render_problems_button(cx));
        let console = self.render_console_button(cx);
        let history = self.render_history_button(cx);
        let row_detail = self.render_row_detail_button(cx);
        let ai_button = self.render_ai_button(cx);
        let t = cx.theme().clone();
        // (Row counts live in each tab's own footer; connection identity in
        // the title bar — the status bar only carries transient messages.)
        // Layout: [toggles · message] | centered pending changes | [spacer].
        let pending = self.pending_summary(cx).map(|p| {
            div()
                .text_caption()
                .font_family(crate::settings::ui_font())
                .text_color(rgb(crate::theme::EDITED))
                .child(crate::kbd::rich_colored(
                    &format!("{p} — [cmd-s] to save"),
                    rgb(crate::theme::EDITED).into(),
                ))
        });
        div()
            .h(px(crate::settings::bar_h()))
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(t.colors.border)
            .bg(t.colors.status_bar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .child(toggles)
                    .children(problems)
                    .child(
                        div()
                            .text_caption()
                            .font_family(crate::settings::ui_font())
                            .text_color(t.colors.muted_foreground)
                            .truncate()
                            .child(self.status_line.clone()),
                    ),
            )
            .children(pending)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .h_full()
                    .items_center()
                    .justify_end()
                    .when(self.screen == AppScreen::Workspace, |this| {
                        // Always-visible way into the SQL editor (also ⌘T
                        // and the tab bar's "+").
                        this.gap_1()
                            .child(history)
                            .child(console)
                            .child(
                                div()
                                    .id("status-sql")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .px_1()
                                    // Same box as the panel toggles on the left.
                                    .h(px(20.))
                                    .rounded(crate::theme::RADIUS_SM)
                                    .text_color(t.colors.muted_foreground)
                                    .hover(|this| {
                                        this.bg(t.colors.muted_foreground.opacity(0.12))
                                            .text_color(t.colors.foreground)
                                    })
                                    // Mono "SQL" glyph, same weight as the panel icons.
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .line_height(px(12.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .font_family(crate::settings::table_font())
                                            .child("SQL"),
                                    )
                                    .tooltip(|window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new("New SQL Query")
                                            .key_binding(crate::kbd::tip("cmd-t"))
                                            .build(window, cx)
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_sql_tab(window, cx)
                                    })),
                            )
                            .child(ai_button)
                            // The right-sidebar toggle stays rightmost.
                            .child(row_detail)
                    }),
            )
    }

    /// Panel toggles at the left of the status bar: pick the
    /// sidebar list; clicking the active one hides / shows the sidebar.
    fn render_panel_toggles(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.screen != AppScreen::Workspace {
            return DbIcon::Database.icon().into_any_element();
        }
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        // bare line icons, muted; the open panel's icon is tinted blue.
        let active_tint = cx.theme().accent;
        let mut row = div().flex().items_center().gap_1();
        // Only the object kinds the engine has (no Functions for SQLite…).
        let caps = self.pool.as_ref().map(|p| p.caps());
        for panel in SidebarPanel::ALL {
            let shown = match (panel, caps) {
                (SidebarPanel::Views, Some(c)) => c.views,
                (SidebarPanel::Functions, Some(c)) => c.functions,
                _ => true,
            };
            if !shown {
                continue;
            }
            let active = self.sidebar_open && self.sidebar_panel == panel;
            row = row.child(
                div()
                    .id(SharedString::from(format!("panel-{}", panel.title())))
                    .w(px(22.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(crate::theme::RADIUS_SM)
                    .text_color(if active { active_tint } else { muted })
                    .hover(|this| {
                        this.bg(muted.opacity(0.12)).text_color(if active {
                            active_tint
                        } else {
                            fg
                        })
                    })
                    .child(panel.mono_icon().size(px(14.)))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(panel.title()).build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.sidebar_panel == panel {
                            this.sidebar_open = !this.sidebar_open;
                        } else {
                            this.sidebar_panel = panel;
                            this.sidebar_open = true;
                        }
                        cx.notify();
                    })),
            );
        }
        row.child(div().w(px(1.)).h(px(14.)).mx_1().bg(cx.theme().border))
            .into_any_element()
    }

    // ---------- connection screen ----------

    fn on_pick_saved(
        &mut self,
        ix: usize,
        double: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(conn) = self.form.saved.get(ix).cloned() {
            self.form.selected = Some(ix);
            self.form.load(&conn, window, cx);
            cx.notify();
            if double {
                self.conn_manager = false;
                self.connect_now(window, cx);
            }
        }
    }

    fn read_form_or_notice(&mut self, cx: &mut Context<Self>) -> Option<(SavedConnection, String)> {
        let (conn, typed) = self.form.read_form(cx);
        let password = crate::conn::ConnectionForm::password_or_keychain(&conn.name, typed);
        if !conn.is_valid() {
            self.form.notice = Some((
                false,
                "The saved connection is incomplete — edit it and fill in its fields.".to_string(),
            ));
            cx.notify();
            return None;
        }
        // No password is fine: local SQLite / Redis / … need none, and a
        // server that does says so itself.
        Some((conn, password))
    }

    fn on_test(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.read_form_or_notice(cx) else {
            return;
        };
        self.form.busy = true;
        self.form.notice = Some((true, "Testing connection…".to_string()));
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = db::test_connect(conn, password, None).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.form.busy = false;
                this.form.notice = Some(match result {
                    Ok(v) => (true, v.summary()),
                    Err(e) => (false, e),
                });
                cx.notify();
            });
        })
        .detach();
        let _ = window;
    }

    fn on_save(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.read_form_or_notice(cx) else {
            return;
        };
        match db::save_password(&conn.name, &password) {
            Ok(()) => {}
            Err(e) => {
                self.form.notice = Some((false, format!("{} error: {e}", db::CREDENTIAL_STORE)));
                cx.notify();
                return;
            }
        }
        // Verify the write by reading back (see dialog.rs: environments that
        // silently drop Keychain writes must not get a false "Saved").
        let keychain_warning = match db::load_password(&conn.name) {
            Ok(back) if back == password => None,
            Ok(_) => Some(format!(
                "the {} gave back a different password",
                db::CREDENTIAL_STORE
            )),
            Err(e) => Some(format!(
                "the password can't be read back from the {} ({e})",
                db::CREDENTIAL_STORE
            )),
        };
        match db::upsert_connection(conn.clone(), None) {
            Ok((list, ix)) => {
                self.form.saved = list;
                self.form.selected = Some(ix);
                self.form.notice = Some(match keychain_warning {
                    Some(w) => (
                        false,
                        format!(
                            "Saved “{}” — but {w}; it won't survive a restart.",
                            conn.name
                        ),
                    ),
                    None => (true, format!("Saved “{}”.", conn.name)),
                });
            }
            Err(e) => {
                self.form.notice = Some((false, format!("Save failed: {e:#}")));
            }
        }
        cx.notify();
    }

    fn connect_now(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.read_form_or_notice(cx) else {
            return;
        };
        self.form.busy = true;
        self.form.notice = Some((true, format!("Connecting to {}…", conn.name)));
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = db::connect(conn.clone(), password.clone(), None).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.form.busy = false;
                match result {
                    Ok(c) => this.connected_with(c, &password, cx),
                    Err(e) => {
                        // Outside the dialog (welcome list, ⌘1…) only a toast is
                        // seen: render turns the error notice into one.
                        log::warn!("connect {}: {e}", conn.name);
                        this.form.notice = Some((false, format!("{}: {e}", conn.name)));
                        // No / wrong saved password: open the form to type it.
                        if Self::wants_password(&conn, &password, &e) {
                            crate::dialog::ConnDialog::open_edit(conn.clone(), false, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---------- sidebar (Phase 5) ----------

    /// After a successful connect: fetch schemas, pick default, load objects.
    fn load_sidebar_after_connect(&mut self, cx: &mut Context<Self>) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        // A panel this engine doesn't have (Functions on SQLite) → Tables.
        let caps = pool.caps();
        if (self.sidebar_panel == SidebarPanel::Views && !caps.views)
            || (self.sidebar_panel == SidebarPanel::Functions && !caps.functions)
        {
            self.sidebar_panel = SidebarPanel::Tables;
        }
        self.objects_loading = true;
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let schemas = db::fetch_schemas(&pool).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| match schemas {
                Ok(list) if !list.is_empty() => {
                    this.schemas = list;
                    let preferred = pool.driver().default_schema();
                    let def = match preferred {
                        Some(p) if this.schemas.contains(&p) => p,
                        _ if this.schemas.iter().any(|s| s == "public") => "public".to_string(),
                        _ => this.schemas[0].clone(),
                    };
                    this.current_schema = def.clone();
                    this.fetch_objects_for(&def, cx);
                }
                Ok(_) => {
                    this.objects_loading = false;
                    this.toast(false, "Connected — no schemas found.");
                    cx.notify();
                }
                Err(e) => {
                    this.objects_loading = false;
                    this.toast(false, format!("Connected, but schema list failed: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn fetch_objects_for(&mut self, schema: &str, cx: &mut Context<Self>) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let schema = schema.to_string();
        if let Some(c) = &self.completer {
            c.set_schema(&schema);
        }
        self.objects_loading = true;
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = db::fetch_objects(&pool, &schema).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.objects_loading = false;
                match result {
                    Ok(tree) => {
                        let counts = tree.tables.len() + tree.views.len() + tree.matviews.len();
                        this.objects = tree;
                        this.reload_obj_groups();
                        this.refresh_quick_open_tables();
                        let _ = counts;
                        this.status_line.clear();
                    }
                    Err(e) => {
                        this.toast(false, format!("Object list failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Switch schema from the bottom selector.
    pub fn switch_schema(&mut self, schema: String, cx: &mut Context<Self>) {
        if schema == self.current_schema || self.pool.is_none() {
            return;
        }
        self.current_schema = schema.clone();
        self.selected_object = None;
        self.fetch_objects_for(&schema, cx);
    }

    /// Keep cmd+p in sync with the sidebar.
    fn refresh_quick_open_tables(&mut self) {
        let schema = self.current_schema.clone();
        let mut tables = Vec::new();
        for name in &self.objects.tables {
            tables.push(TableRef {
                schema: schema.clone(),
                name: name.clone(),
                kind: TableKind::Table,
            });
        }
        for name in &self.objects.views {
            tables.push(TableRef {
                schema: schema.clone(),
                name: name.clone(),
                kind: TableKind::View,
            });
        }
        for name in &self.objects.matviews {
            tables.push(TableRef {
                schema: schema.clone(),
                name: name.clone(),
                kind: TableKind::MaterializedView,
            });
        }
        self.tables = tables;
    }

    fn on_pick_object(
        &mut self,
        kind: TableKind,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_object = Some((kind.clone(), name.clone()));
        match kind {
            TableKind::Function => {
                self.status_line = format!(
                    "{}.{}() — function detail lands with tabs (Phase 6)",
                    self.current_schema, name
                );
                cx.notify();
            }
            kind => {
                let table = TableRef {
                    schema: self.current_schema.clone(),
                    name,
                    kind,
                };
                self.open_grid_tab(table, window, cx);
            }
        }
    }

    /// Open a table grid tab (or activate it if already open).
    pub fn open_grid_tab(&mut self, table: TableRef, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != AppScreen::Workspace || self.pool.is_none() {
            return;
        }
        if let Some(ix) = self.tabs.iter().position(|t| {
            matches!(t, WorkspaceTab::Grid(g) if g.table.schema == table.schema && g.table.name == table.name)
        }) {
            self.activate_tab(ix, cx);
            self.pending_table = Some(table);
            return;
        }
        let pool = self.pool.clone().unwrap();
        let editable = table.kind == TableKind::Table && pool.caps().edit_rows;
        let mut delegate =
            GridDelegate::new(pool, table.schema.clone(), table.name.clone(), editable);
        delegate.width_key = Some(format!(
            "{}/{}/{}.{}",
            self.active_name,
            self.current_database(),
            table.schema,
            table.name
        ));
        let state = grid::new_state(delegate, window, cx);
        // Double-click a cell → in-place editor (row turns orange).
        let sub = cx.subscribe_in(
            &state,
            window,
            |_this, state, ev: &TableEvent, window, cx| match ev {
                TableEvent::DoubleClickedCell(r, c) => {
                    let (r, c) = (*r, *c);
                    state.update(cx, |st, cx| st.delegate_mut().begin_edit(r, c, window, cx));
                }
                TableEvent::ColumnWidthsChanged(widths) => {
                    let widths = widths.clone();
                    state.update(cx, |st, _| st.delegate_mut().remember_widths(&widths));
                }
                _ => {}
            },
        );
        self.tabs.push(WorkspaceTab::Grid(DataTab {
            table: table.clone(),
            state: state.clone(),
            view: TabView::Data,
            structure: None,
            filters: FilterBar::default(),
            subs: vec![sub],
            draft: None,
            rename: None,
            indexes: None,
            index_count: None,
            triggers: None,
            trigger_count: None,
            ddl: None,
            page_limit_input: Self::page_input(grid::PAGE_LIMIT, window, cx),
            page_offset_input: Self::page_input(0, window, cx),
        }));
        self.activate_tab(self.tabs.len() - 1, cx);
        self.pending_table = Some(table.clone());
        cx.notify();
        // Initial metadata + count + first window off the UI thread.
        grid::reload(&state, cx);
    }

    /// Run `f` on the active tab's data grid (Data view only).
    fn with_active_grid(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(
            &mut TableState<GridDelegate>,
            &mut Window,
            &mut Context<TableState<GridDelegate>>,
        ),
    ) {
        let Some(WorkspaceTab::Grid(g)) = self.active_tab.and_then(|ix| self.tabs.get(ix)) else {
            return;
        };
        if g.view != TabView::Data {
            return;
        }
        let st = g.state.clone();
        st.update(cx, |s, cx| f(s, window, cx));
        cx.notify();
    }

    pub(super) fn page_input(
        v: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(v.to_string(), window, cx);
            st
        })
    }

    pub fn open_table_ref(&mut self, table: TableRef, window: &mut Window, cx: &mut Context<Self>) {
        self.open_grid_tab(table, window, cx);
    }

    pub(super) fn close_tab_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.tabs.remove(ix);
        // A pane showing the closed tab closes the split; others shift down.
        self.split = self.split.and_then(|(l, r)| {
            if l == ix || r == ix {
                None
            } else {
                Some((l - usize::from(l > ix), r - usize::from(r > ix)))
            }
        });
        // Keep the ← → history pointing at the same tabs.
        for stack in [&mut self.nav_back, &mut self.nav_fwd] {
            stack.retain(|&i| i != ix);
            for i in stack.iter_mut() {
                if *i > ix {
                    *i -= 1;
                }
            }
            stack.dedup();
        }
        self.active_tab = match self.active_tab {
            Some(a) if a == ix => {
                if self.tabs.is_empty() {
                    None
                } else {
                    Some(ix.min(self.tabs.len() - 1))
                }
            }
            Some(a) if a > ix => Some(a - 1),
            other => other,
        };
        cx.notify();
    }

    /// ⌘W: a tab with unsaved changes (or a New Table draft) asks first.
    pub(super) fn close_tab_guarded(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = self.tab_pending(ix, cx);
        if n == 0 {
            self.close_tab_at(ix, cx);
            self.focus.focus(window, cx);
            return;
        }
        let is_draft =
            matches!(self.tabs.get(ix), Some(WorkspaceTab::Grid(g)) if g.draft.is_some());
        let detail = if is_draft {
            "The new table hasn't been created yet.".to_string()
        } else {
            format!(
                "{n} unsaved change{} will be discarded.",
                if n == 1 { "" } else { "s" }
            )
        };
        let answer = crate::dialog_keys::confirm(
            window,
            PromptLevel::Warning,
            "Close this tab?",
            Some(&detail),
            "Discard",
            cx,
        );
        cx.spawn_in(window, async move |weak, cx| {
            if !answer.await {
                return;
            }
            let _ = weak.update_in(cx, |this: &mut TuskApp, window, cx| {
                if ix < this.tabs.len() {
                    this.close_tab_at(ix, cx);
                    this.focus.focus(window, cx);
                }
            });
        })
        .detach();
    }

    fn refresh_active_tab(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.active_tab else { return };
        let is_sql = matches!(self.tabs.get(ix), Some(WorkspaceTab::Sql(_)));
        if is_sql {
            self.run_sql_in_tab(ix, crate::sql::RunScope::Last, cx);
            return;
        }
        let Some(WorkspaceTab::Grid(g)) = self.tabs.get(ix) else {
            return;
        };
        let state = g.state.clone();
        // Pending edits survive a refresh (keyed by ctid).
        grid::reload(&state, cx);
    }

    // ---------- SQL tabs (Phase 7) ----------

    pub fn open_sql_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_sql_tab_with(None, window, cx);
    }

    /// New query tab, prefilled with `text` (scripts from the object menu)
    /// or a SELECT on the first table of the schema.
    pub fn open_sql_tab_with(
        &mut self,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.screen != AppScreen::Workspace || self.pool.is_none() {
            return;
        }
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, WorkspaceTab::Sql(_)))
            .count()
            + 1;
        // A plain ⌘T starts from a SELECT on the open table (or the
        // schema's first one), selected so typing replaces it.
        let starter = text.is_none();
        let initial = text.unwrap_or_else(|| {
            let open = self.active_tab.and_then(|ix| match self.tabs.get(ix) {
                Some(WorkspaceTab::Grid(g)) if g.draft.is_none() => {
                    Some((g.table.schema.clone(), g.table.name.clone()))
                }
                _ => None,
            });
            match open {
                Some((schema, table)) => {
                    crate::ddl::starter_query(db::engine(), &schema, Some(table.as_str()))
                }
                None => crate::ddl::starter_query(
                    db::engine(),
                    &self.current_schema,
                    self.objects.tables.first().map(String::as_str),
                ),
            }
        });
        let editor = cx.new(|cx| {
            gpui_kit::component::input::EditorState::new(window, cx)
                .language("sql")
                .line_number(crate::settings::get().editor_line_numbers)
                .soft_wrap(crate::settings::get().editor_soft_wrap)
                .tab_size(gpui_kit::component::input::TabSize {
                    tab_size: crate::settings::get().editor_tab_size as usize,
                    hard_tabs: false,
                })
                .default_value(initial)
                .placeholder("Write SQL…")
        });
        let result = crate::sql::new_result_state(window, cx);
        let mut tab = crate::sql::SqlTab::new(format!("SQL-{n}"), editor.clone(), result.clone());
        // Double-click an editable result cell → in-place editor.
        tab.subs.push(cx.subscribe_in(
            &result,
            window,
            |_this, st, ev: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(r, c) = ev {
                    let (r, c) = (*r, *c);
                    st.update(cx, |st, cx| st.delegate_mut().begin_edit(r, c, window, cx));
                }
            },
        ));
        // Language server: the editor's own completion menu is fed by the
        // server; every edit is synced so diagnostics stay current.
        if let Some(client) = self.lsp.clone() {
            let doc = crate::lsp::SqlDocument::open(client, &editor.read(cx).text().to_string());
            let provider: std::rc::Rc<dyn gpui_kit::component::input::CompletionProvider> =
                std::rc::Rc::new(doc.clone());
            editor.update(cx, |st, _| {
                let lsp = st.lsp_mut();
                lsp.completion_provider = Some(provider);
                // Room for `name  Table · schema` without truncation.
                lsp.completion_menu.max_width = px(460.);
            });
            let sync_doc = doc.clone();
            tab.subs.push(cx.subscribe(
                &editor,
                move |_, editor, ev: &gpui_kit::component::input::InputEvent, cx| {
                    if matches!(ev, gpui_kit::component::input::InputEvent::Change) {
                        sync_doc.sync(&editor.read(cx).text().to_string());
                    }
                },
            ));
            tab.doc = Some(doc);
        } else if let Some(c) = self.completer.clone() {
            let provider: std::rc::Rc<dyn gpui_kit::component::input::CompletionProvider> =
                std::rc::Rc::new(c);
            editor.update(cx, |st, _| {
                let lsp = st.lsp_mut();
                lsp.completion_provider = Some(provider);
                lsp.completion_menu.max_width = px(460.);
            });
        }
        self.tabs.push(WorkspaceTab::Sql(tab));
        self.activate_tab(self.tabs.len() - 1, cx);
        editor.read(cx).focus_handle(cx).focus(window, cx);
        if starter {
            editor.update(cx, |e, cx| e.select_all(window, cx));
        }
        cx.notify();
    }

    /// Execute the active SQL tab: whole buffer, or selection when non-empty.
    /// Run SQL in a query tab: a selection always
    /// runs as-is; otherwise `Current` runs the statement under the cursor
    /// and `All` the whole editor. Each SELECT becomes its own result tab.
    pub fn run_sql_in_tab(
        &mut self,
        ix: usize,
        scope: crate::sql::RunScope,
        cx: &mut Context<Self>,
    ) {
        use crate::sql::{QueryOutput, ResultSet};
        let Some(WorkspaceTab::Sql(tab)) = self.tabs.get(ix) else {
            return;
        };
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let sql = Self::scope_sql(tab, scope, cx);
        if sql.trim().is_empty() {
            return;
        }
        let result_state = tab.result.clone();
        // A completion menu left open over the editor would cover the run.
        tab.editor.update(cx, |e, cx| e.dismiss_lsp_overlays(cx));
        if let Some(WorkspaceTab::Sql(tab)) = self.tabs.get_mut(ix) {
            tab.running = true;
            tab.output = QueryOutput::running();
            tab.last_sql = Some(sql.clone());
        }
        cx.notify();
        let (hist_conn, hist_db) = self
            .active_conn
            .as_ref()
            .map(|(c, _)| (c.name.clone(), c.database.clone()))
            .unwrap_or_default();
        let run_schema = self.current_schema.clone();
        cx.spawn(async move |_weak, cx: &mut AsyncApp| {
            let t0 = std::time::Instant::now();
            let statements = db::split_statements(&sql);
            // Every statement the user runs lands in History.
            let hist = |stmt: &str, started: std::time::Instant, ok: bool| {
                crate::console::add_history(crate::console::HistoryItem {
                    at: chrono::Local::now(),
                    sql: stmt.trim().to_string(),
                    connection: hist_conn.clone(),
                    database: hist_db.clone(),
                    ms: started.elapsed().as_millis(),
                    ok,
                })
            };
            let mut sets: Vec<ResultSet> = Vec::new();
            // Per-statement log for scripts ("UPDATE … 3 rows affected").
            let mut log: Vec<String> = Vec::new();
            let mut error: Option<QueryOutput> = None;
            for stmt in &statements {
                let started = std::time::Instant::now();
                match db::classify_statement(stmt) {
                    db::StmtKind::Query => {
                        let rows =
                            match db::run_query_rows(&pool, stmt, crate::sql::QUERY_ROW_LIMIT + 1)
                                .await
                            {
                                Ok(rows) => {
                                    hist(stmt, started, true);
                                    rows
                                }
                                Err(e) => {
                                    hist(stmt, started, false);
                                    error = Some(error_output(e, t0));
                                    break;
                                }
                            };
                        let truncated = rows.len() as i64 > crate::sql::QUERY_ROW_LIMIT;
                        let mut rows = rows;
                        rows.truncate(crate::sql::QUERY_ROW_LIMIT as usize);
                        let columns = if rows.is_empty() {
                            match db::run_query_columns(&pool, stmt).await {
                                Ok(names) => names
                                    .into_iter()
                                    .map(|name| crate::sql::QCol { name, right: false })
                                    .collect(),
                                Err(e) => {
                                    error = Some(error_output(e, t0));
                                    break;
                                }
                            }
                        } else {
                            crate::sql::infer_columns(&rows)
                        };
                        let n = rows.len();
                        let mut output = QueryOutput {
                            columns,
                            rows: crate::sql::rows_to_vec(rows),
                            truncated,
                            message: Some(if truncated {
                                format!(
                                    "{} (truncated at {})",
                                    crate::sql::n_rows(n),
                                    crate::sql::QUERY_ROW_LIMIT
                                )
                            } else {
                                crate::sql::n_rows(n)
                            }),
                            error: None,
                            ms: started.elapsed().as_millis(),
                            edit_note: None,
                        };
                        // Editable result? (one base table with its primary key selected)
                        let names: Vec<String> =
                            output.columns.iter().map(|c| c.name.clone()).collect();
                        let source =
                            match db::result_edit_source(&pool, stmt, &run_schema, names).await {
                                Ok(Ok(src)) => Some(src),
                                Ok(Err(reason)) => {
                                    output.edit_note = Some(reason);
                                    None
                                }
                                Err(_) => None,
                            };
                        log.push(format!("{} — {}", first_line(stmt), crate::sql::n_rows(n)));
                        sets.push(ResultSet { output, source });
                    }
                    db::StmtKind::Mutation | db::StmtKind::Other => {
                        match db::run_exec(&pool, stmt).await {
                            Ok(affected) => {
                                hist(stmt, started, true);
                                log.push(format!(
                                    "{} — {} affected",
                                    first_line(stmt),
                                    crate::sql::n_rows(affected as usize)
                                ))
                            }
                            Err(e) => {
                                hist(stmt, started, false);
                                error = Some(error_output(e, t0));
                                break;
                            }
                        }
                    }
                }
            }
            let ms = t0.elapsed().as_millis();
            // Shown in the pane: the error, else the last result set, else the
            // statement log (a script with no SELECT).
            let active = sets.len().saturating_sub(1);
            let output = match (error, sets.last()) {
                (Some(mut e), _) => {
                    if !log.is_empty() {
                        let done = log.join("\n");
                        e.error = e
                            .error
                            .map(|msg| format!("{msg}\n\nCompleted before the error:\n{done}"));
                    }
                    e
                }
                (None, Some(set)) => set.output.clone(),
                (None, None) => QueryOutput {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    truncated: false,
                    message: Some(if log.len() == 1 {
                        log[0].split(" — ").last().unwrap_or_default().to_string()
                    } else {
                        log.join("\n")
                    }),
                    error: None,
                    ms,
                    edit_note: None,
                },
            };
            let shown = if output.error.is_none() {
                sets.get(active).cloned()
            } else {
                None
            };
            result_state.update(cx, |state, cx| {
                let (cols, rows, source) = match shown {
                    Some(set) => (set.output.columns, set.output.rows, set.source),
                    None => (Vec::new(), Vec::new(), None),
                };
                state.delegate_mut().set_result(cols, rows, source);
                // Query columns also arrive after construction; rebuild layout.
                state.refresh(cx);
                cx.notify();
            });
            cx.update(|cx| {
                let view = cx.global::<TuskHandle>().0.clone();
                view.update(cx, |this: &mut TuskApp, cx| {
                    if let Some(WorkspaceTab::Sql(tab)) = this.tabs.get_mut(ix) {
                        tab.running = false;
                        tab.output = output;
                        tab.results = sets;
                        tab.active_result = active;
                    }
                    cx.notify();
                });
            });
        })
        .detach();

        fn first_line(stmt: &str) -> String {
            let line = stmt.trim().lines().next().unwrap_or_default();
            if line.chars().count() > 60 {
                format!("{}…", line.chars().take(60).collect::<String>())
            } else {
                line.to_string()
            }
        }

        fn error_output(e: String, t0: std::time::Instant) -> crate::sql::QueryOutput {
            crate::sql::QueryOutput {
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
                message: None,
                edit_note: None,
                error: Some(e),
                ms: t0.elapsed().as_millis(),
            }
        }
    }

    /// The SQL a run executes: a selection always wins; otherwise the
    /// statement under the cursor (Current), the editor (All) or the last run.
    fn scope_sql(tab: &crate::sql::SqlTab, scope: crate::sql::RunScope, cx: &App) -> String {
        use crate::sql::RunScope;
        let editor = tab.editor.read(cx);
        let full = editor.text().to_string();
        let selected = editor.selected_text().to_string();
        let cursor = editor.cursor();
        match scope {
            RunScope::Last => tab.last_sql.clone().unwrap_or(full),
            _ if !selected.trim().is_empty() => selected,
            RunScope::All => full,
            RunScope::Current => db::statement_at(&full, cursor).unwrap_or_default(),
        }
    }

    /// ⌘↵ / ⇧⌘↵. Also bound in the "Input" context (see bind_keys), so
    /// ignore them while the palette owns the keyboard. Safe mode asks
    /// before dangerous statements.
    /// ⌃Space in the SQL editor: ask the language server for completions at
    /// the cursor and open the menu over the word being typed.
    fn show_completions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::input::CompletionProvider as _;
        let Some(WorkspaceTab::Sql(tab)) = self.active_tab.and_then(|ix| self.tabs.get(ix)) else {
            return;
        };
        let (Some(doc), editor) = (tab.doc.clone(), tab.editor.clone()) else {
            return;
        };
        let (rope, offset) = {
            let st = editor.read(cx);
            (st.text().clone(), st.cursor())
        };
        let full = rope.to_string();
        let prefix = crate::lsp::word_prefix(&full, offset);
        let start = offset.saturating_sub(prefix.len());
        let context = lsp_types::CompletionContext {
            trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
            trigger_character: None,
        };
        let task = doc.completions(&rope, offset, context, window, cx);
        cx.spawn(async move |_, cx: &mut AsyncApp| {
            let items = match task.await {
                Ok(lsp_types::CompletionResponse::Array(items)) => items,
                Ok(lsp_types::CompletionResponse::List(list)) => list.items,
                Err(_) => return,
            };
            editor.update(cx, |st, cx| {
                st.present_completion_items(start, prefix, items, cx)
            });
        })
        .detach();
    }

    fn run_action(
        &mut self,
        scope: crate::sql::RunScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            return;
        }
        let Some(ix) = self.active_tab else { return };
        let Some(WorkspaceTab::Sql(tab)) = self.tabs.get(ix) else {
            return;
        };
        let sql = Self::scope_sql(tab, scope, cx);
        let prefs = crate::settings::get();
        let stmts = db::split_statements(&sql);
        let risky: Vec<&String> = stmts
            .iter()
            .filter(|s| prefs.confirm_destructive && db::is_destructive(s))
            .collect();
        // "Confirm Before Saving" also covers writes run from the editor.
        let writes: Vec<&String> = stmts
            .iter()
            .filter(|s| prefs.confirm_save && db::is_write(s))
            .collect();
        let (level, title, shown) = if !risky.is_empty() {
            (PromptLevel::Warning, "Run a dangerous statement?", risky)
        } else if !writes.is_empty() {
            let title = if writes.len() == 1 {
                "Run a statement that writes to the database?"
            } else {
                "Run statements that write to the database?"
            };
            (PromptLevel::Info, title, writes)
        } else {
            self.run_sql_in_tab(ix, scope, cx);
            return;
        };
        const MAX_SHOWN: usize = 8;
        let mut detail = shown
            .iter()
            .take(MAX_SHOWN)
            .map(|s| s.trim().lines().next().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if shown.len() > MAX_SHOWN {
            detail.push_str(&format!("\n… and {} more", shown.len() - MAX_SHOWN));
        }
        let answer = crate::dialog_keys::confirm(
            window,
            level,
            title,
            Some(&format!("{detail}\n\n(Safe mode — Settings ▸ Safe Mode)")),
            "Run",
            cx,
        );
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            if answer.await {
                let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                    this.run_sql_in_tab(ix, scope, cx)
                });
            }
        })
        .detach();
    }

    /// SQL result footer "Export…": the rows on screen, any format.
    pub(crate) fn export_result(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.active_tab else { return };
        let Some(WorkspaceTab::Sql(tab)) = self.tabs.get(ix) else {
            return;
        };
        let columns = tab.output.columns.iter().map(|c| c.name.clone()).collect();
        let source = crate::export::Source::Result {
            title: format!("{}-result-{}", tab.title, tab.active_result + 1),
            columns,
            rows: tab.output.rows.clone(),
        };
        crate::export::ExportWindow::open(self.pool.clone(), source, cx);
    }

    /// Show another result set of the last run in the grid.
    fn show_result(&mut self, ix: usize, which: usize, cx: &mut Context<Self>) {
        let Some(WorkspaceTab::Sql(tab)) = self.tabs.get_mut(ix) else {
            return;
        };
        let Some(set) = tab.results.get(which).cloned() else {
            return;
        };
        tab.active_result = which;
        tab.output = set.output.clone();
        tab.result.update(cx, |state, cx| {
            state
                .delegate_mut()
                .set_result(set.output.columns, set.output.rows, set.source);
            state.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn render_sql_tab(&self, tab: &crate::sql::SqlTab, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;
        let err_red = cx.theme().red;

        // Result pane: error | message | grid.
        let result_pane: AnyElement = if let Some(err) = tab.output.error.clone() {
            let sql = tab
                .last_sql
                .clone()
                .unwrap_or_else(|| tab.editor.read(cx).text().to_string());
            let err_c = err.clone();
            div()
                .flex_1()
                .p_3()
                .flex()
                .flex_col()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_family(crate::settings::ui_font())
                        .text_color(err_red)
                        .child(err),
                )
                .child(
                    Button::new("sql-error-ai")
                        .outline()
                        .xsmall()
                        .icon(IconName::Sparkles)
                        .label("Ask AI to Fix")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.send_to_chat("Failed query", sql.clone(), "sql", window, cx);
                            this.send_to_chat("Error", err_c.clone(), "text", window, cx);
                        })),
                )
                .into_any_element()
        } else if tab.output.rows.is_empty() {
            let msg = tab
                .output
                .message
                .clone()
                .unwrap_or_else(|| "No rows".to_string());
            let mut extra = format!("{} ms", tab.output.ms);
            if tab.output.truncated {
                extra.push_str(" · truncated");
            }
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_family(crate::settings::ui_font())
                        .text_color(muted)
                        .child(crate::kbd::rich_colored(&msg, muted)),
                )
                .when(tab.last_sql.is_some() && !tab.running, |d| {
                    d.child(
                        div()
                            .text_caption()
                            .font_family(crate::settings::ui_font())
                            .text_color(muted)
                            .child(extra),
                    )
                })
                .into_any_element()
        } else {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .child(crate::sql::result_element(&tab.result)),
                )
                .child(
                    div()
                        .h(px(crate::settings::bar_h()))
                        .flex()
                        .items_center()
                        .px_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(border)
                        .when(tab.results.len() > 1, |bar| {
                            // one tab per SELECT of a script.
                            let tabs = tab.results.iter().enumerate().map(|(i, _)| {
                                let active = i == tab.active_result;
                                div()
                                    .id(SharedString::from(format!("result-tab-{i}")))
                                    .px_2()
                                    .h(px(crate::settings::row_h() - 4.))
                                    .flex()
                                    .items_center()
                                    .rounded(crate::theme::RADIUS_SM)
                                    .text_caption()
                                    .font_family(crate::settings::ui_font())
                                    .text_color(if active { foreground } else { muted })
                                    .when(active, |t| t.bg(muted.opacity(0.18)))
                                    .hover(|t| t.bg(muted.opacity(0.1)))
                                    .child(format!("Result {}", i + 1))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(ix) = this.active_tab {
                                            this.show_result(ix, i, cx);
                                        }
                                    }))
                            });
                            bar.child(div().flex().items_center().gap_0p5().children(tabs))
                                .child(div().w(px(1.)).h(px(14.)).bg(border))
                        })
                        .child(
                            div()
                                .text_caption()
                                .font_family(crate::settings::ui_font())
                                .text_color(foreground)
                                .child({
                                    let n = tab.output.rows.len();
                                    let mut s =
                                        format!("{} · {} ms", crate::sql::n_rows(n), tab.output.ms);
                                    if tab.output.truncated {
                                        s.push_str(&format!(
                                            " · truncated at {}",
                                            crate::sql::QUERY_ROW_LIMIT
                                        ));
                                    }
                                    s
                                }),
                        )
                        .child(div().flex_1())
                        .child({
                            // `✎ public.events` (editable) or the read-only
                            // reason, dim; details in the tooltip.
                            let src = tab.result.read(cx).delegate().source.clone();
                            let (icon, text, tip) = match (&src, &tab.output.edit_note) {
                                (Some(src), _) => (
                                    Some(IconName::Pencil),
                                    format!("{}.{}", src.schema, src.table),
                                    format!(
                                        "Editable — double-click a cell, {}-s to save",
                                        if cfg!(target_os = "macos") {
                                            "cmd"
                                        } else {
                                            "ctrl"
                                        }
                                    ),
                                ),
                                (None, Some(note)) => {
                                    (Some(IconName::Lock), "read-only".into(), note.clone())
                                }
                                (None, None) => (None, String::new(), String::new()),
                            };
                            div()
                                .id("result-edit-note")
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_1()
                                .text_caption()
                                .font_family(crate::settings::ui_font())
                                .text_color(muted.opacity(0.7))
                                .children(icon.map(|i| Icon::new(i).size(px(11.))))
                                .child(text)
                                .when(!tip.is_empty(), |this| {
                                    this.tooltip(move |w, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                                            .build(w, cx)
                                    })
                                })
                        })
                        .child(div().w(px(1.)).h(px(14.)).mx_1().bg(border))
                        .child(
                            div()
                                .id("result-export")
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .h(px(crate::settings::row_h()))
                                .px_2()
                                .rounded(crate::theme::RADIUS_MD)
                                .border_1()
                                .border_color(border)
                                .text_caption()
                                .text_color(muted)
                                .hover(|this| this.bg(muted.opacity(0.1)).text_color(foreground))
                                .child(Icon::new(IconName::Download).size(px(12.)))
                                .child("Export")
                                .on_click(cx.listener(|this, _, _, cx| this.export_result(cx))),
                        ),
                )
                .into_any_element()
        };

        // SIMPLIFIED (BUG-11): plain fixed split instead of v_resizable.
        // The resizable panels ignored size(220)+flex_none (editor panel came
        // out ~390px and pushed results + status bar off-screen), and splitter
        // drags behaved erratically. Fixed 220px editor until the framework
        // interaction is understood; drag-resize is a follow-up.
        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(tab.view_draft.clone(), |this, input| {
                this.child(Self::draft_name_bar(
                    "View name",
                    &input,
                    "[cmd-s] creates the view",
                    cx,
                ))
            })
            .child(
                div()
                    .map(|d| if self.results_hidden { d.flex_1() } else { d.h(px(tab.editor_h)) })
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                // No own border / radius: the pane's single
                                // divider below separates it from the run bar.
                                gpui_kit::component::input::Editor::new(&tab.editor)
                                    .bordered(false)
                                    .h_full(),
                            ),
                    )
                    // Drag handle: resize the editor / results split.
                    .child(
                        div()
                            .id("sql-split")
                            .h(px(5.))
                            .w_full()
                            .flex_none()
                            .cursor_row_resize()
                            .hover(|this| this.bg(cx.theme().accent.opacity(0.35)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                    if let Some(WorkspaceTab::Sql(t)) =
                                        this.active_tab.and_then(|ix| this.tabs.get(ix))
                                    {
                                        this.split_drag =
                                            Some((f32::from(e.position.y), t.editor_h));
                                        cx.stop_propagation();
                                    }
                                }),
                            ),
                    )
                    .child(
                        div()
                            .h(px(crate::settings::bar_h() + 10.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_3()
                            .border_t_1()
                            .border_color(border)
                            .child(
                                div()
                                    .text_caption()
                                    .font_family(crate::settings::ui_font())
                                    .text_color(muted)
                                    .child(crate::kbd::rich_colored("[cmd-enter] run current (or selection) · [cmd-shift-enter] run all", muted)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("run-sql-all")
                                            .label("Run All")
                                            .small()
                                            .outline()
                                            .disabled(tab.running)
                                            .on_click(cx.listener(|this, _, w, cx| {
                                                this.run_action(crate::sql::RunScope::All, w, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("run-sql")
                                            .label("Run Current")
                                            .small()
                                            // Quiet outlined button, not a
                                            // white filled one.
                                            .outline()
                                            .disabled(tab.running)
                                            .on_click(cx.listener(|this, _, w, cx| {
                                                this.run_action(
                                                    crate::sql::RunScope::Current,
                                                    w,
                                                    cx,
                                                )
                                            })),
                                    ),
                            ),
                    ),
            )
            .when(!self.results_hidden, |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .child(result_pane),
                )
            })
    }

    // ---------- palette + actions ----------

    pub fn open_palette(&mut self, mode: PaletteMode, window: &mut Window, cx: &mut Context<Self>) {
        // Runs must parallel the rendered command items — build and store together.
        if mode == PaletteMode::Commands {
            let (_, runs) = crate::palette::command_rows(self);
            self.palette_runs = runs;
        } else {
            self.palette_runs.clear();
        }
        let overlay = PaletteOverlay::open(mode, window, cx);
        self.palette = Some(overlay);
        cx.notify();
    }

    pub fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_runs.clear();
        // Hand focus back to the app so keys keep working.
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// State the command palette builds its dynamic rows from.
    pub fn palette_context(&self) -> crate::palette::PaletteContext {
        crate::palette::PaletteContext {
            connected: self.screen == AppScreen::Workspace,
            saved: self.form.saved.iter().map(|c| c.name.clone()).collect(),
            active_connection: self.active_name.clone(),
            databases: self.databases.clone(),
            current_database: self.current_database(),
            schemas: self.schemas.clone(),
            current_schema: self.current_schema.clone(),
            focus_object: self
                .active_tab
                .and_then(|ix| match self.tabs.get(ix) {
                    Some(WorkspaceTab::Grid(g)) => {
                        Some((g.table.kind.clone(), g.table.name.clone()))
                    }
                    _ => None,
                })
                .or_else(|| self.selected_object.clone()),
            sql_tab_active: matches!(
                self.active_tab.and_then(|ix| self.tabs.get(ix)),
                Some(WorkspaceTab::Sql(_))
            ),
        }
    }

    pub fn palette_switch_connection(&mut self, ix: usize, w: &mut Window, cx: &mut Context<Self>) {
        if self.screen == AppScreen::Workspace {
            self.switch_connection(ix, w, cx);
        } else {
            self.on_pick_saved(ix, true, w, cx);
        }
    }

    /// Confirm handler for the overlay (called via the TuskHandle global).
    pub fn palette_confirm(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Picking closes the palette: hand focus back to the app first, or
        // the dropped palette input takes it along and no shortcut matches.
        self.focus.focus(window, cx);
        let mode = self.palette.as_ref().map(|p| p.mode);
        match mode {
            Some(PaletteMode::Commands) => {
                if let Some(run) = self.palette_runs.get(row) {
                    let run = run.clone();
                    self.palette = None;
                    self.palette_runs.clear();
                    run(self, window, cx);
                }
            }
            Some(PaletteMode::Tabs) => {
                self.palette = None;
                self.activate_tab(row, cx);
            }
            Some(PaletteMode::Databases) => {
                let name = self
                    .palette
                    .as_ref()
                    .and_then(|p| p.names.get(row))
                    .cloned();
                self.palette = None;
                if let Some(name) = name {
                    self.switch_database(name, window, cx);
                }
            }
            Some(PaletteMode::Connections) => {
                let name = self
                    .palette
                    .as_ref()
                    .and_then(|p| p.names.get(row))
                    .cloned();
                self.palette = None;
                if let Some(ix) =
                    name.and_then(|n| self.form.saved.iter().position(|c| c.name == n))
                {
                    self.switch_connection(ix, window, cx);
                }
            }
            Some(PaletteMode::Tables) => {
                if let Some(table) = self
                    .palette
                    .as_ref()
                    .and_then(|p| p.table_rows.get(row))
                    .cloned()
                {
                    self.palette = None;
                    self.open_table_ref(table, window, cx);
                }
            }
            None => {}
        }
        cx.notify();
    }

    // Palette run functions (must match `RunFn`).
    pub fn run_new_connection(_this: &mut TuskApp, _w: &mut Window, cx: &mut Context<TuskApp>) {
        crate::dialog::ConnDialog::open(cx);
    }
    /// A toast in the bottom-right corner (shown on the next frame).
    pub(crate) fn toast(&mut self, ok: bool, msg: impl Into<String>) {
        self.toasts.push((Some(ok), msg.into()));
    }
    /// Neutral toast ("Nothing to undo.").
    pub(crate) fn toast_info(&mut self, msg: impl Into<String>) {
        self.toasts.push((None, msg.into()));
    }

    pub fn run_quick_connect(this: &mut TuskApp, w: &mut Window, cx: &mut Context<TuskApp>) {
        if let Some(conn) = this.form.saved.first().cloned() {
            this.form.selected = Some(0);
            this.form.load(&conn, w, cx);
            this.connect_now(w, cx);
        } else {
            this.form.notice = Some((false, "No saved connections.".to_string()));
            cx.notify();
        }
    }
    /// The window's close button: from a workspace go back to the welcome
    /// screen (returns false = keep the window); on the welcome screen, close.
    pub fn on_close_request(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.screen == AppScreen::Workspace {
            // Asks first when unsaved changes / queries would be lost.
            self.disconnect_guarded(window, cx);
            self.focus.focus(window, cx);
            false
        } else {
            true
        }
    }

    /// A failed connect that a password would fix: a server engine and no
    /// saved password, or the server rejected it.
    fn wants_password(conn: &SavedConnection, password: &str, err: &str) -> bool {
        let lower = err.to_lowercase();
        conn.engine.form() == crate::engine::Form::Server
            && (password.is_empty()
                || lower.contains("password")
                || lower.contains("authentication"))
    }

    /// Forget the workspace of the current connection: tabs, split panes,
    /// history, sidebar selection and pending sidebar edits.
    /// Settings ▸ SQL Editor changed: give every open editor the new values.
    fn sync_editor_prefs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let p = crate::settings::get();
        let now = (p.editor_line_numbers, p.editor_soft_wrap, p.editor_tab_size);
        if now == self.editor_prefs {
            return;
        }
        self.editor_prefs = now;
        for tab in &self.tabs {
            if let WorkspaceTab::Sql(t) = tab {
                t.editor.update(cx, |e, cx| {
                    e.set_line_number(now.0, window, cx);
                    e.set_soft_wrap(now.1, window, cx);
                    e.set_tab_size(
                        gpui_kit::component::input::TabSize {
                            tab_size: now.2 as usize,
                            hard_tabs: false,
                        },
                        cx,
                    );
                });
            }
        }
    }

    fn reset_workspace(&mut self) {
        self.tabs.clear();
        self.split = None;
        self.split_focus = 0;
        self.nav_back.clear();
        self.nav_fwd.clear();
        self.active_tab = None;
        self.selected_object = None;
        self.pending_drops.clear();
        self.pending_renames.clear();
        self.row_panel.detail = None;
        // The console belongs to the connection it logged.
        crate::console::clear();
    }

    pub fn run_disconnect(this: &mut TuskApp, _w: &mut Window, cx: &mut Context<TuskApp>) {
        this.pool = None;
        this.tunnel = None;
        this.lsp = None;
        this.completer = None;
        this.active_conn = None;
        db::forget_last_connection();
        this.databases.clear();
        // Drop open tabs too, or the status bar keeps showing the last tab's
        // row count on the welcome screen.
        this.reset_workspace();
        this.active_name.clear();
        this.server_label = None;
        this.screen = AppScreen::Connection;
        this.status_line = "Not connected".to_string();
        // The "Connecting to …" / error notice belongs to the last attempt;
        // back on the welcome screen it would read as still in progress.
        this.form.notice = None;
        this.form.busy = false;
        this.conn_manager = false;
        cx.notify();
    }

    /// Called by the connection dialog on success: install the pool and enter.
    /// The profile comes from `Connected`: `connect` fills a blank database
    /// with the engine default (or the picked D1 one), and the crumb,
    /// "last used" and the recent list must read the filled value.
    pub fn connected_with(
        &mut self,
        connected: db::Connected,
        password: &str,
        cx: &mut Context<Self>,
    ) {
        let conn = connected.conn.clone();
        let name = conn.name.clone();
        self.clear_conn_search = true;
        self.active_conn_folded = false;
        // Another connection (or database): the old tabs, split, selection
        // and pending edits belong to the old one.
        let same = self
            .active_conn
            .as_ref()
            .is_some_and(|(c, _)| c.name == conn.name && c.database == conn.database);
        if !same {
            self.reset_workspace();
        }
        self.conn_manager = false;
        let db::Connected {
            pool,
            tunnel,
            host: reach_host,
            port: reach_port,
            ..
        } = connected;
        self.tunnel = tunnel;
        db::set_engine(pool.engine());
        db::remember_last_connection(&conn);
        self.touch_last_used(&conn.name);
        let started = self.start_language_server(&conn, &reach_host, reach_port, password, cx);
        self.completer = (!started).then(|| {
            let schema = pool.driver().default_schema().unwrap_or_default();
            crate::complete::SchemaCompletion::new(pool.clone(), schema)
        });
        self.reach = Some((reach_host.clone(), reach_port));
        self.active_conn = Some((conn.clone(), password.to_string()));
        // A database just opened under this profile: feed the dropdown's
        // "recent" shortcut list.
        if !conn.database.trim().is_empty() {
            crate::settings::push_recent_database(cx, &conn.name, &conn.database);
        }
        let list_pool = pool.clone();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let dbs = db::fetch_databases(&list_pool).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.databases = dbs.unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
        self.pool = Some(pool);
        self.active_name = name.clone();
        // Connections tree: show the new connection by unfolding its group.
        if let Some(folder) = conn.folder.as_deref()
            && crate::settings::group_collapsed(folder)
        {
            crate::settings::toggle_group_collapsed(cx, folder);
        }
        self.server_label = Some(format!(
            "{} · {name} @ {}",
            conn.engine.label(),
            conn.endpoint()
        ));
        self.status_line.clear();
        self.screen = AppScreen::Workspace;
        self.load_sidebar_after_connect(cx);
        cx.notify();
    }

    /// Whether the connected engine has a feature; tells the user when not.
    fn supports(
        &mut self,
        what: &str,
        has: fn(crate::engine::Caps) -> bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pool) = &self.pool else { return true };
        if has(pool.caps()) {
            return true;
        }
        let msg = format!("{what} isn't available for {}.", pool.engine().label());
        self.toast_info(msg);
        cx.notify();
        false
    }

    /// Database select box: reconnect with the same profile to another
    /// database (tabs belong to the old database and are closed).
    pub fn switch_database(&mut self, name: String, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.active_conn.clone() else {
            return;
        };
        if conn.database == name {
            return;
        }
        let mut conn = conn;
        conn.database = name.clone();
        self.status_line = format!("Connecting to {name}…");
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = db::connect(conn.clone(), password.clone(), None).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match result {
                    Ok(c) => {
                        this.reset_workspace();
                        this.connected_with(c, &password, cx);
                    }
                    Err(e) => {
                        this.status_line.clear();
                        this.toast(false, format!("Can't open {name}: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Launch: reopen the last connection straight away (password from the
    /// Keychain, read off the UI thread). Falls back to the welcome screen.
    pub fn auto_connect(&mut self, cx: &mut Context<Self>) {
        if !crate::settings::get().reopen_last {
            return;
        }
        let Some(conn) = db::last_connection() else {
            return;
        };
        self.form.notice = Some((true, format!("Reconnecting to {}…", conn.name)));
        self.form.busy = true;
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let name = conn.name.clone();
            let password = cx
                .background_executor()
                .spawn(async move { db::load_password(&name) })
                .await;
            let pw = password.unwrap_or_default();
            let result = db::connect(conn.clone(), pw.clone(), None)
                .await
                .map(|c| (c, pw));
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.form.busy = false;
                match result {
                    Ok((c, pw)) => {
                        this.form.notice = None;
                        this.connected_with(c, &pw, cx);
                    }
                    Err(e) => {
                        // Render turns the error notice into a toast.
                        this.form.notice = Some((false, format!("{}: {e}", conn.name)));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stamp a saved profile as just used ("Recent" is ordered by this).
    fn touch_last_used(&mut self, name: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let mut list = db::load_connections();
        if let Some(c) = list.iter_mut().find(|c| c.name == name) {
            c.last_used = Some(now);
            let _ = db::save_connections(&list);
        }
        self.form.saved = list;
    }

    /// Re-read `connections.json` (after the dialog saved one).
    /// "Migrate from TablePlus…": open the import sheet (review → import).
    pub fn migrate_from_tableplus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let existing: Vec<String> = self.form.saved.iter().map(|c| c.name.clone()).collect();
        match crate::migrate::plan(&existing) {
            Ok(plan) => {
                let picked = vec![true; plan.connections.len()];
                self.migrate = Some(migrate_sheet::MigrateSheet {
                    plan,
                    picked,
                    phase: migrate_sheet::Phase::Review,
                });
            }
            Err(e) => self.form.notice = Some((false, format!("TablePlus import: {e}"))),
        }
        cx.notify();
    }

    /// "Import from Docker Compose…": pick a compose file (or its folder),
    /// then review its database services in the import sheet.
    pub fn import_docker_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let existing: Vec<(String, Option<String>)> = this
                    .form
                    .saved
                    .iter()
                    .map(|c| (c.name.clone(), c.folder.clone()))
                    .collect();
                match crate::compose::find_file(&path)
                    .and_then(|f| crate::compose::plan(&f, &existing))
                {
                    Ok(plan) if plan.connections.is_empty() => {
                        let why = if plan.skipped.is_empty() {
                            "no database services Tusk supports".to_string()
                        } else {
                            format!("nothing to import — {}", plan.skipped.join(", "))
                        };
                        this.toast_info(format!("Docker Compose: {why}"));
                    }
                    Ok(plan) => {
                        let picked = vec![true; plan.connections.len()];
                        this.migrate = Some(migrate_sheet::MigrateSheet {
                            plan,
                            picked,
                            phase: migrate_sheet::Phase::Review,
                        });
                    }
                    Err(e) => this.toast(false, format!("Docker Compose: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn reload_saved_connections(&mut self, cx: &mut Context<Self>) {
        self.form.saved = db::load_connections();
        cx.notify();
    }

    /// Start the connection's SQL language server (postgres-language-server
    /// for Postgres, sqls for the other SQL engines it knows) and route its
    /// diagnostics into the matching SQL tab's editor (kit `DiagnosticSet`).
    /// `host`/`port`: where the server is reachable from here (the SSH
    /// tunnel's local end for SSH profiles). False when there's none to run.
    fn start_language_server(
        &mut self,
        conn: &SavedConnection,
        host: &str,
        port: u16,
        password: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        self.lsp = None;
        let spec = if conn.engine.caps().lsp {
            crate::lsp::ServerSpec::pgls(&crate::lsp::DbSettings {
                host: host.to_string(),
                port,
                username: conn.user.clone(),
                password: password.to_string(),
                database: conn.database.clone(),
            })
        } else {
            match crate::lsp::sqls_connection(conn, host, port, password) {
                Some(c) => crate::lsp::ServerSpec::sqls(c),
                None => return false,
            }
        };
        match crate::lsp::LspClient::start(spec) {
            Ok((client, mut diagnostics)) => {
                self.lsp = Some(client);
                cx.spawn(async move |weak, cx: &mut AsyncApp| {
                    while let Some((uri, diags)) = diagnostics.recv().await {
                        let alive = weak.update(cx, |this: &mut TuskApp, cx| {
                            this.apply_diagnostics(&uri, diags, cx)
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                })
                .detach();
                true
            }
            Err(e) => {
                log::warn!("SQL language server unavailable: {e:#}");
                false
            }
        }
    }

    fn apply_diagnostics(
        &mut self,
        uri: &str,
        diags: Vec<lsp_types::Diagnostic>,
        cx: &mut Context<Self>,
    ) {
        // Unchanged diagnostics (common while typing): nothing to redraw.
        let old = self.problems.iter().find(|(u, _)| u == uri).map(|(_, d)| d);
        if old.map_or(diags.is_empty(), |d| *d == diags) {
            return;
        }
        self.problems.retain(|(u, _)| u != uri);
        if !diags.is_empty() {
            self.problems.push((uri.to_string(), diags.clone()));
        }
        cx.notify();
        for tab in &self.tabs {
            if let WorkspaceTab::Sql(t) = tab
                && t.doc.as_ref().is_some_and(|d| d.uri == uri)
            {
                t.editor.update(cx, |st, cx| {
                    if let Some(set) = st.diagnostics_mut() {
                        set.clear();
                        set.extend(diags.clone());
                    }
                    cx.notify();
                });
            }
        }
    }

    // Action handlers (wired via `.on_action` on the app root).
    fn on_toggle_palette(&mut self, _: &TogglePalette, w: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(w, cx);
        } else {
            self.open_palette(PaletteMode::Commands, w, cx);
        }
    }
    /// ⌘K / ⇧⌘K: pick a database / a saved connection to open.
    pub(super) fn open_name_picker(
        &mut self,
        mode: PaletteMode,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.as_ref().is_some_and(|p| p.mode == mode) {
            self.close_palette(w, cx);
            return;
        }
        let names = match mode {
            PaletteMode::Databases => self.databases.clone(),
            PaletteMode::Tabs => self.tabs.iter().map(|t| t.label()).collect(),
            _ => self.form.saved.iter().map(|c| c.name.clone()).collect(),
        };
        self.open_palette(mode, w, cx);
        if let Some(p) = self.palette.as_mut() {
            p.names = names;
            *p.items.borrow_mut() = None;
        }
        cx.notify();
    }

    fn on_quick_open(&mut self, _: &QuickOpenTables, w: &mut Window, cx: &mut Context<Self>) {
        if self
            .palette
            .as_ref()
            .is_some_and(|p| p.mode == PaletteMode::Tables)
        {
            self.close_palette(w, cx);
            return;
        }
        self.open_palette(PaletteMode::Tables, w, cx);
        if let Some(p) = self.palette.as_mut() {
            p.table_rows = self.tables.clone();
            *p.items.borrow_mut() = None;
        }
        cx.notify();
    }
    fn on_close_overlay(&mut self, _: &CloseOverlay, w: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(w, cx);
        } else if self.preview.is_some() {
            self.preview = None;
            cx.notify();
        } else if self.processes.is_some() {
            self.processes = None;
            cx.notify();
        } else if self.db_search.is_some() {
            self.db_search = None;
            cx.notify();
        } else if self.users.is_some() {
            self.users = None;
            cx.notify();
        } else if self.conn_manager {
            self.conn_manager = false;
            cx.notify();
        } else if self.migrate.as_ref().is_some_and(|m| !m.importing()) {
            self.migrate = None;
            cx.notify();
        } else if self.filter.read(cx).focus_handle(cx).is_focused(w) {
            self.close_sidebar_filter(w, cx);
        } else if self.cancel_editors(cx) {
            // The editor held focus; without this ⌘S etc. go nowhere.
            self.focus_active_table(w, cx);
        }
    }
    fn on_new_connection(&mut self, _: &NewConnection, w: &mut Window, cx: &mut Context<Self>) {
        Self::run_new_connection(self, w, cx);
    }
    fn on_quick_connect(&mut self, _: &QuickConnect, w: &mut Window, cx: &mut Context<Self>) {
        Self::run_quick_connect(self, w, cx);
    }
    fn on_test_action(&mut self, _: &TestConnection, w: &mut Window, cx: &mut Context<Self>) {
        let ev = ClickEvent::default();
        self.on_test(&ev, w, cx);
    }
    fn on_save_action(&mut self, _: &SaveConnection, w: &mut Window, cx: &mut Context<Self>) {
        let ev = ClickEvent::default();
        self.on_save(&ev, w, cx);
    }
    fn on_connect_action(&mut self, _: &ConnectNow, w: &mut Window, cx: &mut Context<Self>) {
        self.connect_now(w, cx);
    }
    fn on_recent(&mut self, ix: usize, w: &mut Window, cx: &mut Context<Self>) {
        // In the workspace ⌘1–⌘5 pick tabs, as ⌘6–⌘9 do.
        if self.screen == AppScreen::Workspace {
            self.tab_at(ix, cx);
            return;
        }
        // ⌘N = the N-th *recent* profile, matching the welcome list.
        if let Some(&real) = self.recent_indexes().get(ix)
            && let Some(conn) = self.form.saved.get(real).cloned()
        {
            self.form.selected = Some(real);
            self.form.load(&conn, w, cx);
            self.connect_now(w, cx);
        }
    }

    // ---------- welcome screen ----------

    /// Section header: small caps muted mono label + hairline.
    /// Welcome section header: `GET STARTED ────` flush with the
    /// column edge, faint rule to the right.
    fn section_header(title: &str, muted: gpui::Hsla) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(20.))
            .child(
                div()
                    .flex_none()
                    .text_size(zrem(10.))
                    .font_family(crate::settings::table_font())
                    .text_color(muted)
                    .child(title.to_uppercase()),
            )
            .child(div().flex_1().h(px(1.)).bg(muted.opacity(0.12)))
    }

    /// Welcome row: 24px, icon at +5, label at +22, `cmd-n` hint right.
    fn welcome_row(
        &self,
        id: impl Into<ElementId>,
        icon: impl Into<Icon>,
        label: &str,
        hint: &str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut TuskApp, &ClickEvent, &mut Window, &mut Context<TuskApp>) + 'static,
    ) -> Stateful<Div> {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        Self::welcome_line(
            id,
            icon,
            label.to_string(),
            None,
            Self::kbd_hint(hint),
            muted,
            muted,
            foreground,
        )
        .on_click(
            cx.listener(move |this, ev: &ClickEvent, window, cx| on_click(this, ev, window, cx)),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn welcome_line(
        id: impl Into<ElementId>,
        icon: impl Into<Icon>,
        label: String,
        detail: Option<String>,
        trailing: AnyElement,
        icon_color: gpui::Hsla,
        muted: gpui::Hsla,
        foreground: gpui::Hsla,
    ) -> Stateful<Div> {
        div()
            .id(id.into())
            .flex()
            .items_center()
            .w_full()
            .h(px(24.))
            .mt(px(1.))
            .pl(px(5.))
            .pr(px(6.))
            .rounded(crate::theme::RADIUS_SM)
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(
                div()
                    .w(px(17.))
                    .flex_none()
                    .child(icon.into().size(zrem(12.)).text_color(icon_color)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(zrem(14.))
                    .font_family(crate::settings::ui_font())
                    .text_color(foreground)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_2()
                    .truncate()
                    .text_size(zrem(12.))
                    .font_family(crate::settings::ui_font())
                    .text_color(muted.opacity(0.55))
                    .children(detail),
            )
            .child(div().flex_none().flex().items_center().child(trailing))
    }

    /// Shortcut hint as key caps (`cmd-shift-o` → ⌘⇧O) in the system font,
    /// which has the modifier glyphs whatever the UI font is.
    fn kbd_hint(hint: &str) -> AnyElement {
        crate::kbd::caps_sized(hint, zrem(12.))
    }

    /// Recent connection: small database icon,
    /// name, `host · database` dimmed, `cmd-N`.
    fn recent_row(&self, n: usize, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(conn) = self.form.saved.get(ix) else {
            return div().into_any_element();
        };
        let (muted, fg) = (cx.theme().muted_foreground, cx.theme().foreground);
        use crate::engine::Form;
        let where_ = match conn.engine.form() {
            Form::Server if conn.database.is_empty() => conn.host.clone(),
            Form::Server => format!("{} · {}", conn.host, conn.database),
            _ => conn.endpoint(),
        };
        let mut detail = format!("{} · {where_}", conn.engine.label());
        if conn.ssh.is_some() {
            detail.push_str(" · ssh");
        }
        let tint = conn.status_rgb().map(|c| rgb(c).into());
        // The engine's own logo, drawn like the other line icons.
        let icon = match crate::icons::engine_logo(conn.engine) {
            Some(bytes) => Icon::default().data(bytes),
            None => Icon::new(IconName::Database),
        };
        Self::welcome_line(
            ("recent-conn", ix),
            icon,
            conn.name.clone(),
            Some(detail),
            if self.form.busy && self.form.selected == Some(ix) {
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_caption()
                    .text_color(muted)
                    .child(
                        gpui_kit::component::spinner::Spinner::new()
                            .xsmall()
                            .color(muted),
                    )
                    .child("Connecting…")
                    .into_any_element()
            } else {
                Self::kbd_hint(&format!("cmd-{}", n + 1))
            },
            tint.unwrap_or(muted),
            muted,
            fg,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.on_pick_saved(ix, true, window, cx);
        }))
        .into_any_element()
    }

    fn render_connection(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;

        let mut col = div()
            .flex()
            .flex_col()
            .w(px(386.))
            // logo + two-line title, centered over the column.
            .child(
                div()
                    .flex()
                    .justify_center()
                    .items_center()
                    .gap(px(14.))
                    .pb(px(25.))
                    .child(DbIcon::Postgres.icon_px(48.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(0.))
                            .child(
                                div()
                                    .text_size(zrem(20.))
                                    .line_height(zrem(26.))
                                    .font_family(crate::settings::ui_font())
                                    .text_color(foreground)
                                    .child("Welcome back to Tusk"),
                            )
                            .child(
                                div()
                                    .text_size(zrem(12.))
                                    .line_height(zrem(18.))
                                    .italic()
                                    .font_family(crate::settings::ui_font())
                                    .text_color(muted)
                                    .child("The gentle giant for your databases"),
                            ),
                    ),
            )
            .child(
                div()
                    .pb(px(4.))
                    .child(Self::section_header("Get Started", muted)),
            )
            .child(self.welcome_row(
                "welcome-new",
                IconName::Plus,
                "New Connection…",
                "cmd-n",
                cx,
                |this, _, w, cx| Self::run_new_connection(this, w, cx),
            ))
            .child(self.welcome_row(
                "welcome-open",
                IconName::FolderOpen,
                "Open Connection…",
                "cmd-shift-o",
                cx,
                |this, _, _, cx| this.toggle_conn_manager(cx),
            ))
            .child(self.welcome_row(
                "welcome-backup",
                IconName::HardDriveDownload,
                "Backup Database…",
                "",
                cx,
                |_, _, _, cx| {
                    crate::backup::BackupWindow::open(crate::backup::Mode::Backup, None, None, cx)
                },
            ))
            .child(self.welcome_row(
                "welcome-restore",
                IconName::HardDriveUpload,
                "Restore Database…",
                "",
                cx,
                |_, _, _, cx| {
                    crate::backup::BackupWindow::open(crate::backup::Mode::Restore, None, None, cx)
                },
            ));
        // Recent: the five last-used profiles (⌘1–⌘5 follow this order).
        let recent = self.recent_indexes();
        if !recent.is_empty() {
            col = col.child(
                div()
                    .pt(px(18.))
                    .pb(px(4.))
                    .child(Self::section_header("Recent Connections", muted)),
            );
            for (n, ix) in recent.into_iter().take(5).enumerate() {
                col = col.child(self.recent_row(n, ix, cx));
            }
        }

        col = col.child(
            div()
                .pt(px(18.))
                .pb(px(4.))
                .child(Self::section_header("Import", muted)),
        );
        if crate::migrate::available() {
            col = col.child(self.welcome_row(
                "welcome-migrate",
                IconName::ArrowDownToLine,
                "Migrate from TablePlus…",
                "",
                cx,
                |this, _, w, cx| this.migrate_from_tableplus(w, cx),
            ));
        }
        col = col.child(self.welcome_row(
            "welcome-compose",
            Icon::default().data(include_bytes!("../assets/icons/ui/docker.svg")),
            "Import from Docker Compose…",
            "",
            cx,
            |this, _, w, cx| this.import_docker_compose(w, cx),
        ));

        // Scrolls once the connection list outgrows the window.
        div()
            .id("welcome-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .min_h_full()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .items_center()
                    .py_8()
                    .child(col),
            )
    }

    fn object_row(&self, kind: TableKind, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let selected = self.selected_object == Some((kind.clone(), name.to_string()));
        let sel_bg = crate::theme::selection(cx.theme());
        let dropping = self.is_pending_drop(&kind, name);
        let renamed = self.pending_rename(&kind, name).cloned();
        let editing = self
            .renaming
            .as_ref()
            .filter(|r| r.kind == kind && r.name == name);
        let icon = kind.icon();
        let name = name.to_string();
        let row_id = format!("obj-row-{}-{}", kind.label(), name);
        let label: AnyElement = match editing {
            // Same text size as the label it replaces, full row width, so the
            // row doesn't shrink while renaming.
            Some(r) => div()
                .flex_1()
                .min_w_0()
                .child(
                    Input::new(&r.input)
                        .small()
                        .h(px(crate::settings::row_h() - 2.))
                        .text_size(px(crate::settings::ui_text()))
                        .font_family(crate::settings::ui_font()),
                )
                .into_any_element(),
            None => div()
                .text_size(px(crate::settings::ui_text()))
                .font_family(crate::settings::ui_font())
                .text_color(if selected || dropping || renamed.is_some() {
                    foreground
                } else {
                    muted
                })
                .when(dropping, |this| this.line_through())
                .child(renamed.clone().unwrap_or_else(|| name.clone()))
                .into_any_element(),
        };
        let (kind_c, name_c) = (kind.clone(), name.clone());
        let (kind_m, name_m) = (kind.clone(), name.clone());
        let (kind_r, name_r) = (kind.clone(), name.clone());
        div()
            .id(SharedString::from(row_id))
            .flex()
            .flex_row()
            .items_center()
            .gap_1p5()
            .px_2()
            .h(px(crate::settings::row_h()))
            .rounded(crate::theme::RADIUS_SM)
            .when(selected, |this| this.bg(sel_bg))
            .when(renamed.is_some() && !dropping, |this| {
                this.bg(rgb(crate::theme::EDITED).opacity(0.25))
            })
            .when(dropping, |this| {
                this.bg(rgb(crate::theme::DELETED).opacity(0.35))
            })
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(icon)
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.sidebar_focus.focus(window, cx);
                this.on_pick_object(kind_c.clone(), name_c.clone(), window, cx);
            }))
            // Right-click selects the row first, so the menu's target (and
            // Truncate / Delete) is the highlighted object.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.selected_object = Some((kind_r.clone(), name_r.clone()));
                    cx.notify();
                }),
            )
            .context_menu(move |menu, window, cx| {
                TuskApp::object_menu(kind_m.clone(), name_m.clone(), menu, window, cx)
            })
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let background = cx.theme().background;
        let filter_text = self.filter.read(cx).value().to_string();
        let filter_lower = filter_text.to_lowercase();

        // The active panel's objects (views panel also lists matviews).
        let mut items: Vec<(TableKind, &String)> = Vec::new();
        match self.sidebar_panel {
            SidebarPanel::Tables => {
                items.extend(self.objects.tables.iter().map(|n| (TableKind::Table, n)))
            }
            SidebarPanel::Views => {
                items.extend(self.objects.views.iter().map(|n| (TableKind::View, n)));
                items.extend(
                    self.objects
                        .matviews
                        .iter()
                        .map(|n| (TableKind::MaterializedView, n)),
                );
            }
            SidebarPanel::Functions => items.extend(
                self.objects
                    .functions
                    .iter()
                    .map(|n| (TableKind::Function, n)),
            ),
        }
        let matching: Vec<(TableKind, &String)> = items
            .into_iter()
            .filter(|(_, n)| filter_lower.is_empty() || n.to_lowercase().contains(&filter_lower))
            .collect();

        let mut list = div().flex().flex_col().gap_0();
        let empty_msg = if self.objects_loading {
            Some("Loading objects…".to_string())
        } else if matching.is_empty() {
            Some(format!(
                "No {} in this schema.",
                self.sidebar_panel.title().to_lowercase()
            ))
        } else {
            None
        };
        if let Some(msg) = empty_msg {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_caption()
                    .text_color(muted)
                    .child(msg),
            );
        }
        // sidebar folders first, then everything ungrouped. Members are
        // `kind:name` keys: look them up in one index of the visible objects
        // (no per-object formatting / scans for every group each frame).
        let index: std::collections::HashMap<(&str, &str), usize> = matching
            .iter()
            .enumerate()
            .map(|(i, (k, n))| ((k.label(), n.as_str()), i))
            .collect();
        let mut grouped = vec![false; matching.len()];
        for g in &self.obj_groups {
            let mut hits: Vec<usize> = g
                .members
                .iter()
                .filter_map(|m| m.split_once(':'))
                .filter_map(|(k, n)| index.get(&(k, n)).copied())
                .collect();
            hits.sort_unstable();
            for &i in &hits {
                grouped[i] = true;
            }
            let members: Vec<&(TableKind, &String)> = hits.iter().map(|&i| &matching[i]).collect();
            if members.is_empty() && !filter_lower.is_empty() {
                continue;
            }
            list = list.child(self.obj_group_header(g, members.len(), cx));
            if !self.obj_groups_collapsed.contains(&g.name) || !filter_lower.is_empty() {
                for (kind, name) in members {
                    list = list.child(div().pl(px(14.)).child(self.object_row(
                        kind.clone(),
                        name,
                        cx,
                    )));
                }
            }
        }
        for (i, (kind, name)) in matching.iter().enumerate() {
            if !grouped[i] {
                list = list.child(self.object_row(kind.clone(), name, cx));
            }
        }
        // Tables being designed: a green pending row until ⌘S creates them.
        if self.sidebar_panel == SidebarPanel::Tables {
            for (tab_ix, tab) in self.tabs.iter().enumerate() {
                let WorkspaceTab::Grid(g) = tab else { continue };
                let Some(input) = &g.draft else { continue };
                let name = input.read(cx).value().to_string();
                let active = self.active_tab == Some(tab_ix);
                list = list.child(
                    div()
                        .id(("draft-table", tab_ix))
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_2()
                        .h(px(crate::settings::row_h()))
                        .rounded(crate::theme::RADIUS_SM)
                        .bg(rgb(crate::theme::ADDED).opacity(if active { 0.35 } else { 0.25 }))
                        .child(TableKind::Table.icon())
                        .child(
                            div()
                                .text_size(px(crate::settings::ui_text()))
                                .font_family(crate::settings::ui_font())
                                .text_color(cx.theme().foreground)
                                .truncate()
                                .child(if name.is_empty() {
                                    "untitled".to_string()
                                } else {
                                    name
                                }),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(tab_ix, cx))),
                );
            }
        }
        // panel header: `TABLES (8) 🔍`; the search icon (or ⌘F
        // while the sidebar is focused) turns the row into a compact filter
        // box, Esc clears and folds it back. No permanent input on top.
        let show_filter = self.filter_open || !filter_text.is_empty();
        let header =
            if show_filter {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.filter)
                                .xsmall()
                                .prefix(Icon::new(IconName::Search).size(px(12.)).text_color(muted))
                                .font_family(crate::settings::ui_font()),
                        ),
                    )
                    .child(
                        div()
                            .id("sidebar-filter-close")
                            .w(px(20.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(crate::theme::RADIUS_SM)
                            .text_color(muted)
                            .hover(|this| this.bg(muted.opacity(0.12)))
                            .child(Icon::new(IconName::Close).size(px(12.)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_sidebar_filter(window, cx)
                            })),
                    )
            } else {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_2()
                    .pt_2()
                    .pb_1()
                    .child(
                        div()
                            .flex_1()
                            .text_caption()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(muted)
                            .child(format!(
                                "{} ({})",
                                self.sidebar_panel.title().to_uppercase(),
                                matching.len()
                            )),
                    )
                    .child(self.sidebar_new_button(cx))
                    .child(
                        div()
                            .id("sidebar-filter-open")
                            .w(px(20.))
                            .h(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(crate::theme::RADIUS_SM)
                            .text_color(muted)
                            .hover(|this| this.bg(muted.opacity(0.12)))
                            .child(Icon::new(IconName::Search).size(px(13.)))
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Filter")
                                    .key_binding(crate::kbd::tip("cmd-f"))
                                    .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_sidebar_filter(window, cx)
                            })),
                    )
            };

        if crate::settings::connections_tree() {
            return self.render_conn_tree(header.into_any_element(), list.into_any_element(), cx);
        }
        div()
            .id("sidebar")
            .key_context("Sidebar")
            .track_focus(&self.sidebar_focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(background)
            .child(header)
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .flex()
                    .flex_col()
                    .child(list),
            )
            .into_any_element()
    }

    pub(super) fn open_sidebar_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_open = true;
        self.filter.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn close_sidebar_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_open = false;
        self.filter
            .update(cx, |st, cx| st.set_value("", window, cx));
        self.sidebar_focus.focus(window, cx);
        cx.notify();
    }

    /// Title-bar pill: jump to another saved connection (password from the
    /// Keychain; the welcome screen asks if there is none).
    fn switch_connection(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.form.saved.get(ix).cloned() else {
            return;
        };
        if self
            .active_conn
            .as_ref()
            .is_some_and(|(c, _)| c.name == conn.name)
        {
            return;
        }
        // Connect first: the current workspace stays (tabs and all) until
        // the new connection is up. A missing / wrong password opens the
        // connection's form to enter it instead of leaving you disconnected.
        self.conn_manager = false;
        let password = crate::conn::ConnectionForm::password_or_keychain(&conn.name, String::new());
        self.status_line = format!("Connecting to {}…", conn.name);
        cx.notify();
        cx.spawn_in(window, async move |weak, cx| {
            let result = db::connect(conn.clone(), password.clone(), None).await;
            let _ = weak.update_in(cx, |this: &mut TuskApp, window, cx| {
                this.status_line.clear();
                match result {
                    Ok(c) => {
                        this.form.selected = Some(ix);
                        this.form.load(&conn, window, cx);
                        this.connected_with(c, &password, cx);
                    }
                    Err(e) => {
                        log::warn!("switch to {}: {e}", conn.name);
                        if Self::wants_password(&conn, &password, &e) {
                            this.toast(
                                false,
                                format!("{}: enter the password to connect.", conn.name),
                            );
                            crate::dialog::ConnDialog::open_edit(conn.clone(), false, cx);
                        } else {
                            this.toast(false, format!("{}: {e}", conn.name));
                        }
                        this.focus.focus(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn show_panel(&mut self, panel: SidebarPanel, cx: &mut Context<Self>) {
        self.sidebar_panel = panel;
        self.sidebar_open = true;
        cx.notify();
    }

    fn current_database(&self) -> String {
        self.active_conn
            .as_ref()
            .map(|(c, _)| c.database.clone())
            .unwrap_or_default()
    }

    /// Tab bar: ← → history arrows, full-height tabs separated by
    /// hairlines; the active tab takes the content background (reads as
    /// attached), inactive ones sit on a raised grey; × on hover / active.
    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let (muted, foreground, border) = (t.muted_foreground, t.foreground, t.border);
        let (inactive_bg, tab_bar_bg) = (t.tab, t.tab_bar);
        let nav = |id: &'static str, icon: IconName, enabled: bool| {
            div()
                .id(id)
                .w(px(28.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(if enabled {
                    foreground
                } else {
                    muted.opacity(0.35)
                })
                .when(enabled, |this| {
                    this.hover(|this| this.bg(muted.opacity(0.1)))
                })
                .child(Icon::new(icon).size(px(14.)))
        };
        let can_back = !self.nav_back.is_empty();
        let can_fwd = !self.nav_fwd.is_empty();
        let mut strip = div()
            .id("tab-strip")
            .group("tab-strip")
            .h(px(crate::settings::tab_h()))
            .flex()
            .flex_row()
            .items_stretch()
            .bg(tab_bar_bg)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .items_stretch()
                    .px_1()
                    .border_r_1()
                    .border_color(border)
                    .child(
                        nav("tab-back", IconName::ArrowLeft, can_back)
                            .on_click(cx.listener(|this, _, _, cx| this.navigate_tabs(true, cx))),
                    )
                    .child(
                        nav("tab-fwd", IconName::ArrowRight, can_fwd)
                            .on_click(cx.listener(|this, _, _, cx| this.navigate_tabs(false, cx))),
                    ),
            );
        // tab states: active = content background + bright label,
        // inactive = tab background + dim label, modified = yellow label and
        // a ● where × goes (× on hover).
        let modified_c = cx.theme().yellow;
        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = self.active_tab == Some(ix);
            let dirty = self.tab_pending(ix, cx) > 0;
            let (label, icon) = match tab {
                // A table being designed is titled by its name field.
                WorkspaceTab::Grid(g) => (
                    g.draft
                        .as_ref()
                        .map(|i| i.read(cx).value().to_string())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| g.table.name.clone()),
                    g.table.kind.icon(),
                ),
                WorkspaceTab::Sql(_) => (tab.label(), DbIcon::Sql.icon()),
            };
            let group = SharedString::from(format!("tab-{ix}"));
            strip = strip.child(
                div()
                    .id(format!("grid-tab-{ix}"))
                    .group(group.clone())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    // Left = right padding + the × slot, so the label is
                    // centered whether or not × shows.
                    .pl(px(20.))
                    .pr(px(4.))
                    .border_r_1()
                    .border_color(border)
                    // Active = raised (lighter) tab; inactive ones sit flat
                    // on the bar.
                    .bg(if active {
                        inactive_bg.blend(foreground.opacity(0.07))
                    } else {
                        tab_bar_bg
                    })
                    .child(icon)
                    .child(
                        div()
                            .text_size(px(crate::settings::ui_text()))
                            .font_family(crate::settings::ui_font())
                            .text_color(if dirty {
                                if active {
                                    modified_c
                                } else {
                                    modified_c.opacity(0.75)
                                }
                            } else if active {
                                foreground
                            } else {
                                muted
                            })
                            .child(label),
                    )
                    .child(
                        div()
                            .id(format!("close-tab-{ix}"))
                            .relative()
                            .w(px(16.))
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(crate::theme::RADIUS_SM)
                            .text_color(muted)
                            .hover(|this| this.bg(muted.opacity(0.2)).text_color(foreground))
                            // Modified: a dot, swapped for × while hovering the tab.
                            .when(dirty, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .size(px(7.))
                                        .rounded_full()
                                        .bg(modified_c)
                                        .group_hover(group.clone(), |d| d.opacity(0.)),
                                )
                            })
                            .child(
                                div()
                                    .opacity(0.)
                                    .group_hover(group.clone(), |d| d.opacity(1.))
                                    .child(Icon::new(IconName::Close).size(px(11.))),
                            )
                            .on_click(cx.listener(move |this, _, w, cx| {
                                cx.stop_propagation();
                                // Asks first when the tab has unsaved changes;
                                // refocuses the app (the closed tab may have
                                // held focus, and then no shortcut fires).
                                this.close_tab_guarded(ix, w, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(ix, cx))),
            );
        }
        // Hovering the tab bar reveals a "+" that opens a new SQL editor tab.
        strip.child(
            div().flex().items_center().pl_1().child(
                div()
                    .id("tab-strip-new")
                    .w(px(22.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(crate::theme::RADIUS_SM)
                    .text_color(muted)
                    .opacity(0.)
                    .group_hover("tab-strip", |this| this.opacity(1.))
                    .hover(|this| this.bg(muted.opacity(0.15)).text_color(foreground))
                    .child(Icon::new(IconName::Plus).size(px(14.)))
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("New SQL Query")
                            .key_binding(crate::kbd::tip("cmd-t"))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.open_sql_tab(window, cx))),
            ),
        )
    }

    /// Activate a tab, recording the previous one for ←.
    fn activate_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.active_tab == Some(ix) || ix >= self.tabs.len() {
            return;
        }
        // Split: a tab already in the other pane focuses it; any other tab
        // goes into the focused pane.
        if let Some((l, r)) = self.split {
            if ix == l || ix == r {
                self.split_focus = usize::from(ix == r);
            } else if self.split_focus == 0 {
                self.split = Some((ix, r));
            } else {
                self.split = Some((l, ix));
            }
        }
        if let Some(prev) = self.active_tab {
            self.nav_back.push(prev);
        }
        self.nav_fwd.clear();
        self.active_tab = Some(ix);
        cx.notify();
    }

    /// ← / →: walk the tab activation history.
    fn navigate_tabs(&mut self, back: bool, cx: &mut Context<Self>) {
        let (from, to) = if back {
            (&mut self.nav_back, &mut self.nav_fwd)
        } else {
            (&mut self.nav_fwd, &mut self.nav_back)
        };
        let Some(target) = from.pop() else { return };
        if let Some(cur) = self.active_tab {
            to.push(cur);
        }
        self.active_tab = Some(target);
        cx.notify();
    }

    /// The tab content area: one pane, or two side by side (⇧⌘D).
    fn render_grid_area(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((left, right)) = self.split else {
            return self.render_tab_area(self.active_tab, cx).into_any_element();
        };
        let accent = cx.theme().accent;
        let border = cx.theme().border;
        let (fg, muted) = (cx.theme().foreground, cx.theme().muted_foreground);
        let pane = |p: usize, tab: usize, this: &Self, cx: &mut Context<Self>| {
            let focused = this.split_focus == p;
            // Each pane names its tab (the tab bar alone didn't say which
            // tab sits in which pane); the focused one is underlined in accent.
            let title = match this.tabs.get(tab) {
                Some(WorkspaceTab::Grid(g)) => g.table.name.clone(),
                Some(WorkspaceTab::Sql(t)) => t.title.to_string(),
                None => String::new(),
            };
            div()
                .id(("split-pane", p))
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .capture_any_mouse_down(cx.listener(move |this, _, _, cx| this.focus_pane(p, cx)))
                .child(
                    div()
                        .flex_none()
                        .h(px(22.))
                        .px_2()
                        .flex()
                        .items_center()
                        .border_b_2()
                        .border_color(if focused { accent.opacity(0.8) } else { border })
                        .text_caption()
                        .font_family(crate::settings::ui_font())
                        .text_color(if focused { fg } else { muted })
                        .truncate()
                        .child(title),
                )
                .child(this.render_tab_area(Some(tab), cx))
        };
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(pane(0, left, self, cx))
            .child(div().w_px().h_full().flex_none().bg(border))
            .child(pane(1, right, self, cx))
            .into_any_element()
    }

    fn render_tab_area(&self, tab_ix: Option<usize>, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        match tab_ix.and_then(|ix| self.tabs.get(ix).map(|t| (ix, t))) {
            Some((ix, WorkspaceTab::Grid(g))) => {
                let body = match (g.view, &g.structure) {
                    (TabView::Structure, Some(st)) => structure::element(st).into_any_element(),
                    (TabView::Index, _) => match &g.indexes {
                        Some(st) => crate::indexes::element(st).into_any_element(),
                        None => div().into_any_element(),
                    },
                    (TabView::Triggers, _) => match &g.triggers {
                        Some(st) => crate::sql::result_element(st).into_any_element(),
                        None => div().into_any_element(),
                    },
                    (TabView::Ddl, _) => match &g.ddl {
                        Some(e) => gpui_kit::component::input::Editor::new(e)
                            .bordered(false)
                            .h_full()
                            .into_any_element(),
                        None => div().into_any_element(),
                    },
                    _ => match g.state.read(cx).delegate().error.clone() {
                        // Never an empty grid on failure: show the real error.
                        Some(err) if g.state.read(cx).delegate().columns.is_empty() => div()
                            .size_full()
                            .p_3()
                            .text_sm()
                            .font_family(crate::settings::ui_font())
                            .text_color(cx.theme().red)
                            .child(err)
                            .into_any_element(),
                        _ => grid::grid_element(&g.state).into_any_element(),
                    },
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // The toolbar (view switcher, + Row, count, filters, paging)
                    // sits above the table, below the tab strip — the table
                    // grows to fill what's left.
                    .child(self.render_tab_toolbar(ix, cx))
                    .when(g.view != TabView::Data, |this| {
                        this.child(self.render_table_header(ix, cx))
                    })
                    .when(g.view == TabView::Data && g.filters.visible, |this| {
                        this.child(self.render_filter_bar(ix, cx))
                    })
                    .child(div().flex_1().min_h_0().child(body))
                    .into_any_element()
            }
            Some((_, WorkspaceTab::Sql(s))) => self.render_sql_tab(s, cx).into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_family(crate::settings::ui_font())
                                .text_color(muted)
                                .child("No table open"),
                        )
                        .child(div().text_caption().text_color(muted).child(
                            crate::kbd::rich_colored(
                                "Click a table in the sidebar, or press [cmd-p] to quick-open.",
                                muted,
                            ),
                        )),
                )
                .into_any_element(),
        }
    }

    fn render_workspace(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .when(!self.tabs.is_empty(), |d| {
                d.child(div().flex_none().child(self.render_tab_strip(cx)))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.render_grid_area(cx)),
            )
            .when_some(self.bottom_panel, |d, p| {
                // Deferred above the grid's own deferred cell handles.
                d.child(gpui::deferred(self.render_bottom_panel(p, cx)).with_priority(2))
            });
        // Row detail panel beside the tabs.
        let main = if self.ai.open {
            div()
                .size_full()
                .flex()
                .flex_row()
                .child(div().flex_1().min_w_0().h_full().child(main))
                .child(gpui::deferred(self.render_ai_panel(window, cx)).with_priority(2))
        } else if self.row_panel.open {
            div()
                .size_full()
                .flex()
                .flex_row()
                .child(div().flex_1().min_w_0().h_full().child(main))
                // Above the grid's late-painted cell-frame handles.
                .child(gpui::deferred(self.render_row_detail(window, cx)).with_priority(2))
        } else {
            div().size_full().child(main)
        };
        // The status-bar panel icons can hide the sidebar entirely.
        if !self.sidebar_open {
            return div().flex_1().min_h_0().flex().flex_col().child(main);
        }
        div().flex_1().min_h_0().flex().flex_col().child(
            h_resizable("workspace-split")
                .child(
                    resizable_panel()
                        .size(px(240.))
                        .size_range(px(180.)..px(400.))
                        .flex_none()
                        .child(self.render_sidebar(cx)),
                )
                .child(resizable_panel().child(main)),
        )
    }
}

impl Render for TuskApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A search typed in the connection manager must not keep filtering
        // the connections tree after it connected.
        if std::mem::take(&mut self.clear_conn_search)
            && !self.conn_search.read(cx).value().is_empty()
        {
            self.conn_search
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        if std::mem::take(&mut self.focus_conn_search) && self.conn_manager {
            let handle = self.conn_search.read(cx).focus_handle(cx);
            handle.focus(window, cx);
        }
        // The sidebar highlight follows the active tab (none when no table
        // tab is active); a click or right-click in between still moves it.
        let tab_key = self.active_tab.and_then(|ix| match self.tabs.get(ix) {
            Some(WorkspaceTab::Grid(g)) if g.draft.is_none() => {
                Some((g.table.kind.clone(), g.table.name.clone()))
            }
            _ => None,
        });
        if tab_key != self.synced_tab {
            self.synced_tab = tab_key.clone();
            self.selected_object = tab_key;
        }
        // Nothing focused (a failed connect, a closed panel or tab took the
        // focused element away): take it back, or no shortcut (⇧⌘O, ⌘N…)
        // fires until the next click.
        if window.focused(cx).is_none() {
            self.focus.focus(window, cx);
        }
        // Welcome-screen failures (connect, import…) go out as toasts.
        if self.screen == AppScreen::Connection
            && let Some((false, msg)) = self.form.notice.take()
        {
            self.toast(false, msg);
        }
        // A refused cell value (text in a number column) → toast.
        let refused: Vec<String> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                WorkspaceTab::Grid(g) => g
                    .state
                    .update(cx, |s, _| s.delegate_mut().cell_error.take()),
                _ => None,
            })
            .collect();
        for why in refused {
            self.toast(false, why);
        }
        for (ok, msg) in std::mem::take(&mut self.toasts) {
            crate::toast::push(window, cx, ok, msg);
        }
        self.sync_row_detail(window, cx);
        self.sync_editor_prefs(window, cx);
        // A tab opened from elsewhere lands in the focused pane.
        if let (Some((l, r)), Some(cur)) = (self.split, self.active_tab)
            && cur != l
            && cur != r
            && cur < self.tabs.len()
        {
            self.split = Some(if self.split_focus == 0 {
                (cur, r)
            } else {
                (l, cur)
            });
        }
        let t = cx.theme();
        div()
            .id("tusk-app")
            .key_context("TuskApp")
            .track_focus(&self.focus)
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                if this.drag_ai_panel(f32::from(e.position.x), window, cx) {
                    return;
                }
                if this.drag_row_detail(f32::from(e.position.x), window, cx) {
                    return;
                }
                if let Some((y0, h0)) = this.bottom_drag {
                    let max = (f32::from(window.viewport_size().height) - 200.).max(120.);
                    this.bottom_h = (h0 - (f32::from(e.position.y) - y0)).clamp(90., max);
                    cx.notify();
                    return;
                }
                let Some((y0, h0)) = this.split_drag else {
                    return;
                };
                let max = (f32::from(window.viewport_size().height) - 220.).max(120.);
                let h = (h0 + f32::from(e.position.y) - y0).clamp(80., max);
                if let Some(WorkspaceTab::Sql(t)) =
                    this.active_tab.and_then(|ix| this.tabs.get_mut(ix))
                {
                    t.editor_h = h;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    this.split_drag = None;
                    this.bottom_drag = None;
                    this.row_panel.drag = None;
                    this.ai.drag = None;
                }),
            )
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_quick_open))
            .on_action(cx.listener(Self::on_close_overlay))
            .on_action(cx.listener(Self::on_new_connection))
            .on_action(cx.listener(Self::on_quick_connect))
            .on_action(cx.listener(Self::on_test_action))
            .on_action(cx.listener(Self::on_save_action))
            .on_action(cx.listener(Self::on_connect_action))
            .on_action(cx.listener(|this, _: &ConnectRecent1, w, cx| this.on_recent(0, w, cx)))
            .on_action(cx.listener(|this, _: &ConnectRecent2, w, cx| this.on_recent(1, w, cx)))
            .on_action(cx.listener(|this, _: &ConnectRecent3, w, cx| this.on_recent(2, w, cx)))
            .on_action(cx.listener(|this, _: &ConnectRecent4, w, cx| this.on_recent(3, w, cx)))
            .on_action(cx.listener(|this, _: &ConnectRecent5, w, cx| this.on_recent(4, w, cx)))
            .on_action(cx.listener(|this, _: &NewSqlTab, w, cx| this.open_sql_tab(w, cx)))
            .on_action(cx.listener(|this, _: &RunCurrent, w, cx| {
                this.run_action(crate::sql::RunScope::Current, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenConnections, _, cx| this.toggle_conn_manager(cx)))
            .on_action(cx.listener(|this, _: &GridEdit, w, cx| {
                // Enter edits the framed cell in the Structure / Index views too.
                if let Some(WorkspaceTab::Grid(g)) =
                    this.active_tab.and_then(|ix| this.tabs.get(ix))
                {
                    match (g.view, g.structure.clone(), g.indexes.clone()) {
                        (TabView::Structure, Some(st), _) => {
                            st.update(cx, |st, cx| {
                                let cell = st.selected_cell();
                                st.delegate_mut().enter_edit(cell, w, cx);
                            });
                            return;
                        }
                        (TabView::Index, _, Some(st)) => {
                            st.update(cx, |st, cx| {
                                let cell = st.selected_cell();
                                st.delegate_mut().enter_edit(cell, w, cx);
                            });
                            return;
                        }
                        _ => {}
                    }
                }
                this.with_active_grid(w, cx, |st, w, cx| {
                    if let Some((r, c)) = st.selected_cell() {
                        st.delegate_mut().begin_edit(r, c, w, cx);
                    }
                })
            }))
            .on_action(cx.listener(|this, _: &GridInsertRow, w, cx| {
                this.with_active_grid(w, cx, |st, w, cx| {
                    st.delegate_mut().add_row(w, cx);
                })
            }))
            .on_action(cx.listener(|this, _: &GridDuplicateRow, w, cx| {
                this.with_active_grid(w, cx, |st, w, cx| {
                    let row = st.selected_cell().map(|(r, _)| r).or(st.selected_row());
                    if let Some(r) = row {
                        st.delegate_mut().duplicate_row(r, w, cx);
                    }
                })
            }))
            .on_action(cx.listener(|this, _: &GridCopy, w, cx| this.grid_copy(w, cx)))
            .on_action(cx.listener(|this, _: &GridCopyCells, w, cx| this.grid_copy(w, cx)))
            .on_action(cx.listener(|this, _: &GridPaste, w, cx| this.grid_paste(w, cx)))
            .on_action(cx.listener(|this, _: &GridSetNull, w, cx| {
                this.with_active_grid(w, cx, |st, _, cx| {
                    if let Some((r, c)) = st.selected_cell() {
                        st.delegate_mut().set_null(r, c);
                        cx.notify();
                    }
                })
            }))
            .on_action(cx.listener(|this, _: &GridEditNext, w, cx| {
                if let Some(WorkspaceTab::Grid(g)) =
                    this.active_tab.and_then(|ix| this.tabs.get(ix))
                {
                    match (g.view, g.structure.clone(), g.indexes.clone()) {
                        (TabView::Structure, Some(st), _) => {
                            st.update(cx, |st, cx| {
                                st.delegate_mut().edit_neighbour(1, w, cx);
                                // The red frame follows the editor.
                                if let Some((r, c)) = st.delegate().editing_cell() {
                                    st.set_selected_cell(r, c, cx);
                                }
                            });
                            return;
                        }
                        (TabView::Index, _, Some(st)) => {
                            st.update(cx, |st, cx| {
                                st.delegate_mut().edit_neighbour(1, w, cx);
                                // The red frame follows the editor.
                                if let Some((r, c)) = st.delegate().editing_cell() {
                                    st.set_selected_cell(r, c, cx);
                                }
                            });
                            return;
                        }
                        _ => {}
                    }
                }
                this.with_active_grid(w, cx, |st, w, cx| {
                    st.delegate_mut().edit_neighbour(1, w, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &GridEditPrev, w, cx| {
                if let Some(WorkspaceTab::Grid(g)) =
                    this.active_tab.and_then(|ix| this.tabs.get(ix))
                {
                    match (g.view, g.structure.clone(), g.indexes.clone()) {
                        (TabView::Structure, Some(st), _) => {
                            st.update(cx, |st, cx| {
                                st.delegate_mut().edit_neighbour(-1, w, cx);
                                // The red frame follows the editor.
                                if let Some((r, c)) = st.delegate().editing_cell() {
                                    st.set_selected_cell(r, c, cx);
                                }
                            });
                            return;
                        }
                        (TabView::Index, _, Some(st)) => {
                            st.update(cx, |st, cx| {
                                st.delegate_mut().edit_neighbour(-1, w, cx);
                                // The red frame follows the editor.
                                if let Some((r, c)) = st.delegate().editing_cell() {
                                    st.set_selected_cell(r, c, cx);
                                }
                            });
                            return;
                        }
                        _ => {}
                    }
                }
                this.with_active_grid(w, cx, |st, w, cx| {
                    st.delegate_mut().edit_neighbour(-1, w, cx)
                })
            }))
            .on_action(cx.listener(|this, _: &PrevPage, w, cx| {
                if let Some(ix) = this.active_tab {
                    this.step_page(ix, -1, w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &NextPage, w, cx| {
                if let Some(ix) = this.active_tab {
                    this.step_page(ix, 1, w, cx)
                }
            }))
            // ⌘Z / ⇧⌘Z: text fields handle these themselves (deeper focus);
            // anywhere else they walk the pending changes.
            .on_action(
                cx.listener(|this, _: &gpui_kit::component::input::Undo, w, cx| {
                    this.undo_changes(false, w, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &gpui_kit::component::input::Redo, w, cx| {
                    this.undo_changes(true, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &BackupDatabase, _, cx| {
                if this.supports("Backup", |c| c.backup, cx) {
                    this.open_backup(crate::backup::Mode::Backup, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &RestoreDatabase, _, cx| {
                if this.supports("Restore", |c| c.backup, cx) {
                    this.open_backup(crate::backup::Mode::Restore, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &RunAll, w, cx| {
                this.run_action(crate::sql::RunScope::All, w, cx)
            }))
            .on_action(cx.listener(|this, _: &RefreshActive, _, cx| this.refresh_active_tab(cx)))
            .on_action(cx.listener(|this, _: &SaveChanges, w, cx| this.save_changes(w, cx)))
            .on_action(cx.listener(|this, _: &DeleteSelection, w, cx| this.delete_selection(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleFilters, w, cx| this.toggle_filters(w, cx)))
            .on_action(cx.listener(|this, _: &ShowCompletions, w, cx| this.show_completions(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleRowDetail, _, cx| this.toggle_row_detail(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleAiPanel, window, cx| this.toggle_ai_panel(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SendToChat, window, cx| {
                    this.send_active_to_chat(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.cycle_tab(1, cx)))
            .on_action(cx.listener(|this, _: &ProcessList, w, cx| {
                if this.supports("The process list", |c| c.processes, cx) {
                    this.open_process_list(w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &SearchInDatabase, w, cx| {
                if this.supports("Search in database", |c| c.sql, cx) {
                    this.open_db_search(w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &UserManagement, w, cx| {
                if this.supports("User management", |c| c.roles, cx) {
                    this.open_user_mgmt(w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &SplitPaneRight, w, cx| this.toggle_split(w, cx)))
            .on_action(cx.listener(|this, _: &NextPane, _, cx| {
                let p = 1 - this.split_focus;
                this.focus_pane(p, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleResultsPane, _, cx| {
                this.results_hidden = !this.results_hidden;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowAllTabs, w, cx| {
                this.open_name_picker(crate::palette::PaletteMode::Tabs, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenSqlFile, w, cx| this.open_sql_file(w, cx)))
            .on_action(cx.listener(|this, _: &SaveQueryAs, w, cx| this.save_query_as(w, cx)))
            .on_action(cx.listener(|this, _: &ImportCsv, w, cx| this.import_file(false, w, cx)))
            .on_action(cx.listener(|this, _: &ImportJson, w, cx| this.import_file(true, w, cx)))
            .on_action(cx.listener(|this, _: &ImportSqlDump, w, cx| this.import_sql_dump(w, cx)))
            .on_action(
                cx.listener(|this, _: &ImportDockerCompose, w, cx| {
                    this.import_docker_compose(w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ExportTables, _, cx| this.export_tables(cx)))
            .on_action(
                cx.listener(|this, _: &ExportActiveTable, w, cx| this.export_active_table(w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleLineComment, w, cx| this.toggle_line_comment(w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleBlockComment, w, cx| this.toggle_block_comment(w, cx)),
            )
            .on_action(cx.listener(|this, _: &MakeUpperCase, w, cx| {
                this.transform_selection(editor_cmds::CaseTransform::Upper, w, cx)
            }))
            .on_action(cx.listener(|this, _: &MakeLowerCase, w, cx| {
                this.transform_selection(editor_cmds::CaseTransform::Lower, w, cx)
            }))
            .on_action(cx.listener(|this, _: &Capitalize, w, cx| {
                this.transform_selection(editor_cmds::CaseTransform::Capitalize, w, cx)
            }))
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _, cx| this.step_font_size(1., cx)))
            .on_action(
                cx.listener(|this, _: &DecreaseFontSize, _, cx| this.step_font_size(-1., cx)),
            )
            .on_action(cx.listener(|this, _: &ResetFontSize, _, cx| this.step_font_size(0., cx)))
            .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.cycle_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &TabAt6, _, cx| this.tab_at(5, cx)))
            .on_action(cx.listener(|this, _: &TabAt7, _, cx| this.tab_at(6, cx)))
            .on_action(cx.listener(|this, _: &TabAt8, _, cx| this.tab_at(7, cx)))
            .on_action(cx.listener(|this, _: &TabAtLast, _, cx| this.tab_at(8, cx)))
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.navigate_tabs(true, cx)))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.navigate_tabs(false, cx)))
            .on_action(cx.listener(|this, _: &ReloadWorkspace, _, cx| this.reload_workspace(cx)))
            .on_action(cx.listener(|this, _: &Reconnect, _, cx| this.reconnect(cx)))
            .on_action(cx.listener(|this, _: &CloseAllTabs, _, cx| this.close_all_tabs(cx)))
            .on_action(cx.listener(|this, _: &DiscardChanges, _, cx| this.discard_changes(cx)))
            .on_action(cx.listener(|this, _: &PreviewChanges, w, cx| this.preview_changes(w, cx)))
            .on_action(cx.listener(|this, _: &OpenDatabase, w, cx| {
                this.open_name_picker(crate::palette::PaletteMode::Databases, w, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenConnection, w, cx| {
                this.open_name_picker(crate::palette::PaletteMode::Connections, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleConsole, _, cx| {
                this.toggle_bottom_panel(panels::BottomPanel::Console, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleProblems, _, cx| {
                this.toggle_bottom_panel(panels::BottomPanel::Problems, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, _, cx| {
                this.toggle_bottom_panel(panels::BottomPanel::History, cx)
            }))
            .on_action(cx.listener(|this, _: &Disconnect, w, cx| this.disconnect_guarded(w, cx)))
            .on_action(cx.listener(|this, _: &ShowTablesPanel, _, cx| {
                this.show_panel(SidebarPanel::Tables, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowViewsPanel, _, cx| {
                this.show_panel(SidebarPanel::Views, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowFunctionsPanel, _, cx| {
                this.show_panel(SidebarPanel::Functions, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowData, w, cx| {
                if let Some(ix) = this.active_tab {
                    this.set_tab_view(ix, TabView::Data, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ShowStructure, w, cx| {
                if let Some(ix) = this.active_tab {
                    this.set_tab_view(ix, TabView::Structure, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseTab, w, cx| {
                if let Some(ix) = this.active_tab {
                    this.close_tab_guarded(ix, w, cx);
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(t.colors.background)
            .text_color(t.colors.foreground)
            .font_family(crate::settings::ui_font())
            .child(self.render_title_bar(cx))
            .child(match self.screen {
                AppScreen::Connection => self.render_connection(cx).into_any_element(),
                AppScreen::Workspace => self.render_workspace(window, cx).into_any_element(),
            })
            .when(self.screen == AppScreen::Workspace, |d| {
                d.child(self.render_status_bar(cx))
            })
            // Overlays paint after the grid's late-painted cell handles.
            .when_some(
                self.conn_manager.then(|| self.render_conn_manager(cx)),
                |this, m| this.child(gpui::deferred(m).with_priority(4)),
            )
            .when_some(
                self.migrate.is_some().then(|| self.render_migrate(cx)),
                |this, m| this.child(gpui::deferred(m).with_priority(4)),
            )
            // (Not deferred: the palette's command list dispatches its rows'
            // actions through the regular element tree.)
            .when_some(
                self.palette.is_some().then(|| self.render_palette(cx)),
                |this, p| this.child(p),
            )
            .when(self.users.is_some(), |this| {
                this.child(gpui::deferred(self.render_user_mgmt(cx)).with_priority(4))
            })
            .when(self.db_search.is_some(), |this| {
                this.child(gpui::deferred(self.render_db_search(cx)).with_priority(4))
            })
            .when(self.processes.is_some(), |this| {
                this.child(gpui::deferred(self.render_process_list(cx)).with_priority(4))
            })
            .when(self.preview.is_some(), |this| {
                this.child(gpui::deferred(self.render_preview(cx)).with_priority(4))
            })
            .children(gpui_kit::component::Root::render_notification_layer(
                window, cx,
            ))
    }
}

/// Editor-style type scale for the welcome screen: `px16` is a size on a
/// 16px base, scaled so the base is the UI font size (14 → ×0.875).
fn zrem(px16: f32) -> Pixels {
    px(px16 / 16. * crate::settings::get().ui_font_size)
}

/// Shorten `SELECT version()` output to the human part.
pub(crate) fn short_version(full: &str) -> String {
    let first = full.split(',').next().unwrap_or(full).trim();
    if first.len() > 90 {
        format!("{}…", &first[..90])
    } else {
        first.to_string()
    }
}

/// 1234567 -> "1,234,567" for the status bar.
fn fmt_int(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}
