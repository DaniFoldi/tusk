//! New-connection dialog: a separate floating window,
//! opened by cmd-n / "New Connection". Owns its own form state; on a
//! successful connect it hands the pool to the main view and closes itself.

use crate::theme::TextCaption as _;
use gpui_kit::component::IndexPath;
use gpui_kit::component::Root;
use gpui_kit::component::TitleBar;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::TuskHandle;
use crate::conn::ConnectionForm;
use crate::db::{self, SslMode};

/// The open dialog window, so cmd-n focuses it instead of stacking a second one.
#[derive(Default)]
struct OpenDialog(Option<AnyWindowHandle>);
impl Global for OpenDialog {}

pub struct ConnDialog {
    form: ConnectionForm,
    focus: FocusHandle,
    /// Tag picker (kit Select): "none" + the environment tags.
    tag_select: Entity<SelectState<Vec<SharedString>>>,
    ssl_select: Entity<SelectState<Vec<SharedString>>>,
    /// Connection-list group ("No group" + existing groups).
    group_select: Entity<SelectState<Vec<SharedString>>>,
    /// New connection: the engine grid is shown first (the form after a pick).
    choosing: bool,
    /// Highlighted engine in the grid.
    picked: crate::engine::Engine,
    /// "Import from URL": the URL field, shown once the button is pressed.
    url: Option<Entity<gpui_kit::component::input::InputState>>,
    _subs: Vec<Subscription>,
}

const NO_TAG: &str = "none";
const NO_GROUP: &str = "No group";

/// Spells SSL modes in capitals.
fn ssl_label(m: SslMode) -> &'static str {
    match m {
        SslMode::Disable => "DISABLE",
        SslMode::Prefer => "PREFERRED",
        SslMode::Require => "REQUIRED",
    }
}
/// Width of the right-aligned label column.
const LABEL_W: f32 = 92.;
/// Title bar height (the form below it scrolls when taller than the screen).
const TITLE_H: f32 = 34.;

impl ConnDialog {
    pub fn new(form: ConnectionForm, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut items: Vec<SharedString> = vec![NO_TAG.into()];
        items.extend(
            db::ConnTag::ALL
                .iter()
                .map(|t| SharedString::from(t.label())),
        );
        let selected = form
            .tag
            .and_then(|t| db::ConnTag::ALL.iter().position(|x| *x == t))
            .map(|i| i + 1)
            .unwrap_or(0);
        let tag_select =
            cx.new(|cx| SelectState::new(items, Some(IndexPath::new(selected)), window, cx));
        let sub = cx.subscribe(
            &tag_select,
            |this, _, ev: &SelectEvent<Vec<SharedString>>, cx| {
                let SelectEvent::Confirm(value) = ev;
                this.form.tag = value.as_deref().and_then(db::ConnTag::from_label);
                cx.notify();
            },
        );
        // SSL mode.
        let ssl_items: Vec<SharedString> = SslMode::all()
            .iter()
            .map(|m| SharedString::from(ssl_label(*m)))
            .collect();
        let ssl_ix = SslMode::all()
            .iter()
            .position(|m| *m == form.ssl)
            .unwrap_or(1);
        let ssl_select =
            cx.new(|cx| SelectState::new(ssl_items, Some(IndexPath::new(ssl_ix)), window, cx));
        let ssl_sub = cx.subscribe(
            &ssl_select,
            |this, _, ev: &SelectEvent<Vec<SharedString>>, cx| {
                let SelectEvent::Confirm(value) = ev;
                if let Some(m) = SslMode::all()
                    .into_iter()
                    .find(|m| value.as_deref() == Some(ssl_label(*m)))
                {
                    this.form.ssl = m;
                }
                cx.notify();
            },
        );
        // Group: existing groups (incl. empty ones) + the profile's own.
        let current_group = ConnectionForm::text(&form.folder, cx).trim().to_string();
        let mut groups = db::load_groups();
        groups.extend(form.saved.iter().filter_map(|c| c.folder.clone()));
        if !current_group.is_empty() {
            groups.push(current_group.clone());
        }
        groups.sort_by_key(|g| g.to_lowercase());
        groups.dedup();
        let mut group_items: Vec<SharedString> = vec![NO_GROUP.into()];
        group_items.extend(groups.into_iter().map(SharedString::from));
        let group_ix = group_items
            .iter()
            .position(|g| !current_group.is_empty() && g.as_ref() == current_group)
            .unwrap_or(0);
        let group_select =
            cx.new(|cx| SelectState::new(group_items, Some(IndexPath::new(group_ix)), window, cx));
        let group_sub = cx.subscribe_in(
            &group_select,
            window,
            |this, _, ev: &SelectEvent<Vec<SharedString>>, window, cx| {
                let SelectEvent::Confirm(value) = ev;
                let v = value
                    .as_deref()
                    .filter(|v| *v != NO_GROUP)
                    .unwrap_or("")
                    .to_string();
                this.form
                    .folder
                    .update(cx, |s, cx| s.set_value(v, window, cx));
                cx.notify();
            },
        );
        let choosing = form.editing.is_none() && ConnectionForm::text(&form.name, cx).is_empty();
        let picked = form.engine;
        Self {
            form,
            focus: cx.focus_handle(),
            tag_select,
            ssl_select,
            group_select,
            choosing,
            picked,
            url: None,
            _subs: vec![sub, ssl_sub, group_sub],
        }
    }

