//! Workspace editing (child module of `app`, so it sees TuskApp's fields):
//! Data/Structure switch, filter bar, pending sidebar drops/renames, Delete
//! key and the ⌘S save that commits everything in one transaction.

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::InputEvent;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};

use super::*;

/// Small text button: muted text, hairline border, hover tint.
/// (Stock `ghost()` buttons keep a stuck pressed tint on this theme.)
fn pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    pill_frame(id, active, cx).child(label.into())
}

/// The pill's box without content (icon + label buttons add their own).
fn pill_frame(id: impl Into<ElementId>, active: bool, cx: &App) -> Stateful<Div> {
    let t = cx.theme();
    let (muted, fg, border) = (t.muted_foreground, t.foreground, t.border);
    div()
        .id(id)
        .h(px(crate::settings::row_h()))
        .px_2()
        .flex()
        .items_center()
        .rounded(crate::theme::RADIUS_MD)
        .border_1()
        .border_color(border)
        .text_caption()
        .text_color(if active { fg } else { muted })
        .when(active, |this| this.bg(muted.opacity(0.18)))
        .hover(|this| this.bg(muted.opacity(0.1)).text_color(fg))
}

/// Square icon button for the bottom bar (paging arrows, page settings).
fn icon_btn(id: impl Into<ElementId>, icon: IconName, enabled: bool, cx: &App) -> Stateful<Div> {
    let t = cx.theme();
    let (muted, fg) = (t.muted_foreground, t.foreground);
    div()
        .id(id)
        .size(px(crate::settings::row_h()))
        .flex()
        .items_center()
        .justify_center()
        .rounded(crate::theme::RADIUS_SM)
        .text_color(if enabled { muted } else { muted.opacity(0.3) })
        .when(enabled, |this| {
            this.hover(|this| this.bg(muted.opacity(0.12)).text_color(fg))
        })
        .child(Icon::new(icon).size(px(14.)))
}

impl TuskApp {
    pub(super) fn grid_tab(&self, ix: usize) -> Option<&DataTab> {
        match self.tabs.get(ix) {
            Some(WorkspaceTab::Grid(g)) => Some(g),
            _ => None,
        }
    }

    pub(super) fn grid_tab_mut(&mut self, ix: usize) -> Option<&mut DataTab> {
        match self.tabs.get_mut(ix) {
            Some(WorkspaceTab::Grid(g)) => Some(g),
            _ => None,
        }
    }

