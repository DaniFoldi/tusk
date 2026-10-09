// A GUI app on Windows: no console window behind it (release builds only, so
// `cargo run` still shows logs). `--mcp-bridge` keeps working over its pipes.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use gpui_kit::assets::AllAssets;
use gpui_kit::component::Root;
use gpui_kit::component::TitleBar;
use gpui_kit::*;

mod about;
mod acp;
mod acp_registry;
mod actions;
mod agent;
mod app;
mod backup;
mod cell_edit;
mod combo;
mod complete;
mod compose;
mod conn;
mod console;
mod copy_as;
mod db;
mod ddl;
mod dialog;
mod dialog_keys;
mod dock;
mod drivers;
mod engine;
mod export;
mod filter;
mod grid;
mod icons;
mod indexes;
mod kbd;
mod lsp;
mod mcp_bridge;
mod menus;
mod migrate;
mod objects;
mod omarchy;
mod palette;
mod settings;
mod sql;
mod ssh;
mod structure;
mod theme;
mod themes;
mod toast;
mod undo;
mod updater;
mod whats_new;
mod widths;

/// Handle of the main (connection / workspace) window, for dock reopen.
struct MainWindow(AnyWindowHandle);
impl Global for MainWindow {}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Started by an ACP agent as its MCP server: relay to the running app.
    if args.get(1).map(String::as_str) == Some(mcp_bridge::FLAG) {
        if let Some(sock) = args.get(2) {
            mcp_bridge::run(std::path::Path::new(sock));
        }
        return;
    }
    // `tusk mcp`: the same MCP server for any client (Claude Desktop,
    // Cursor, opencode…). It finds the running app itself.
    let first = args.get(1).map(String::as_str);
    if first == Some("mcp") || first == Some("--mcp") {
        mcp_bridge::run_standalone();
        return;
    }
    #[cfg(target_os = "linux")]
    settings::apply_gpu_preference();
    let app = gpui_kit::application().with_assets(AllAssets);
    // Dock click / relaunch while running: bring the main window back, or
    // reopen it on the welcome screen if it was closed.
    app.on_reopen(|cx| {
        if let Some(handle) = cx.try_global::<MainWindow>().map(|m| m.0)
            && cx
                .update_window(handle, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        open_main_window(cx, false);
    });
    dock::set_process_name();
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        actions::bind_keys(cx);
        dialog_keys::bind_keys(cx);
        updater::init(cx);
        menus::install(cx);

        if let Err(e) = theme::load_embedded_fonts(cx) {
            eprintln!("font load error: {e:#}");
        }
        theme::apply(cx);
        omarchy::watch(cx);
        dock::set_icon();
        open_main_window(cx, true);
        // Automated UI checks run the app without taking focus.
        if !background() {
            cx.activate(true);
        }
    });
}

/// `TUSK_BACKGROUND=1`: never activate the app or focus new windows.
pub fn background() -> bool {
    std::env::var_os("TUSK_BACKGROUND").is_some()
}

fn open_main_window(cx: &mut App, auto_connect: bool) {
    let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(600.))),
            focus: !crate::background(),
            ..TitleBar::window_options()
        },
        |window, cx| {
            let initial = db::load_connections()
                .into_iter()
                .next()
                .unwrap_or_else(db::dev_default);
            let form = conn::ConnectionForm::new(window, cx, &initial);
            let filter = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).placeholder("Filter…")
            });
            let conn_search = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx)
                    .placeholder("Search for connection…")
            });
            let history_search = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx)
                    .placeholder("Search history…")
            });
            let view =
                cx.new(|cx| app::TuskApp::new(form, filter, conn_search, history_search, cx));
            cx.set_global(app::TuskHandle(view.clone()));
            view.read(cx).focus_handle().focus(window, cx);
            // Red traffic light in a workspace = leave it: the
            // window stays and shows the welcome screen. On the welcome screen
            // it closes (the dock icon reopens it).
            // Mode "System" (or "Omarchy" without a readable theme):
            // re-theme when the OS switches light / dark.
            window
                .observe_window_appearance(|_, cx| {
                    let mode = settings::get().appearance;
                    if mode == settings::Appearance::System
                        || (mode == settings::Appearance::Omarchy && omarchy::palette().is_none())
                    {
                        theme::apply(cx);
                        cx.refresh_windows();
                    }
                })
                .detach();
            let close_view = view.clone();
            window.on_window_should_close(cx, move |window, cx| {
                close_view.update(cx, |app, cx| app.on_close_request(window, cx))
            });
            if auto_connect {
                // ⌘Q and relaunch lands straight back in the last connection.
                view.update(cx, |app, cx| app.auto_connect(cx));
                whats_new::announce(window, cx);
            }
            cx.new(|cx| Root::new(view, window, cx))
        },
    );
    match result {
        Ok(handle) => cx.set_global(MainWindow(handle.into())),
        Err(e) => eprintln!("failed to open window: {e}"),
    }
}