    /// Engine picked in the grid: prefill the form with its defaults.
    fn choose_engine(
        &mut self,
        engine: crate::engine::Engine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let set = |e: &Entity<gpui_kit::component::input::InputState>,
                   v: String,
                   window: &mut Window,
                   cx: &mut App| {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        };
        self.form.engine = engine;
        let port = engine.default_port();
        set(
            &self.form.port,
            if port == 0 {
                String::new()
            } else {
                port.to_string()
            },
            window,
            cx,
        );
        set(
            &self.form.user,
            engine.default_user().to_string(),
            window,
            cx,
        );
        let db = match engine {
            crate::engine::Engine::Postgres | crate::engine::Engine::Greenplum => "postgres",
            crate::engine::Engine::Cockroach => "defaultdb",
            crate::engine::Engine::MsSql => "master",
            crate::engine::Engine::Oracle => "FREEPDB1",
            crate::engine::Engine::Redis => "0",
            crate::engine::Engine::ClickHouse => "default",
            _ => "",
        };
        set(&self.form.database, db.to_string(), window, cx);
        let hint = engine.database_hint();
        self.form
            .database
            .update(cx, |s, cx| s.set_placeholder(hint, window, cx));
        if engine == crate::engine::Engine::DynamoDb {
            for (k, e) in &self.form.options {
                if *k == "region" {
                    set(e, "us-east-1".to_string(), window, cx);
                }
            }
        }
        self.choosing = false;
        self.form.name.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Browse… for a database / key file.
    fn browse_file(
        &mut self,
        target: Option<&'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let v = path.display().to_string();
                let input = match target {
                    None => Some(this.form.path.clone()),
                    Some(k) => this
                        .form
                        .options
                        .iter()
                        .find(|(key, _)| *key == k)
                        .map(|(_, e)| e.clone()),
                };
                if let Some(e) = input {
                    e.update(cx, |s, cx| s.set_value(v, window, cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn option(&self, key: &str) -> Option<Entity<gpui_kit::component::input::InputState>> {
        self.form
            .options
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, e)| e.clone())
    }

    /// Blank profile for "New Connection".
    fn blank() -> db::SavedConnection {
        db::SavedConnection {
            engine: crate::engine::Engine::Postgres,
            path: None,
            options: Default::default(),
            name: String::new(),
            host: "127.0.0.1".to_string(),
            port: 5432,
            database: "postgres".to_string(),
            user: "postgres".to_string(),
            ssl: SslMode::Prefer,
            folder: None,
            tag: None,
            last_used: None,
            ssh: None,
            status_color: None,
        }
    }

    /// Edit (or duplicate, with `as_copy`) a saved profile in the dialog.
    pub fn open_edit(conn: db::SavedConnection, as_copy: bool, cx: &mut App) {
        let mut conn = conn;
        if as_copy {
            conn.name = format!("{} copy", conn.name);
            conn.last_used = None;
        }
        Self::open_with(Some(conn), !as_copy, cx);
    }

    /// Open the dialog as a new centered window.
    /// Opens at an estimated height; the first frame measures the content and
    /// resizes the window to fit it exactly (see the canvas in `render`).
    pub fn open(cx: &mut App) {
        Self::open_with(None, false, cx);
    }

    fn open_with(conn: Option<db::SavedConnection>, editing: bool, cx: &mut App) {
        if let Some(handle) = cx.default_global::<OpenDialog>().0
            && cx
                .update_window(handle, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let bounds = Bounds::centered(None, size(px(600.), px(600.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(600.), px(200.))),
                focus: !crate::background(),
                kind: crate::theme::secondary_window_kind(),
                ..TitleBar::window_options()
            },
            |window, cx| {
                let initial = conn.clone().unwrap_or_else(Self::blank);
                let mut form = ConnectionForm::new(window, cx, &initial);
                if editing {
                    form.editing = Some(initial.name.clone());
                }
                let view = cx.new(|cx| ConnDialog::new(form, window, cx));
                let focus = view.read(cx).focus.clone();
                focus.focus(window, cx);
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        match result {
            Ok(handle) => cx.set_global(OpenDialog(Some(handle.into()))),
            Err(e) => eprintln!("connection dialog failed to open: {e}"),
        }
    }

    fn read_or_notice(&mut self, cx: &mut Context<Self>) -> Option<(db::SavedConnection, String)> {
        let (conn, typed) = self.form.read_form(cx);
        let password = crate::conn::ConnectionForm::password_or_keychain(&conn.name, typed);
        if !conn.is_valid() {
            use crate::engine::Form;
            let what = match conn.engine.form() {
                Form::File => "Fill in the name and the database file.",
                Form::UrlToken => "Fill in the name and the server URL.",
                Form::CloudflareD1 => {
                    "Fill in the name and the account id (the database is optional)."
                }
                Form::Snowflake => "Fill in the name, account and user.",
                Form::BigQuery => "Fill in the name and the project.",
                Form::DynamoDb => "Fill in the name and the region (or an endpoint).",
                Form::Server => "Fill in name, host, port, database and user.",
            };
            self.form.notice = Some((false, what.to_string()));
            cx.notify();
            return None;
        }
        Some((conn, password))
    }

    fn on_test(&mut self, _: &ClickEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.read_or_notice(cx) else {
            return;
        };
        let ssh_secret = self.form.ssh_secret_text(cx);
        self.form.busy = true;
        self.form.notice = Some((true, "Testing connection…".to_string()));
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result = db::test_connect(conn, password, Some(ssh_secret)).await;
            let _ = weak.update(cx, |this: &mut ConnDialog, cx| {
                this.form.busy = false;
                this.form.notice = Some(match result {
                    Ok(v) => (true, v.summary()),
                    Err(e) => (false, e),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// A new profile (or a rename) may not take an existing profile's name:
    /// saving would silently replace that profile.
    fn name_taken_notice(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        if !db::connection_name_taken(name, self.form.editing.as_deref()) {
            return false;
        }
        self.form.notice = Some((
            false,
            format!("A connection named “{name}” already exists. Pick another name."),
        ));
        cx.notify();
        true
    }

    /// Write the profile to `connections.json` and its secrets to the
    /// Keychain (Save, and Connect — a profile you connected to is kept).
    /// Ok carries a Keychain warning, if any; Err the message to show.
    fn persist(
        &mut self,
        conn: &db::SavedConnection,
        password: &str,
        ssh_secret: &str,
        cx: &mut Context<Self>,
    ) -> Result<Option<String>, String> {
        // Verify the write by reading back: some environments (e.g. unsigned
        // dev builds) silently drop Keychain writes, and a false "Saved"
        // leaves the user locked out on restart with no explanation.
        let mut keychain_warning: Option<String> = None;
        if self.form.save_password {
            db::save_password(&conn.name, password)
                .map_err(|e| format!("{} error: {e}", db::CREDENTIAL_STORE))?;
            match db::load_password(&conn.name) {
                Ok(back) if back == password => {}
                Ok(_) => {
                    keychain_warning = Some(format!(
                        "the {} gave back a different password",
                        db::CREDENTIAL_STORE
                    ));
                }
                Err(e) => {
                    keychain_warning = Some(format!(
                        "the password can't be read back from the {} ({e})",
                        db::CREDENTIAL_STORE
                    ));
                }
            }
        }
        if conn.ssh.is_some() && !ssh_secret.is_empty() {
            db::save_ssh_secret(&conn.name, ssh_secret)
                .map_err(|e| format!("{} error: {e}", db::CREDENTIAL_STORE))?;
        }
        // Editing an existing profile: replace it in place (a rename moves
        // its Keychain secrets along); otherwise add or update by name.
        let original = self.form.editing.clone();
        if let Some(old) = original.as_ref().filter(|old| **old != conn.name) {
            if password.is_empty()
                && let Ok(pw) = db::load_password(old)
            {
                let _ = db::save_password(&conn.name, &pw);
            }
            if ssh_secret.is_empty()
                && let Some(sec) = db::load_ssh_secret(old)
            {
                let _ = db::save_ssh_secret(&conn.name, &sec);
            }
            db::delete_secrets(old);
        }
        let (list, ix) = db::upsert_connection(conn.clone(), original.as_deref())
            .map_err(|e| format!("Save failed: {e:#}"))?;
        self.form.saved = list;
        self.form.selected = Some(ix);
        self.form.editing = Some(conn.name.clone());
        // The welcome screen lists saved connections — show it now, not only
        // after a restart.
        let main = cx.global::<TuskHandle>().0.clone();
        main.update(cx, |app, cx| app.reload_saved_connections(cx));
        Ok(keychain_warning)
    }

    fn on_save(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.read_or_notice(cx) else {
            return;
        };
        if self.name_taken_notice(&conn.name, cx) {
            return;
        }
        let ssh_secret = self.form.ssh_secret_text(cx);
        match self.persist(&conn, &password, &ssh_secret, cx) {
            // Saved cleanly: done, close the window.
            Ok(None) => {
                window.remove_window();
                return;
            }
            // A Keychain warning keeps it open so the message can be read.
            Ok(Some(w)) => {
                self.form.notice = Some((
                    false,
                    format!(
                        "Saved “{}” — but {w}; it won't survive a restart.",
                        conn.name
                    ),
                ));
            }
            Err(e) => self.form.notice = Some((false, e)),
        }
        cx.notify();
    }

    fn on_connect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, password)) = self.read_or_notice(cx) else {
            return;
        };
        if self.name_taken_notice(&conn.name, cx) {
            return;
        }
        let ssh_secret = self.form.ssh_secret_text(cx);
        self.form.busy = true;
        self.form.notice = Some((true, format!("Connecting to {}…", conn.name)));
        cx.notify();
        let dialog_window = window.window_handle();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let result =
                db::connect(conn.clone(), password.clone(), Some(ssh_secret.clone())).await;
            let main = cx.update(|cx| cx.global::<TuskHandle>().0.clone());
            let _ = weak.update(cx, |this: &mut ConnDialog, cx| {
                this.form.busy = false;
                match result {
                    Ok(c) => {
                        // Connected: keep the profile (before `connected_with`,
                        // which stamps it as recently used).
                        let saved = this.persist(&conn, &password, &ssh_secret, cx);
                        main.update(cx, |app, cx| {
                            match saved {
                                Ok(None) => {}
                                Ok(Some(w)) => {
                                    app.toast(false, format!("Saved “{}” — but {w}.", conn.name))
                                }
                                Err(e) => app.toast(false, e),
                            }
                            app.connected_with(c, &password, cx);
                        });
                        cx.notify();
                        let _ = cx.update_window(dialog_window, |_, window, _| {
                            window.remove_window();
                        });
                    }
                    Err(e) => {
                        this.form.notice = Some((false, e));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

impl ConnDialog {
    fn render_inner(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let ok_green = cx.theme().green;
        let err_red = cx.theme().red;
        let background = cx.theme().background;
        let border = cx.theme().border;
        let busy = self.form.busy;

        // Result / progress line: inline, right above the buttons, so it
        // never covers a field (click to dismiss). The window grows to fit.
        let notice = self.form.notice.clone().map(|(ok, text)| {
            let fg = if busy {
                muted
            } else if ok {
                ok_green
            } else {
                err_red
            };
            div()
                .id("dlg-notice")
                .px_1()
                .text_sm()
                .font_family(crate::settings::ui_font())
                .text_color(fg)
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.form.busy {
                        this.form.notice = None;
                        cx.notify();
                    }
                }))
                .child(text)
        });

        // ---- connection form: cards of label/field rows ----
        let card_bg = cx.theme().group_box;
        let label = |text: &str| {
            div()
                .w(px(LABEL_W))
                .flex_none()
                .text_sm()
                .text_right()
                .text_color(foreground)
                .child(text.to_string())
        };
        let line = |children: Vec<AnyElement>| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .py(px(3.))
                .children(children)
        };
        let grow = |el: AnyElement| div().flex_1().min_w_0().child(el).into_any_element();
        let fixed =
            |w: f32, el: AnyElement| div().w(px(w)).flex_none().child(el).into_any_element();
        let small_label = |text: &str| {
            div()
                .flex_none()
                .text_sm()
                .text_color(foreground)
                .child(text.to_string())
                .into_any_element()
        };
        let card = |rows: Vec<AnyElement>| {
            div()
                .flex()
                .flex_col()
                .px_3()
                .py_1p5()
                .rounded(crate::theme::RADIUS_LG)
                .bg(card_bg)
                .border_1()
                .border_color(border)
                .children(rows)
        };

        // Status color swatches: the selected one is a pill.
        let mut swatches = div().flex().items_center().gap_1p5();
        for (i, c) in db::STATUS_COLORS.iter().enumerate() {
            let selected = self.form.status_color.unwrap_or(0) == i;
            swatches = swatches.child(
                div()
                    .id(("dlg-color", i))
                    .h(px(22.))
                    .w(px(if selected { 44. } else { 22. }))
                    .rounded(crate::theme::RADIUS_MD)
                    .bg(rgb(*c))
                    .when(selected, |this| {
                        this.border_2().border_color(foreground.opacity(0.6))
                    })
                    .hover(|this| this.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.form.status_color = Some(i);
                        cx.notify();
                    })),
            );
        }

        let identity = card(vec![
            line(vec![
                label("Name").into_any_element(),
                grow(Input::new(&self.form.name).into_any_element()),
            ])
            .into_any_element(),
            line(vec![
                label("Status Color").into_any_element(),
                swatches.into_any_element(),
                div().flex_1().into_any_element(),
                small_label("Tag"),
                fixed(
                    150.,
                    Select::new(&self.tag_select).small().into_any_element(),
                ),
            ])
            .into_any_element(),
            line(vec![
                label("Group").into_any_element(),
                grow(Select::new(&self.group_select).small().into_any_element()),
            ])
            .into_any_element(),
        ]);

        use crate::engine::{Engine, Form};
        let engine = self.form.engine;
        let keychain = Checkbox::new("dlg-save-pw")
            .label(format!("Store in {}", db::CREDENTIAL_STORE))
            .checked(self.form.save_password)
            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                this.form.save_password = *checked;
                cx.notify();
            }))
            .into_any_element();
        let secret_row = |label_text: &str, keychain: AnyElement| {
            line(vec![
                label(label_text).into_any_element(),
                grow(Input::new(&self.form.password).into_any_element()),
                fixed(150., keychain),
            ])
            .into_any_element()
        };
        let opt_row = |label_text: &str, key: &str| {
            self.option(key).map(|e| {
                line(vec![
                    label(label_text).into_any_element(),
                    grow(Input::new(&e).into_any_element()),
                ])
                .into_any_element()
            })
        };
        let browse = |id: &'static str, target: Option<&'static str>| {
            Button::new(id)
                .label("Browse…")
                .small()
                .on_click(
                    cx.listener(move |this, _, window, cx| this.browse_file(target, window, cx)),
                )
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        match engine.form() {
            Form::Server => {
                rows.push(
                    line(vec![
                        label("Host/Socket").into_any_element(),
                        grow(Input::new(&self.form.host).into_any_element()),
                        small_label("Port"),
                        fixed(90., Input::new(&self.form.port).into_any_element()),
                    ])
                    .into_any_element(),
                );
                if engine != Engine::Redis {
                    rows.push(
                        line(vec![
                            label("User").into_any_element(),
                            grow(Input::new(&self.form.user).into_any_element()),
                        ])
                        .into_any_element(),
                    );
                }
                rows.push(secret_row("Password", keychain));
                rows.push(
                    line(vec![
                        label(match engine {
                            Engine::Oracle => "Service",
                            Engine::Cassandra => "Keyspace",
                            Engine::Redis => "Database",
                            _ => "Database",
                        })
                        .into_any_element(),
                        grow(Input::new(&self.form.database).into_any_element()),
                    ])
                    .into_any_element(),
                );
                if !matches!(engine, Engine::Redis | Engine::Cassandra | Engine::MongoDb) {
                    rows.push(
                        line(vec![
                            label("SSL mode").into_any_element(),
                            fixed(
                                200.,
                                Select::new(&self.ssl_select).small().into_any_element(),
                            ),
                        ])
                        .into_any_element(),
                    );
                }
            }
            Form::File => {
                rows.push(
                    line(vec![
                        label("Path").into_any_element(),
                        grow(Input::new(&self.form.path).into_any_element()),
                        browse("dlg-browse", None),
                    ])
                    .into_any_element(),
                );
                rows.push(
                    div()
                        .pl(px(LABEL_W + 8.))
                        .pb_1()
                        .text_caption()
                        .text_color(muted)
                        .child("A file that doesn't exist yet is created.")
                        .into_any_element(),
                );
            }
            Form::UrlToken => {
                rows.push(
                    line(vec![
                        label("URL").into_any_element(),
                        grow(Input::new(&self.form.path).into_any_element()),
                    ])
                    .into_any_element(),
                );
                rows.push(secret_row("Auth Token", keychain));
            }
            Form::CloudflareD1 => {
                rows.extend(opt_row("Account ID", "account_id"));
                rows.push(
                    line(vec![
                        label("Database").into_any_element(),
                        grow(Input::new(&self.form.database).into_any_element()),
                    ])
                    .into_any_element(),
                );
                rows.push(secret_row("API Token", keychain));
            }
            Form::Snowflake => {
                rows.extend(opt_row("Account", "account"));
                rows.push(
                    line(vec![
                        label("User").into_any_element(),
                        grow(Input::new(&self.form.user).into_any_element()),
                    ])
                    .into_any_element(),
                );
                rows.push(secret_row("Access Token", keychain));
                // Key-pair sign-in instead of a token.
                if let Some(e) = self.option("key_file") {
                    rows.push(
                        line(vec![
                            label("Private Key").into_any_element(),
                            grow(Input::new(&e).into_any_element()),
                            browse("dlg-browse-sf-key", Some("key_file")),
                        ])
                        .into_any_element(),
                    );
                }
                rows.extend(opt_row("Warehouse", "warehouse"));
                rows.push(
                    line(vec![
                        label("Database").into_any_element(),
                        grow(Input::new(&self.form.database).into_any_element()),
                    ])
                    .into_any_element(),
                );
                rows.extend(opt_row("Schema", "schema"));
                rows.extend(opt_row("Role", "role"));
            }
            Form::BigQuery => {
                rows.extend(opt_row("Project", "project"));
                rows.extend(opt_row("Dataset", "dataset"));
                if let Some(e) = self.option("key_file") {
                    rows.push(
                        line(vec![
                            label("Key File").into_any_element(),
                            grow(Input::new(&e).into_any_element()),
                            browse("dlg-browse-key", Some("key_file")),
                        ])
                        .into_any_element(),
                    );
                }
                rows.extend(opt_row("Endpoint", "endpoint"));
            }
            Form::DynamoDb => {
                rows.extend(opt_row("Region", "region"));
                rows.extend(opt_row("Access Key", "access_key"));
                rows.push(secret_row("Secret Key", keychain));
                rows.extend(opt_row("Endpoint", "endpoint"));
            }
        }
        let server = card(rows);

        let ssh_card = self.form.ssh_enabled.then(|| {
            card(vec![
                line(vec![
                    label("Server").into_any_element(),
                    grow(Input::new(&self.form.ssh_host).into_any_element()),
                    small_label("Port"),
                    fixed(90., Input::new(&self.form.ssh_port).into_any_element()),
                ])
                .into_any_element(),
                line(vec![
                    label("User").into_any_element(),
                    grow(Input::new(&self.form.ssh_user).into_any_element()),
                ])
                .into_any_element(),
                line(vec![
                    label(if self.form.ssh_use_key {
                        "Passphrase"
                    } else {
                        "Password"
                    })
                    .into_any_element(),
                    grow(Input::new(&self.form.ssh_secret).into_any_element()),
                ])
                .into_any_element(),
                line(vec![
                    div()
                        .w(px(LABEL_W))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .child(
                            Checkbox::new("dlg-ssh-key")
                                .label("Use SSH key")
                                .checked(self.form.ssh_use_key)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.form.ssh_use_key = *checked;
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                    grow(
                        Input::new(&self.form.ssh_key)
                            .disabled(!self.form.ssh_use_key)
                            .into_any_element(),
                    ),
                ])
                .into_any_element(),
                div()
                    .pl(px(LABEL_W + 8.))
                    .pb_1()
                    .text_caption()
                    .text_color(muted)
                    .child("Leave the key empty to use ~/.ssh/id_ed25519 or id_rsa.")
                    .into_any_element(),
            ])
        });

        let footer_btn = |id: &'static str, text: &'static str| {
            Button::new(id).label(text).disabled(busy).w(px(84.))
        };
        let ssh_on = self.form.ssh_enabled;
        let can_ssh = engine.form() == Form::Server;
        let (accent, accent_fg) = (cx.theme().accent, cx.theme().accent_foreground);
        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .when(self.form.editing.is_none(), |d| {
                d.child(Button::new("dlg-back").label("Back").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.picked = this.form.engine;
                        this.choosing = true;
                        cx.notify();
                    },
                )))
            })
            .when(can_ssh, |d| {
                d.child(
                    Button::new("dlg-ssh")
                        .label("Over SSH")
                        // blue while the SSH section is on.
                        .when(ssh_on, |b| b.bg(accent).text_color(accent_fg))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.form.ssh_enabled = !this.form.ssh_enabled;
                            cx.notify();
                        })),
                )
            })
            .child(div().flex_1())
            .child(footer_btn("dlg-save", "Save").on_click(cx.listener(Self::on_save)))
            .child(footer_btn("dlg-test", "Test").on_click(cx.listener(Self::on_test)))
            .child(
                footer_btn("dlg-connect", "Connect")
                    .primary()
                    .on_click(cx.listener(Self::on_connect)),
            );

        let title = if self.choosing {
            "Create a new connection".to_string()
        } else if self.form.editing.is_some() {
            format!("Edit {} Connection", engine.label())
        } else {
            format!("{} Connection", engine.label())
        };

        // Content-fit height (capped to the screen): measure the form.
        let measure = canvas(
            |bounds, window, cx| {
                let viewport = window.viewport_size();
                let max = window
                    .display(cx)
                    .map(|d| d.bounds().size.height - px(80.))
                    .unwrap_or(px(900.));
                let height = (bounds.size.height + px(TITLE_H)).min(max).ceil();
                if (height - viewport.height).abs() > px(0.5) {
                    window.on_next_frame(move |window, _| {
                        window.resize(size(window.viewport_size().width, height));
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .font_family(crate::settings::ui_font())
            .child(
                TitleBar::new()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_center()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child(title),
                    )
                    .child(div().w(px(60.))),
            )
            .child(
                div()
                    .id("dlg-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .px_5()
                            .pt_3()
                            .pb_4()
                            .child(measure)
                            .map(|d| {
                                if self.choosing {
                                    d.child(self.render_engine_grid(cx)).children(notice)
                                } else {
                                    d.child(identity)
                                        .child(server)
                                        .children(ssh_card.filter(|_| can_ssh))
                                        .children(notice)
                                        .child(footer)
                                }
                            }),
                    ),
            )
    }
}

impl ConnDialog {
    /// The engine grid of "New Connection".
    fn render_engine_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::engine::Engine;
        let t = cx.theme();
        let (fg, muted, border, active) = (
            t.foreground,
            t.muted_foreground,
            t.border,
            t.tokens.table_active,
        );
        let tiles = Engine::ALL.iter().map(|&e| {
            let on = e == self.picked;
            div()
                .id(("engine", e as usize))
                .w(px(128.))
                .h(px(96.))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .rounded(crate::theme::RADIUS_LG)
                .border_1()
                .border_color(if on {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .when(on, |d| d.bg(active))
                .hover(|d| d.bg(muted.opacity(0.1)))
                .child(crate::icons::engine_badge(e, 40.))
                .child(
                    div()
                        .text_caption()
                        .text_color(fg)
                        .text_center()
                        .child(e.label()),
                )
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    this.picked = e;
                    if ev.click_count() >= 2 {
                        this.choose_engine(e, window, cx);
                    }
                    cx.notify();
                }))
        });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .p_2()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .children(tiles),
            )
            .children(self.url.as_ref().map(|url| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(url)))
                    .child(
                        Button::new("dlg-url-import")
                            .label("Import")
                            .primary()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.import_url(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("dlg-url-close")
                            .icon(gpui_kit::assets::IconName::Close)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.url = None;
                                cx.notify();
                            })),
                    )
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(self.url.is_none(), |d| {
                        d.child(
                            Button::new("dlg-import-url")
                                .label("Import from URL")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.show_url_field(window, cx)
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("dlg-cancel")
                            .label("Cancel")
                            .w(px(84.))
                            .on_click(cx.listener(|_, _, window, _| window.remove_window())),
                    )
                    .child(
                        Button::new("dlg-create")
                            .label("Create")
                            // Import is the default while the URL field is open.
                            .when(self.url.is_none(), |b| b.primary())
                            .w(px(84.))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let e = this.picked;
                                this.choose_engine(e, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Return: Create on the engine grid, Connect on the form.
    fn on_default(
        &mut self,
        _: &crate::dialog_keys::DialogConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.form.busy {
            return;
        }
        if self.choosing && self.url.is_some() {
            self.import_url(window, cx);
        } else if self.choosing {
            let e = self.picked;
            self.choose_engine(e, window, cx);
        } else {
            self.on_connect(&ClickEvent::default(), window, cx);
        }
    }

    /// "Import from URL": show the URL field (pre-filled when the clipboard
    /// holds a connection URL). Import then fills the form from it
    /// (`postgresql://user@host:5432/db`, `mysql://…`, `redis://…`, …).
    fn show_url_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let clip = cx
            .read_from_clipboard()
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| crate::engine::parse_url(t).is_some())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            let mut st = gpui_kit::component::input::InputState::new(window, cx)
                .placeholder("postgresql://user@host:5432/database");
            st.set_value(clip, window, cx);
            st
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.url = Some(input);
        cx.notify();
    }

    fn import_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .url
            .as_ref()
            .map(|u| u.read(cx).value().to_string())
            .unwrap_or_default();
        let Some(parsed) = crate::engine::parse_url(text.trim()) else {
            self.form.notice = Some((
                false,
                "Enter a connection URL like mysql://user@host/db.".into(),
            ));
            cx.notify();
            return;
        };
        self.url = None;
        self.choose_engine(parsed.engine, window, cx);
        let set = |e: &Entity<gpui_kit::component::input::InputState>,
                   v: String,
                   window: &mut Window,
                   cx: &mut App| {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        };
        if let Some(h) = parsed.host {
            set(&self.form.host, h, window, cx);
        }
        if let Some(p) = parsed.port {
            set(&self.form.port, p.to_string(), window, cx);
        }
        if let Some(u) = parsed.user {
            set(&self.form.user, u, window, cx);
        }
        if let Some(p) = parsed.password {
            set(&self.form.password, p, window, cx);
        }
        if let Some(d) = parsed.database {
            set(&self.form.database, d, window, cx);
        }
        let file = parsed.path.as_deref().map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map_or_else(|| p.to_string(), |f| f.to_string_lossy().into_owned())
        });
        if let Some(p) = parsed.path.clone() {
            set(&self.form.path, p, window, cx);
        }
        // A name to start from ("tusk_dev @ 127.0.0.1", or the file name).
        if self.form.name.read(cx).value().trim().is_empty() {
            let host = self.form.host.read(cx).value().to_string();
            let db = self.form.database.read(cx).value().to_string();
            let name = match (file, db.is_empty(), host.is_empty()) {
                (Some(f), _, _) => f,
                (None, false, false) => format!("{db} @ {host}"),
                (None, true, false) => host,
                (None, false, true) => db,
                (None, true, true) => String::new(),
            };
            if !name.is_empty() {
                set(&self.form.name, name, window, cx);
            }
        }
        cx.notify();
    }
}

impl Render for ConnDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .key_context(crate::dialog_keys::CONTEXT)
            .on_action(crate::dialog_keys::close)
            .on_action(cx.listener(Self::on_default))
            .size_full()
            .relative()
            .child(self.render_inner(window, cx))
            .children(gpui_kit::component::Root::render_notification_layer(
                window, cx,
            ))
    }
}