    /// Bottom-bar Data | Structure switch. The structure state is built from
    /// the grid's column metadata the first time it is shown.
    pub(super) fn set_tab_view(
        &mut self,
        ix: usize,
        view: TabView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.grid_tab(ix) else { return };
        // A table still being designed has no rows to show.
        if tab.draft.is_some() && view == TabView::Data {
            return;
        }
        if view == TabView::Index && self.pool.as_ref().is_some_and(|p| !p.caps().indexes) {
            return;
        }
        // Indexes / triggers / DDL need a table that exists.
        if tab.draft.is_some() && matches!(view, TabView::Index | TabView::Triggers | TabView::Ddl)
        {
            return;
        }
        // Triggers / DDL are read fresh every time they open.
        match view {
            TabView::Triggers => self.load_triggers(ix, window, cx),
            TabView::Ddl => self.load_ddl(ix, window, cx),
            _ => {}
        }
        let Some(tab) = self.grid_tab(ix) else { return };
        if view == TabView::Index && tab.indexes.is_none() {
            // Index / structure edits are Postgres DDL: other engines show them read-only.
            let ddl = self.pool.as_ref().is_some_and(|p| p.caps().edit_structure);
            let d = crate::indexes::IndexDelegate::new(
                tab.table.schema.clone(),
                tab.table.name.clone(),
                tab.table.kind == TableKind::Table && ddl,
            );
            let st = crate::indexes::new_state(d, window, cx);
            let sub = cx.subscribe_in(&st, window, |_this, st, ev: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(r, c) = ev {
                    let (r, c) = (*r, *c);
                    st.update(cx, |st, cx| st.delegate_mut().begin_edit(r, c, window, cx));
                }
            });
            if let Some(t) = self.grid_tab_mut(ix) {
                t.indexes = Some(st);
                t.subs.push(sub);
            }
            self.load_indexes(ix, cx);
        }
        if view != TabView::Data {
            self.ensure_rename_input(ix, window, cx);
        }
        let Some(tab) = self.grid_tab(ix) else { return };
        let needs_structure = view == TabView::Structure && tab.structure.is_none();
        if needs_structure {
            // Real column order: a header drag in Data only moves the display.
            let metas = {
                let d = tab.state.read(cx).delegate();
                if d.table_metas.is_empty() {
                    d.metas.clone()
                } else {
                    d.table_metas.clone()
                }
            };
            let ddl = self.pool.as_ref().is_some_and(|p| p.caps().edit_structure);
            let d = StructureDelegate::new(
                tab.table.schema.clone(),
                tab.table.name.clone(),
                tab.table.kind == TableKind::Table && ddl,
                &metas,
            );
            let st = structure::new_state(d, window, cx);
            let sub = cx.subscribe_in(&st, window, |_this, st, ev: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(r, c) = ev {
                    let (r, c) = (*r, *c);
                    st.update(cx, |st, cx| st.delegate_mut().begin_edit(r, c, window, cx));
                }
            });
            if let Some(tab) = self.grid_tab_mut(ix) {
                tab.structure = Some(st);
                tab.subs.push(sub);
            }
            self.load_user_types(ix, cx);
        }
        if let Some(tab) = self.grid_tab_mut(ix) {
            tab.view = view;
        }
        cx.notify();
    }

    /// The Structure / Index header's Name field of an existing table:
    /// leaving it with a different name registers a pending rename.
    pub(super) fn ensure_rename_input(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.grid_tab(ix)
            && tab.draft.is_none()
            && tab.rename.is_none()
            && tab.table.kind == TableKind::Table
        {
            let name = tab.table.name.clone();
            let kind = tab.table.kind.clone();
            let input = cx.new(|cx| {
                let mut st = InputState::new(window, cx);
                st.set_value(name.clone(), window, cx);
                st
            });
            // Leaving the field registers the rename as a pending change.
            let sub = cx.subscribe(&input, move |this, input, ev: &InputEvent, cx| {
                if !matches!(ev, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    return;
                }
                let new = input.read(cx).value().trim().to_string();
                let key = (kind.clone(), name.clone());
                this.pending_renames.retain(|(k, _)| *k != key);
                if !new.is_empty() && new != name {
                    this.pending_renames.push((key, new));
                }
                cx.notify();
            });
            if let Some(t) = self.grid_tab_mut(ix) {
                t.rename = Some(input);
                t.subs.push(sub);
            }
        }
    }

    /// The schema's enums / domains into the data_type picker of a tab.
    pub(super) fn load_user_types(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(pool), Some(tab)) = (self.pool.clone(), self.grid_tab(ix)) else {
            return;
        };
        let Some(st) = tab.structure.clone() else {
            return;
        };
        let schema = tab.table.schema.clone();
        cx.spawn(async move |_, cx: &mut AsyncApp| {
            if let Ok(types) = db::fetch_user_types(&pool, &schema).await {
                st.update(cx, |st, _| st.delegate_mut().user_types = types);
            }
        })
        .detach();
    }

    /// (Re)load the Index tab's rows.
    pub(crate) fn load_indexes(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(pool), Some(tab)) = (self.pool.clone(), self.grid_tab(ix)) else {
            return;
        };
        let (schema, table) = (tab.table.schema.clone(), tab.table.name.clone());
        let Some(st) = tab.indexes.clone() else {
            return;
        };
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let rows = db::fetch_indexes(&pool, &schema, &table).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match rows {
                    Ok(defs) => {
                        let n = defs.len();
                        st.update(cx, |s, cx| {
                            s.delegate_mut().set_indexes(&defs);
                            s.refresh(cx);
                            cx.notify();
                        });
                        if let Some(t) = this.grid_tab_mut(ix) {
                            t.index_count = Some(n);
                        }
                    }
                    Err(e) => this.toast(false, format!("Indexes: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Index view "+ Index": a pending row, its name cell framed for typing.
    fn add_index_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(st) = self.grid_tab(ix).and_then(|t| t.indexes.clone()) else {
            return;
        };
        st.update(cx, |st, cx| {
            let row = st.delegate_mut().add_index();
            st.scroll_to_row(row, cx);
            st.set_selected_cell(row, 0, cx);
            st.focus_handle(cx).focus(window, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// "+ Column" in the Structure view.
    pub(super) fn add_structure_column(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(st) = self.grid_tab(ix).and_then(|t| t.structure.clone()) else {
            return;
        };
        st.update(cx, |st, cx| {
            let row = st.delegate_mut().add_column();
            st.scroll_to_row(row, cx);
            // The new row's name cell is framed and the grid has focus:
            // Enter edits it right away.
            st.set_selected_cell(row, 0, cx);
            st.focus_handle(cx).focus(window, cx);
            cx.notify();
        });
        cx.notify();
    }

    // ---------- filters ----------

    /// ⌘F / "Filters": show or hide the filter bar of the active grid tab.
    pub(super) fn toggle_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // ⌘F in the sidebar filters the object list instead.
        if self.sidebar_focus.is_focused(window) {
            self.open_sidebar_filter(window, cx);
            return;
        }
        let Some(ix) = self.active_tab else { return };
        let Some(tab) = self.grid_tab_mut(ix) else {
            return;
        };
        if tab.view != TabView::Data {
            return;
        }
        tab.filters.visible = !tab.filters.visible;
        let open_empty = tab.filters.visible && tab.filters.rows.is_empty();
        if open_empty {
            self.add_filter_row(ix, None, window, cx);
        }
        if let Some(tab) = self.grid_tab(ix)
            && tab.filters.visible
            && let Some(row) = tab.filters.rows.first()
        {
            row.value.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Insert a filter row after `after` (or at the end).
    pub(super) fn add_filter_row(
        &mut self,
        ix: usize,
        after: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let columns: Vec<String> = self
            .grid_tab(ix)
            .map(|t| {
                t.state
                    .read(cx)
                    .delegate()
                    .metas
                    .iter()
                    .map(|m| m.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Settings ▸ filter menu ▸ Default Filter Column.
        let default_col = match crate::settings::get().filter_default_column.as_str() {
            _ if columns.is_empty() => None,
            "Raw SQL" => None,
            "Primary key" => self
                .grid_tab(ix)
                .and_then(|t| {
                    t.state
                        .read(cx)
                        .delegate()
                        .metas
                        .iter()
                        .position(|m| m.is_pk)
                })
                .or(Some(0)),
            _ => Some(0),
        };
        let row = FilterRow::new(default_col, &columns, window, cx);
        let value_id = row.value.entity_id();
        // Enter in a value field applies every filter (Apply All).
        let sub = cx.subscribe_in(
            &row.value,
            window,
            move |this, _, ev: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.apply_filters(ix, cx);
                }
            },
        );
        let cols = columns.clone();
        let col_sub = cx.subscribe_in(
            &row.column_select,
            window,
            move |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, window, cx| {
                let SelectEvent::Confirm(Some(v)) = ev else {
                    return;
                };
                if let Some(tab) = this.grid_tab_mut(ix)
                    && let Some(r) = tab
                        .filters
                        .rows
                        .iter_mut()
                        .find(|r| r.value.entity_id() == value_id)
                {
                    r.set_column_label(v, &cols);
                    let input = r.value.clone();
                    input.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
            },
        );
        let op_sub = cx.subscribe_in(
            &row.op_select,
            window,
            move |this, _, ev: &SelectEvent<Vec<SharedString>>, window, cx| {
                let SelectEvent::Confirm(Some(v)) = ev else {
                    return;
                };
                if let Some(tab) = this.grid_tab_mut(ix)
                    && let Some(r) = tab
                        .filters
                        .rows
                        .iter_mut()
                        .find(|r| r.value.entity_id() == value_id)
                {
                    r.set_op_label(v);
                    let input = r.value.clone();
                    input.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
            },
        );
        if let Some(tab) = self.grid_tab_mut(ix) {
            tab.subs.push(col_sub);
            tab.subs.push(op_sub);
        }
        if let Some(tab) = self.grid_tab_mut(ix) {
            let at = after.map(|a| a + 1).unwrap_or(tab.filters.rows.len());
            tab.filters.rows.insert(at.min(tab.filters.rows.len()), row);
            tab.subs.push(sub);
        }
        cx.notify();
    }

    /// Filter bar "Export": the Export window for this table, limited to the
    /// rows matching the enabled filters (applied or not yet).
    fn export_filtered(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let (kind, name) = (tab.table.kind.clone(), tab.table.name.clone());
        self.apply_filters(ix, cx);
        self.export_object(kind, name, window, cx);
    }

    /// Filter bar "SQL": `SELECT * FROM t WHERE <filters>` in a new query tab.
    fn filter_as_sql(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let specs: Vec<_> = tab.filters.rows.iter().map(|r| r.spec(cx)).collect();
        let d = tab.state.read(cx).delegate();
        let clause = crate::filter::build_where_for(&specs, &d.metas, d.pool.dialect());
        let mut sql = format!(
            "SELECT * FROM {}.{}",
            db::quote_ident(&tab.table.schema),
            db::quote_ident(&tab.table.name)
        );
        if let Some(w) = clause {
            sql.push_str(&format!("\nWHERE {}", crate::filter::display_sql(&w)));
        }
        sql.push_str("\nLIMIT 300;");
        self.open_sql_tab_with(Some(sql), window, cx);
    }

    /// Filter bar ☰: defaults for new filter rows and the table order,
    /// each a submenu with the current choice checked.
    fn filter_defaults_menu(muted: gpui::Hsla) -> AnyElement {
        use crate::settings::{self as st, Prefs};
        fn choices(
            m: PopupMenu,
            options: Vec<String>,
            current: String,
            set: fn(&mut Prefs, String),
        ) -> PopupMenu {
            options.into_iter().fold(m, |m, o| {
                let checked = o == current;
                let v = o.clone();
                m.item(
                    PopupMenuItem::new(o)
                        .checked(checked)
                        .on_click(move |_, _, cx| {
                            let v = v.clone();
                            st::update(cx, |p| set(p, v));
                        }),
                )
            })
        }
        Button::new("flt-defaults")
            .ghost()
            .xsmall()
            // Same 16px box as the row checkboxes above: the icon sits on
            // their axis.
            .child(
                div()
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::SlidersHorizontal)
                            .size(px(14.))
                            .text_color(muted),
                    ),
            )
            .dropdown_menu(|menu, window, cx| {
                let p = st::get();
                let strs = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
                let ops: Vec<String> = crate::filter::FilterOp::ALL
                    .iter()
                    .map(|o| o.label().to_string())
                    .collect();
                let (cs, dc, dop, ts) = (
                    p.filter_column_sort.clone(),
                    p.filter_default_column.clone(),
                    p.filter_default_operator.clone(),
                    p.default_table_sort.clone(),
                );
                let state = if p.filter_default_enabled {
                    "Enabled"
                } else {
                    "Disabled"
                }
                .to_string();
                menu.submenu("Default Filter Column Sort", window, cx, move |m, _, _| {
                    choices(m, strs(&st::FILTER_COLUMN_SORTS), cs.clone(), |p, v| {
                        p.filter_column_sort = v
                    })
                })
                .submenu("Default Filter Column", window, cx, move |m, _, _| {
                    choices(m, strs(&st::FILTER_DEFAULT_COLUMNS), dc.clone(), |p, v| {
                        p.filter_default_column = v
                    })
                })
                .submenu("Default Filter Operator", window, cx, move |m, _, _| {
                    choices(m, ops.clone(), dop.clone(), |p, v| {
                        p.filter_default_operator = v
                    })
                })
                .submenu("Default Filter State", window, cx, move |m, _, _| {
                    choices(
                        m,
                        vec!["Enabled".into(), "Disabled".into()],
                        state.clone(),
                        |p, v| p.filter_default_enabled = v == "Enabled",
                    )
                })
                .separator()
                .submenu("Default Table Sort", window, cx, move |m, _, _| {
                    choices(m, strs(&st::TABLE_SORTS), ts.clone(), |p, v| {
                        p.default_table_sort = v
                    })
                })
            })
            .into_any_element()
    }

    /// The filter row whose value field has focus.
    fn focused_filter_row(&self, ix: usize, window: &Window, cx: &App) -> Option<usize> {
        self.grid_tab(ix)?
            .filters
            .rows
            .iter()
            .position(|r| r.value.read(cx).focus_handle(cx).is_focused(window))
    }

    fn focus_filter_row(
        &mut self,
        ix: usize,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.grid_tab(ix) {
            let n = tab.filters.rows.len();
            if n > 0 {
                let input = tab.filters.rows[row.min(n - 1)].value.clone();
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
        }
        cx.notify();
    }

    /// ‹ / › (⌘← / ⌘→): previous / next page of the grid.
    pub(crate) fn step_page(
        &mut self,
        ix: usize,
        dir: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let state = tab.state.clone();
        let moved = state.update(cx, |st, _| {
            let d = st.delegate_mut();
            let next = (d.page_offset + dir * d.page_limit).max(0);
            let in_range = d.total_rows().is_none_or(|n| next < n);
            if next == d.page_offset || !in_range {
                return None;
            }
            d.page_offset = next;
            Some((d.page_limit, next))
        });
        if let Some((limit, offset)) = moved {
            self.sync_page_inputs(ix, limit, offset, window, cx);
            grid::reload(&state, cx);
            cx.notify();
        }
    }

    /// Page popover "Go": apply the Limit / Offset fields.
    pub(crate) fn apply_page_inputs(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let parse =
            |e: &Entity<InputState>, cx: &App| e.read(cx).value().trim().parse::<i64>().ok();
        let limit = parse(&tab.page_limit_input, cx)
            .filter(|n| *n > 0)
            .unwrap_or(grid::PAGE_LIMIT);
        let offset = parse(&tab.page_offset_input, cx).unwrap_or(0).max(0);
        let state = tab.state.clone();
        state.update(cx, |st, _| {
            let d = st.delegate_mut();
            d.page_limit = limit.min(100_000);
            d.page_offset = offset;
        });
        grid::reload(&state, cx);
        cx.notify();
    }

    fn sync_page_inputs(
        &mut self,
        ix: usize,
        limit: i64,
        offset: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let (l, o) = (tab.page_limit_input.clone(), tab.page_offset_input.clone());
        l.update(cx, |s, cx| s.set_value(limit.to_string(), window, cx));
        o.update(cx, |s, cx| s.set_value(offset.to_string(), window, cx));
    }

    /// Grid menu "Filter ▸ …": replace the active tab's filters with one
    /// `column <op> value` row and apply it.
    pub fn quick_filter(
        &mut self,
        col_ix: usize,
        op: crate::filter::FilterOp,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.active_tab else { return };
        let Some(tab) = self.grid_tab_mut(ix) else {
            return;
        };
        tab.filters.rows.clear();
        tab.filters.visible = true;
        self.add_filter_row(ix, None, window, cx);
        let columns: Vec<String> = self
            .grid_tab(ix)
            .map(|t| {
                t.state
                    .read(cx)
                    .delegate()
                    .metas
                    .iter()
                    .map(|m| m.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(tab) = self.grid_tab_mut(ix)
            && let Some(row) = tab.filters.rows.first_mut()
        {
            row.column = Some(col_ix);
            row.op = op;
            row.sync_pickers(&columns, window, cx);
            let input = row.value.clone();
            input.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        self.apply_filters(ix, cx);
    }

    fn remove_filter_row(&mut self, ix: usize, row: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.grid_tab_mut(ix)
            && row < tab.filters.rows.len()
        {
            tab.filters.rows.remove(row);
        }
        if self.grid_tab(ix).is_some_and(|t| t.filters.rows.is_empty()) {
            self.clear_filters(ix, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn apply_filters(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let specs: Vec<_> = tab.filters.rows.iter().map(|r| r.spec(cx)).collect();
        let state = tab.state.clone();
        let clause = {
            let d = state.read(cx).delegate();
            crate::filter::build_where_for(&specs, &d.metas, d.pool.dialect())
        };
        if let Some(tab) = self.grid_tab_mut(ix) {
            tab.filters.applied = clause.clone();
        }
        state.update(cx, |st, _| {
            let d = st.delegate_mut();
            d.filter = clause;
            d.page_offset = 0;
        });
        grid::reload(&state, cx);
        cx.notify();
    }

    fn clear_filters(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.grid_tab_mut(ix) else {
            return;
        };
        tab.filters.rows.clear();
        tab.filters.visible = false;
        let had = tab.filters.applied.take().is_some();
        let state = tab.state.clone();
        if had {
            state.update(cx, |st, _| st.delegate_mut().filter = None);
            grid::reload(&state, cx);
        }
        cx.notify();
    }

    // ---------- sidebar: drop / rename ----------

    pub(super) fn is_pending_drop(&self, kind: &TableKind, name: &str) -> bool {
        self.pending_drops
            .iter()
            .any(|(k, n)| k == kind && n == name)
    }

    pub(super) fn pending_rename(&self, kind: &TableKind, name: &str) -> Option<&String> {
        self.pending_renames
            .iter()
            .find(|((k, n), _)| k == kind && n == name)
            .map(|(_, new)| new)
    }

    pub(crate) fn toggle_drop(&mut self, kind: TableKind, name: String, cx: &mut Context<Self>) {
        let before = self.sidebar_pending();
        if let Some(pos) = self
            .pending_drops
            .iter()
            .position(|(k, n)| *k == kind && *n == name)
        {
            self.pending_drops.remove(pos);
        } else {
            self.pending_drops.push((kind, name));
        }
        let after = self.sidebar_pending();
        self.sidebar_history.record(before, &after);
        cx.notify();
    }

    fn sidebar_pending(&self) -> SidebarPending {
        (self.pending_drops.clone(), self.pending_renames.clone())
    }

    /// ⌘Z / ⇧⌘Z outside text fields: step through the pending changes of
    /// what has focus — the sidebar, else the active tab (structure view,
    /// data grid or query result).
    pub(crate) fn undo_changes(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let done = if self.sidebar_focus.is_focused(window) {
            let current = self.sidebar_pending();
            let target = if redo {
                self.sidebar_history.redo(current)
            } else {
                self.sidebar_history.undo(current)
            };
            match target {
                Some((drops, renames)) => {
                    self.pending_drops = drops;
                    self.pending_renames = renames;
                    true
                }
                None => false,
            }
        } else {
            match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
                Some(WorkspaceTab::Grid(g)) => match (&g.structure, g.view) {
                    (_, TabView::Index) if g.indexes.is_some() => {
                        g.indexes.as_ref().is_some_and(|st| {
                            st.update(cx, |s, cx| {
                                let ok = s.delegate_mut().undo(redo);
                                cx.notify();
                                ok
                            })
                        })
                    }
                    (Some(st), TabView::Structure) => st.update(cx, |s, cx| {
                        let ok = s.delegate_mut().undo(redo);
                        cx.notify();
                        ok
                    }),
                    _ => g.state.update(cx, |s, cx| {
                        let ok = s.delegate_mut().undo(redo);
                        cx.notify();
                        ok
                    }),
                },
                Some(WorkspaceTab::Sql(t)) => t.result.update(cx, |s, cx| {
                    let ok = s.delegate_mut().undo(redo);
                    cx.notify();
                    ok
                }),
                None => false,
            }
        };
        if !done {
            self.toast_info(if redo {
                "Nothing to redo."
            } else {
                "Nothing to undo."
            });
        }
        cx.notify();
    }

    pub(crate) fn start_rename(
        &mut self,
        kind: TableKind,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .pending_rename(&kind, &name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(current, window, cx);
            st
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    this.commit_rename(cx);
                    this.sidebar_focus.focus(window, cx);
                }
                InputEvent::Blur => this.commit_rename(cx),
                _ => {}
            },
        );
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.renaming = Some(RenameState {
            kind,
            name,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self.renaming.take() else {
            return;
        };
        let new = r.input.read(cx).value().trim().to_string();
        let before = self.sidebar_pending();
        self.pending_renames
            .retain(|((k, n), _)| !(*k == r.kind && *n == r.name));
        if !new.is_empty() && new != r.name {
            self.pending_renames.push(((r.kind, r.name), new));
        }
        let after = self.sidebar_pending();
        self.sidebar_history.record(before, &after);
        cx.notify();
    }

    // ---------- keyboard ----------

    /// Delete / Backspace: sidebar object → pending DROP; grid row → pending
    /// DELETE; structure column → pending DROP COLUMN. All red until ⌘S.
    pub(super) fn delete_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_focus.is_focused(window) {
            if let Some((kind, name)) = self.selected_object.clone() {
                self.toggle_drop(kind, name, cx);
            }
            return;
        }
        if let Some(WorkspaceTab::Sql(t)) = self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            t.result.update(cx, |st, cx| {
                let row = st.selected_row().or(st.selected_cell().map(|c| c.0));
                if let Some(row) = row {
                    st.delegate_mut().toggle_delete(row);
                    cx.notify();
                }
            });
            cx.notify();
            return;
        }
        let Some(tab) = self.active_tab.and_then(|ix| self.grid_tab(ix)) else {
            return;
        };
        match (tab.view, tab.structure.clone(), tab.indexes.clone()) {
            (TabView::Index, _, Some(st)) => st.update(cx, |st, cx| {
                let row = st.selected_row().or(st.selected_cell().map(|c| c.0));
                if let Some(row) = row {
                    st.delegate_mut().toggle_delete(row);
                    cx.notify();
                }
            }),
            (TabView::Structure, Some(st), _) => st.update(cx, |st, cx| {
                let row = st.selected_row().or(st.selected_cell().map(|c| c.0));
                if let Some(row) = row {
                    st.delegate_mut().toggle_delete(row);
                    cx.notify();
                }
            }),
            _ => tab.state.update(cx, |st, cx| {
                let row = st.selected_row().or(st.selected_cell().map(|c| c.0));
                if let Some(row) = row {
                    st.delegate_mut().toggle_delete(row);
                    cx.notify();
                }
            }),
        }
        cx.notify();
    }

    /// Keyboard focus back on the active tab's visible table (after its
    /// cell editor or a picker closed), so ⌘S / arrows work without a click.
    pub(super) fn focus_active_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Sql(t)) => Some(t.result.read(cx).focus_handle(cx)),
            Some(WorkspaceTab::Grid(g)) => match g.view {
                TabView::Structure => g.structure.as_ref().map(|s| s.read(cx).focus_handle(cx)),
                TabView::Index => g.indexes.as_ref().map(|s| s.read(cx).focus_handle(cx)),
                _ => Some(g.state.read(cx).focus_handle(cx)),
            },
            None => None,
        };
        if let Some(h) = handle {
            h.focus(window, cx);
        }
    }

    /// Esc: close an open cell/rename editor without keeping its value.
    pub(super) fn cancel_editors(&mut self, cx: &mut Context<Self>) -> bool {
        if self.renaming.take().is_some() {
            cx.notify();
            return true;
        }
        if let Some(WorkspaceTab::Sql(t)) = self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            let result = t.result.clone();
            return result.update(cx, |st, cx| st.delegate_mut().cancel_edit(cx));
        }
        let Some(tab) = self.active_tab.and_then(|ix| self.grid_tab(ix)) else {
            return false;
        };
        let mut cancelled = tab
            .state
            .update(cx, |st, cx| st.delegate_mut().cancel_edit(cx));
        if let Some(st) = tab.structure.clone() {
            cancelled |= st.update(cx, |st, cx| st.delegate_mut().cancel_edit(cx));
        }
        if let Some(st) = tab.indexes.clone() {
            cancelled |= st.update(cx, |st, cx| st.delegate_mut().cancel_edit(cx));
        }
        cancelled
    }

    /// Unsaved changes across the sidebar and the active tab, for the status bar.
    /// Unsaved grid / structure / result edits held by one tab.
    pub(super) fn tab_pending(&self, ix: usize, cx: &App) -> usize {
        match self.tabs.get(ix) {
            Some(WorkspaceTab::Sql(t)) => {
                t.result.read(cx).delegate().pending_count() + usize::from(t.view_draft.is_some())
            }
            Some(WorkspaceTab::Grid(g)) => {
                g.state.read(cx).delegate().pending_count()
                    + g.structure
                        .as_ref()
                        .map_or(0, |st| st.read(cx).delegate().pending_count())
                    + g.indexes
                        .as_ref()
                        .map_or(0, |st| st.read(cx).delegate().pending_count())
            }
            None => 0,
        }
    }

    pub(super) fn pending_summary(&self, cx: &App) -> Option<String> {
        // tab_pending also counts a New View draft, so safe mode asks before
        // ⌘S creates the view instead of finding "nothing to save".
        let n = self.pending_drops.len()
            + self.pending_renames.len()
            + self.active_tab.map_or(0, |ix| self.tab_pending(ix, cx));
        (n > 0).then(|| format!("{n} unsaved change{}", if n == 1 { "" } else { "s" }))
    }

    // ---------- ⌘S ----------

    /// Commit sidebar drops/renames + the active tab's grid and structure
    /// changes in ONE transaction; on success reload what changed.
    pub(super) fn save_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        // Safe mode: ask first (then run this again with the answer).
        if crate::settings::get().confirm_save && !self.save_confirmed {
            let Some(summary) = self.pending_summary(cx) else {
                return;
            };
            let answer = window.prompt(
                PromptLevel::Info,
                "Save changes to the database?",
                Some(&summary),
                &["Save", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                if answer.await == Ok(0) {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.save_confirmed = true;
                        this.save_changes(window, cx);
                    });
                }
            })
            .detach();
            return;
        }
        self.save_confirmed = false;
        let Some(pool) = self.pool.clone() else {
            return;
        };
        self.commit_rename(cx);
        // Header Name field of the active table: a different name is a rename.
        if let Some(tab) = self.active_tab.and_then(|ix| self.grid_tab(ix))
            && let Some(input) = &tab.rename
        {
            let new = input.read(cx).value().trim().to_string();
            let key = (tab.table.kind.clone(), tab.table.name.clone());
            if !new.is_empty() && new != tab.table.name {
                self.pending_renames.retain(|(k, _)| *k != key);
                self.pending_renames.push((key, new));
            }
        }
        // New columns need a name and a type before anything runs.
        if let Some(st) = self
            .active_tab
            .and_then(|ix| self.grid_tab(ix))
            .and_then(|t| t.structure.clone())
        {
            st.update(cx, |st, cx| st.delegate_mut().commit_edit(cx));
            if let Some(e) = st.read(cx).delegate().validation_error() {
                self.toast(false, e);
                cx.notify();
                return;
            }
        }
        let schema = self.current_schema.clone();
        let mut stmts: Vec<Stmt> = Vec::new();
        let drops = self.pending_drops.clone();
        let renames = self.pending_renames.clone();
        let engine = db::engine();
        for (kind, name) in &drops {
            stmts.push(Stmt::plain(format!(
                "DROP {} {}",
                kind.ddl_keyword(),
                crate::ddl::target(engine, &schema, name)
            )));
        }
        for ((kind, old), new) in &renames {
            if drops.iter().any(|(k, n)| k == kind && n == old) {
                continue;
            }
            match crate::ddl::rename_object(engine, kind.ddl_keyword(), &schema, old, new) {
                Ok(sql) => stmts.push(Stmt::plain(sql)),
                Err(e) => {
                    self.toast(false, e);
                    cx.notify();
                    return;
                }
            }
        }
        let tab_ix = self.active_tab;
        // New Table draft: its name field names the table before CREATE.
        let mut created_draft = false;
        if let Some(ix) = tab_ix
            && let Some(input) = self.grid_tab(ix).and_then(|t| t.draft.clone())
        {
            let name = input.read(cx).value().trim().to_string();
            if name.is_empty() {
                self.toast(false, "Name the new table first.");
                cx.notify();
                return;
            }
            if let Some(tab) = self.grid_tab_mut(ix) {
                tab.table.name = name.clone();
                let (state, st) = (tab.state.clone(), tab.structure.clone());
                state.update(cx, |s, _| s.delegate_mut().table = name.clone());
                if let Some(st) = st {
                    st.update(cx, |s, _| s.delegate_mut().table = name.clone());
                }
            }
            created_draft = true;
        }
        // New View draft: CREATE VIEW <name> AS <editor text>.
        let mut view_stmt: Option<(usize, Stmt)> = None;
        let mut view_name: Option<String> = None;
        if let Some(ix) = tab_ix
            && let Some(WorkspaceTab::Sql(t)) = self.tabs.get(ix)
            && let Some(input) = &t.view_draft
        {
            let name = input.read(cx).value().trim().to_string();
            let body = t.editor.read(cx).text().to_string();
            let body = body.trim().trim_end_matches(';').trim().to_string();
            if name.is_empty() || body.is_empty() {
                self.toast(false, "Name the view and write its query first.");
                cx.notify();
                return;
            }
            view_name = Some(name.clone());
            view_stmt = Some((
                ix,
                Stmt::plain(format!(
                    "CREATE VIEW {} AS\n{body}",
                    crate::ddl::target(engine, &schema, &name)
                )),
            ));
        }
        if let Some((_, st)) = &view_stmt {
            stmts.push(st.clone());
        }
        let (grid_state, struct_state, index_state, table) =
            match tab_ix.and_then(|ix| self.grid_tab(ix)) {
                Some(tab) => (
                    Some(tab.state.clone()),
                    tab.structure.clone(),
                    tab.indexes.clone(),
                    Some(tab.table.clone()),
                ),
                None => (None, None, None, None),
            };
        if let Some(st) = &grid_state {
            st.update(cx, |st, cx| st.delegate_mut().commit_edit(cx));
            stmts.extend(st.read(cx).delegate().save_statements());
        }
        if let Some(st) = &struct_state {
            st.update(cx, |st, cx| st.delegate_mut().commit_edit(cx));
            stmts.extend(st.read(cx).delegate().save_statements());
        }
        if let Some(st) = &index_state {
            st.update(cx, |st, cx| st.delegate_mut().commit_edit(cx));
            if let Some(e) = st.read(cx).delegate().validation_error() {
                self.toast(false, e);
                cx.notify();
                return;
            }
            stmts.extend(st.read(cx).delegate().save_statements());
        }
        // SQL tab: write edited query results back by primary key.
        let sql_result = match tab_ix.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Sql(t)) => Some(t.result.clone()),
            _ => None,
        };
        if let Some(st) = &sql_result {
            st.update(cx, |st, cx| st.delegate_mut().commit_edit(cx));
            stmts.extend(st.read(cx).delegate().save_statements());
        }
        if stmts.is_empty() {
            self.toast_info("No changes to save.");
            cx.notify();
            return;
        }
        self.saving = true;
        self.status_line = format!(
            "Saving {} statement{}…",
            stmts.len(),
            if stmts.len() == 1 { "" } else { "s" }
        );
        cx.notify();
        let n_stmts = stmts.len();
        let struct_changed = struct_state
            .as_ref()
            .is_some_and(|s| s.read(cx).delegate().has_changes());
        cx.spawn_in(window, async move |this, cx| {
            let t0 = std::time::Instant::now();
            let result = db::execute_batch(&pool, stmts).await;
            let ms = t0.elapsed().as_millis();
            // Structure changed → re-read the columns before rebuilding the view.
            let metas = match (&result, struct_changed, &table) {
                (Ok(_), true, Some(t)) => db::fetch_columns(&pool, &t.schema, &t.name).await.ok(),
                _ => None,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(affected) => {
                        this.after_save(&drops, &renames, window, cx);
                        // A created table / view is now a real object.
                        if created_draft || view_stmt.is_some() {
                            if let Some(ix) = tab_ix {
                                if let Some(t) = this.grid_tab_mut(ix) {
                                    t.draft = None;
                                }
                                this.ensure_rename_input(ix, window, cx);
                                if let Some(WorkspaceTab::Sql(t)) = this.tabs.get_mut(ix) {
                                    t.view_draft = None;
                                    // The draft tab now edits a real view: name it so.
                                    if let Some(name) = &view_name {
                                        t.title = name.clone();
                                    }
                                }
                            }
                            let schema = this.current_schema.clone();
                            this.fetch_objects_for(&schema, cx);
                        }
                        if let Some(st) = &grid_state {
                            st.update(cx, |st, _| st.delegate_mut().discard_changes());
                            grid::reload(st, cx);
                        }
                        if let (Some(_), Some(ix)) = (&index_state, tab_ix) {
                            this.load_indexes(ix, cx);
                        }
                        if let (Some(st), Some(ix)) = (&sql_result, tab_ix) {
                            st.update(cx, |st, _| st.delegate_mut().discard_changes());
                            // Re-run so the grid shows what the database now holds.
                            this.run_sql_in_tab(ix, crate::sql::RunScope::Last, cx);
                        }
                        if let (Some(ix), Some(metas), Some(t)) = (tab_ix, metas, &table) {
                            let ddl = this.pool.as_ref().is_some_and(|p| p.caps().edit_structure);
                            let d = StructureDelegate::new(
                                t.schema.clone(),
                                t.name.clone(),
                                t.kind == TableKind::Table && ddl,
                                &metas,
                            );
                            if let Some(st) = this.grid_tab(ix).and_then(|t| t.structure.clone()) {
                                st.update(cx, |st, cx| {
                                    *st.delegate_mut() = d;
                                    st.refresh(cx);
                                    cx.notify();
                                });
                            }
                        }
                        this.status_line.clear();
                        this.toast(
                            true,
                            format!(
                                "Saved — {n_stmts} statement{}, {} affected · {ms} ms",
                                if n_stmts == 1 { "" } else { "s" },
                                crate::sql::n_rows(affected as usize)
                            ),
                        );
                    }
                    Err(e) => {
                        this.status_line.clear();
                        this.toast(false, format!("Save failed (rolled back): {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a successful sidebar save locally: close tabs of dropped objects,
    /// relabel renamed ones, then re-list the schema.
    fn after_save(
        &mut self,
        drops: &[(TableKind, String)],
        renames: &[((TableKind, String), String)],
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if drops.is_empty() && renames.is_empty() {
            return;
        }
        let schema = self.current_schema.clone();
        let mut ix = 0;
        while ix < self.tabs.len() {
            let dropped = matches!(&self.tabs[ix], WorkspaceTab::Grid(g)
                if g.table.schema == schema && drops.iter().any(|(k, n)| *k == g.table.kind && *n == g.table.name));
            if dropped {
                self.close_tab_at(ix, cx);
            } else {
                ix += 1;
            }
        }
        for tab in &mut self.tabs {
            if let WorkspaceTab::Grid(g) = tab
                && g.table.schema == schema
                && let Some((_, new)) = renames
                    .iter()
                    .find(|((k, n), _)| *k == g.table.kind && *n == g.table.name)
            {
                g.table.name = new.clone();
                g.state
                    .update(cx, |st, _| st.delegate_mut().table = new.clone());
                if let Some(s) = &g.structure {
                    s.update(cx, |st, _| st.delegate_mut().table = new.clone());
                }
            }
        }
        self.pending_drops.clear();
        self.pending_renames.clear();
        self.selected_object = None;
        self.fetch_objects_for(&schema, cx);
    }

    // ---------- rendering ----------

    /// Filter bar: one row per filter + an actions row.
    pub(super) fn render_filter_bar(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.grid_tab(ix) else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (border, muted) = (t.border, t.muted_foreground);
        let mut bar = div()
            .key_context("FilterBar")
            .on_action(cx.listener(move |this, _: &FilterInsert, w, cx| {
                let at = this.focused_filter_row(ix, w, cx);
                this.add_filter_row(ix, at, w, cx);
                this.focus_filter_row(ix, at.map_or(0, |r| r + 1), w, cx);
            }))
            .on_action(cx.listener(move |this, _: &FilterRemove, w, cx| {
                if let Some(r) = this.focused_filter_row(ix, w, cx) {
                    this.remove_filter_row(ix, r, cx);
                    this.focus_filter_row(ix, r.saturating_sub(1), w, cx);
                }
            }))
            .on_action(
                cx.listener(move |this, _: &FilterApplyAll, _, cx| this.apply_filters(ix, cx)),
            )
            .on_action(cx.listener(move |this, _: &FilterUp, w, cx| {
                if let Some(r) = this.focused_filter_row(ix, w, cx) {
                    this.focus_filter_row(ix, r.saturating_sub(1), w, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &FilterDown, w, cx| {
                if let Some(r) = this.focused_filter_row(ix, w, cx) {
                    this.focus_filter_row(ix, r + 1, w, cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &FilterColumns, w, cx| {
                if let Some(r) = this.focused_filter_row(ix, w, cx)
                    && let Some(sel) = this
                        .grid_tab(ix)
                        .and_then(|t| t.filters.rows.get(r))
                        .map(|r| r.column_select.clone())
                {
                    sel.update(cx, |s, cx| s.open_menu(w, cx));
                }
            }))
            .on_action(cx.listener(move |this, _: &FilterOperators, w, cx| {
                if let Some(r) = this.focused_filter_row(ix, w, cx)
                    && let Some(sel) = this
                        .grid_tab(ix)
                        .and_then(|t| t.filters.rows.get(r))
                        .map(|r| r.op_select.clone())
                {
                    sel.update(cx, |s, cx| s.open_menu(w, cx));
                }
            }))
            .on_action(cx.listener(move |this, _: &FilterToggle, w, cx| {
                if let Some(r) = this.focused_filter_row(ix, w, cx)
                    && let Some(row) = this
                        .grid_tab_mut(ix)
                        .and_then(|t| t.filters.rows.get_mut(r))
                {
                    row.enabled = !row.enabled;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(move |this, _: &FilterExit, w, cx| {
                if let Some(t) = this.grid_tab_mut(ix) {
                    t.filters.visible = false;
                }
                this.focus.focus(w, cx);
                cx.notify();
            }))
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .bg(t.colors.title_bar);
        for (row_ix, row) in tab.filters.rows.iter().enumerate() {
            let column_menu = Select::new(&row.column_select).small().menu_width(px(240.));
            let op_menu = Select::new(&row.op_select).small().menu_width(px(160.));
            let enabled = row.enabled;
            bar = bar.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(Checkbox::new(("flt-on", row_ix)).checked(enabled).on_click(
                        move |checked, _, cx| {
                            let checked = *checked;
                            let view = cx.global::<TuskHandle>().0.clone();
                            view.update(cx, |this, cx| {
                                if let Some(tab) = this.grid_tab_mut(ix)
                                    && let Some(r) = tab.filters.rows.get_mut(row_ix)
                                {
                                    r.enabled = checked;
                                }
                                cx.notify();
                            });
                        },
                    ))
                    .child(div().w(px(170.)).child(column_menu))
                    .when(row.column.is_some(), |this| {
                        this.child(div().w(px(120.)).child(op_menu))
                    })
                    .child(
                        div().flex_1().child(
                            Input::new(&row.value)
                                .small()
                                .font_family(crate::settings::ui_font())
                                .disabled(row.column.is_some() && !row.op.needs_value()),
                        ),
                    )
                    .child(
                        Button::new(("flt-apply", row_ix))
                            .label("Apply")
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_filters(ix, cx);
                            })),
                    )
                    .child(
                        pill(("flt-del", row_ix), "−", false, cx).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.remove_filter_row(ix, row_ix, cx);
                            },
                        )),
                    )
                    .child(
                        pill(("flt-add", row_ix), "+", false, cx).on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.add_filter_row(ix, Some(row_ix), window, cx);
                            },
                        )),
                    ),
            );
        }
        // The few keys worth knowing, each label right before its caps.
        let hints = [
            ("Apply", "cmd-enter"),
            ("Add", "cmd-i"),
            ("Remove", "cmd-shift-i"),
            ("Close", "escape"),
        ];
        let hint_row = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .gap_5()
            .children(hints.map(|(label, key)| {
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_caption()
                    .child(div().text_color(muted.opacity(0.7)).child(label))
                    .child(crate::kbd::caps(key))
            }));
        bar.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .pt_1()
                .child(Self::filter_defaults_menu(muted))
                // Export the matching rows / open the filter as a SELECT.
                .child(pill("flt-export", "Export", false, cx).on_click(
                    cx.listener(move |this, _, window, cx| this.export_filtered(ix, window, cx)),
                ))
                .child(pill("flt-sql", "SQL", false, cx).on_click(
                    cx.listener(move |this, _, window, cx| this.filter_as_sql(ix, window, cx)),
                ))
                .child(div().w(px(8.)))
                .child(hint_row)
                .child(div().w(px(8.)))
                .child(
                    pill("flt-clear", "Clear", false, cx)
                        .on_click(cx.listener(move |this, _, _, cx| this.clear_filters(ix, cx))),
                )
                .child(
                    Button::new("flt-apply-all")
                        .label("Apply All")
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.apply_filters(ix, cx))),
                ),
        )
        .into_any_element()
    }

    /// Grid bottom bar: `Data | Structure`, + Column, row range, Filters.
    /// The tab's toolbar — view switcher (Data / Structure / Index), add
    /// buttons, row count, filters and paging. Above the table, below the
    /// tab strip.
    pub(super) fn render_tab_toolbar(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.grid_tab(ix) else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (border, muted, fg) = (t.border, t.muted_foreground, t.foreground);
        let view = tab.view;
        let seg = |id: &'static str, label: &'static str, v: TabView, cx: &mut Context<Self>| {
            let active = view == v;
            div()
                .id(id)
                .px_3()
                .h_full()
                .flex()
                .items_center()
                .text_caption()
                .text_color(if active { fg } else { muted })
                .when(active, |this| this.bg(muted.opacity(0.18)))
                .hover(|this| this.bg(muted.opacity(0.08)))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_tab_view(ix, v, window, cx);
                }))
        };
        let d = tab.state.read(cx).delegate();
        let editable = d.editable;
        let center = match view {
            TabView::Data => match (d.total_rows(), d.page_len()) {
                (Some(0), _) => "0 rows".to_string(),
                (Some(n), Some(len)) => {
                    let filtered = if d.filter.is_some() {
                        " (filtered)"
                    } else {
                        ""
                    };
                    let rows = if n == 1 { "row" } else { "rows" };
                    // Every row on screen: just the count, no range.
                    if d.page_offset == 0 && len >= n {
                        format!("{} {rows}{filtered}", super::fmt_int(n))
                    } else {
                        format!(
                            "{}–{} of {} {rows}{filtered}",
                            super::fmt_int(d.page_offset + 1),
                            super::fmt_int(d.page_offset + len),
                            super::fmt_int(n),
                        )
                    }
                }
                _ => "…".to_string(),
            },
            TabView::Structure => {
                let n = tab
                    .structure
                    .as_ref()
                    .map_or(d.metas.len(), |st| st.read(cx).delegate().rows.len());
                format!("{n} column{}", if n == 1 { "" } else { "s" })
            }
            TabView::Index => match tab.index_count {
                Some(n) => format!("{n} index{}", if n == 1 { "" } else { "es" }),
                None => "…".into(),
            },
            TabView::Triggers => match tab.trigger_count {
                Some(n) => format!("{n} trigger{}", if n == 1 { "" } else { "s" }),
                None => "…".into(),
            },
            TabView::Ddl => String::new(),
        };
        let filtered = tab.filters.applied.is_some();
        let (has_prev, has_next) = (
            d.page_offset > 0,
            d.total_rows()
                .is_some_and(|n| d.page_offset + d.page_limit < n),
        );
        let (limit_in, offset_in) = (tab.page_limit_input.clone(), tab.page_offset_input.clone());
        let segmented = div()
            .flex()
            .flex_row()
            .h(px(crate::settings::row_h()))
            .rounded(crate::theme::RADIUS_MD)
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(seg("tab-view-data", "Data", TabView::Data, cx))
            .child(div().w(px(1.)).h_full().bg(border))
            .child(seg(
                "tab-view-structure",
                "Structure",
                TabView::Structure,
                cx,
            ))
            // Engines without indexes (Redis, ClickHouse, Snowflake, …) have no Index view.
            .when(self.pool.as_ref().is_none_or(|p| p.caps().indexes), |d| {
                d.child(div().w(px(1.)).h_full().bg(border)).child(seg(
                    "tab-view-index",
                    "Index",
                    TabView::Index,
                    cx,
                ))
            });
        div()
            .h(px(crate::settings::bar_h() + 4.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(border)
            .child(segmented)
            .when(view == TabView::Data && editable, |this| {
                this.child(
                    pill("grid-add-row", "+ Row", false, cx).on_click(cx.listener(
                        move |this, _, window, cx| {
                            if let Some(t) = this.grid_tab(ix) {
                                let st = t.state.clone();
                                st.update(cx, |s, cx| {
                                    s.delegate_mut().add_row(window, cx);
                                });
                                cx.notify();
                            }
                        },
                    )),
                )
            })
            .when(view == TabView::Index && editable, |this| {
                this.child(pill("idx-add", "+ Index", false, cx).on_click(
                    cx.listener(move |this, _, window, cx| this.add_index_row(ix, window, cx)),
                ))
            })
            .when(view == TabView::Triggers && editable, |this| {
                this.child(pill("trg-add", "+ Trigger", false, cx).on_click(
                    cx.listener(move |this, _, window, cx| this.new_trigger_editor(ix, window, cx)),
                ))
            })
            .when(view == TabView::Structure && editable, |this| {
                this.child(
                    pill("struct-add-col", "+ Column", false, cx).on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.add_structure_column(ix, window, cx);
                        },
                    )),
                )
            })
            // Triggers / DDL next to the add button, as in the structure views.
            .when(view != TabView::Data && tab.draft.is_none(), |this| {
                this.child(
                    pill(
                        "tab-view-triggers",
                        "Triggers",
                        view == TabView::Triggers,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_tab_view(ix, TabView::Triggers, window, cx)
                    })),
                )
                .child(
                    pill("tab-view-ddl", "DDL", view == TabView::Ddl, cx).on_click(cx.listener(
                        move |this, _, window, cx| this.set_tab_view(ix, TabView::Ddl, window, cx),
                    )),
                )
            })
            .child(
                div()
                    .flex_1()
                    .flex()
                    .justify_center()
                    .text_caption()
                    .font_family(crate::settings::ui_font())
                    .text_color(muted)
                    .child(center),
            )
            .when(view == TabView::Data, |this| {
                let t = cx.theme();
                let (fg, muted) = (t.foreground, t.muted_foreground);
                let settings = Popover::new("page-settings")
                    .anchor(gpui_kit::Anchor::TopRight)
                    .trigger(
                        Button::new("page-settings-btn")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::SlidersHorizontal).size(px(14.)))
                            .tooltip("Limit / offset"),
                    )
                    .content(move |_, _, _| {
                        let (l, o) = (limit_in.clone(), offset_in.clone());
                        let row = |label: &'static str, input: &Entity<InputState>| {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().w(px(48.)).text_sm().text_color(fg).child(label))
                                .child(div().w(px(140.)).child(Input::new(input).small()))
                        };
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_1()
                            .child(row("Limit", &l))
                            .child(row("Offset", &o))
                            .child(
                                Button::new("page-go")
                                    .label("Go")
                                    .small()
                                    .w_full()
                                    .on_click(move |_, _, cx| {
                                        let view = cx.global::<TuskHandle>().0.clone();
                                        view.update(cx, |this, cx| this.apply_page_inputs(ix, cx));
                                    }),
                            )
                            .child(div().text_caption().text_color(muted).child(
                                crate::kbd::rich_colored(
                                    "[cmd-left] / [cmd-right] previous / next page",
                                    muted,
                                ),
                            ))
                    });
                this.child(
                    pill_frame("grid-filters", tab.filters.visible, cx)
                        .gap_1p5()
                        .child(Icon::new(IconName::ListFilter).size(px(12.)))
                        .child(if filtered { "Filters •" } else { "Filters" })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_filters(window, cx);
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(
                            icon_btn("page-prev", IconName::ChevronLeft, has_prev, cx)
                                .tooltip(|w, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Previous page")
                                        .key_binding(crate::kbd::tip("cmd-left"))
                                        .build(w, cx)
                                })
                                .on_click(
                                    cx.listener(move |this, _, w, cx| {
                                        this.step_page(ix, -1, w, cx)
                                    }),
                                ),
                        )
                        .child(settings)
                        .child(
                            icon_btn("page-next", IconName::ChevronRight, has_next, cx)
                                .tooltip(|w, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Next page")
                                        .key_binding(crate::kbd::tip("cmd-right"))
                                        .build(w, cx)
                                })
                                .on_click(
                                    cx.listener(move |this, _, w, cx| this.step_page(ix, 1, w, cx)),
                                ),
                        ),
                )
            })
            .into_any_element()
    }
}
