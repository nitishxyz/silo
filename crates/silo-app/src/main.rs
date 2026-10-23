//! Silo: GPUI client for the silo terminal multiplexer daemon.

mod adaptive;
mod add_host;
mod address;
mod agent;
mod agent_block;
mod app_log;
mod app_requests;
mod appearance;
mod approvals;
mod assets;
mod blink;
mod block_status;
mod browser_actions;
mod browser_home;
mod browser_host;
mod browser_pane;
mod cache;
mod cli_link;
mod clipboard;
mod close_confirm;
mod commands;
mod composition;
mod conn;
mod controls;
mod finder;
mod floats;
mod fonts;
mod gesture;
mod hosts;
mod hosts_state;
mod incompatible;
mod keys;
mod latency;
mod layout;
mod material;
mod motion;
mod new_tab_menu;
mod notifications;
mod offers;
mod palette;
mod pane;
mod pane_header;
mod qr;
mod radius;
mod remote;
mod settings;
mod settings_agents;
mod settings_panel;
mod settings_table;
mod share;
mod status;
mod switcher;
mod terminal_prefs;
mod text_field;
mod theme;
mod titlebar;
mod tooltip;
mod updater;
mod web;
mod window_picker;
mod workspace;

use gpui::{App, Bounds, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions, point, prelude::*, px, size};

use crate::{
    commands::act::{CommandPalette, CycleWindows, HideOthers, HideSilo, NewSession, NewWindow, OpenSettings, Quit, ShowAll},
    workspace::Workspace,
};

fn main() {
    // CEF subprocesses (`<App> Helper*.app` are copies of this binary).
    if std::env::args().any(|arg| arg.starts_with("--type=")) {
        std::process::exit(silo_web::run_helper());
    }
    // The app's calls are the user's own: they need no approval.
    silo_client::set_role(silo_proto::control::ClientRole::App);
    // Closes that would end running programs are asked about here.
    silo_client::advertise(silo_proto::compat::feature::CLOSE_GUARD);
    silo_client::advertise(silo_proto::compat::feature::BROWSER_ACTIONS);
    // Browsers no window shows run off screen (`browser_host.rs`).
    silo_client::advertise(silo_proto::compat::feature::BROWSER_HOST);
    let home = silo_proto::paths::home();
    let log = app_log::AppLog::open(&home).map(std::sync::Arc::new);
    {
        use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};
        let filter = tracing_subscriber::EnvFilter::try_new(log_filter(std::env::var("SILO_LOG").ok())).unwrap_or_else(|_| log_filter(None).into());
        let file = log.as_ref().ok().map(|log| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(log.clone()));
        let _ = tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr)).with(file).try_init();
    }
    if let Err(error) = &log {
        tracing::warn!("app log {}: {error}", home.join("app.log").display());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("silo-net").enable_all().build().expect("tokio runtime");
    let rt = runtime.handle().clone();
    let exe = conn::server_exe();
    tracing::info!(home = %home.display(), server = %exe.display(), "starting Silo");
    let service = conn::local_service(&home, exe.clone());
    let managed = service.as_ref().ok().filter(|s| s.manager == silo_client::service::Manager::Launchd);
    if let Some(svc) = managed {
        let (home, silo) = (home.clone(), svc.exe.clone());
        rt.spawn_blocking(move || cli_link::refresh(&home, &silo));
    }
    let agent = match managed {
        Some(svc) => svc.exe.parent().map(|dir| dir.join("silo-menubar")),
        None => conn::sibling(&["silo-menubar"]),
    };
    let loaded = terminal_prefs::load(&home);
    if let Some(problem) = &loaded.problem {
        tracing::warn!("{problem}");
    }
    tracing::info!(theme = %loaded.prefs.theme, font = %loaded.prefs.font_family, "terminal preferences");
    theme::install(loaded.prefs, loaded.problem);
    if let Some(agent) = agent {
        start_menubar(&agent, &home);
    }

    // `silo://settings` (sent by the menu-bar agent) opens Settings, in a
    // running app or at launch.
    let (urls_tx, mut urls) = tokio::sync::mpsc::unbounded_channel::<String>();
    let application = gpui_platform::application().with_assets(assets::Assets);
    application.on_open_urls(move |list| {
        for url in list {
            let _ = urls_tx.send(url);
        }
    });
    // "Open Silo" (menu bar, Dock) on a running app: the frontmost window.
    application.on_reopen(|cx| match frontmost(cx) {
        Some(handle) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        None => {
            let _ = open_window(None, cx).inspect_err(|e| tracing::warn!("open window: {e:#}"));
        }
    });
    application.run(move |cx: &mut App| {
        theme::apply_font(cx);
        theme::warm_fonts(cx);
        // System Settings › Accessibility › Display › Reduce motion, read
        // once at launch (toggling it needs an app restart).
        cx.set_reduce_motion(reduce_motion());
        commands::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &HideSilo, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_app_quit(|_| {
            web::shutdown();
            async {}
        })
        .detach();
        cx.set_menus(vec![
            Menu {
                name: "Silo".into(),
                items: vec![
                    MenuItem::action("Settings…", OpenSettings),
                    MenuItem::separator(),
                    MenuItem::action("Hide Silo", HideSilo),
                    MenuItem::action("Hide Others", HideOthers),
                    MenuItem::action("Show All", ShowAll),
                    MenuItem::separator(),
                    MenuItem::action("Quit Silo", Quit),
                ],
                disabled: false,
            },
            Menu {
                name: "Session".into(),
                items: vec![
                    MenuItem::action("New Session", NewSession),
                    MenuItem::action("New Window…", NewWindow),
                    MenuItem::action("Command Palette", CommandPalette),
                ],
                disabled: false,
            },
            Menu { name: "Window".into(), items: vec![MenuItem::action("Cycle Through Windows", CycleWindows)], disabled: false },
        ]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let hosts = hosts_state::HostsState::install(rt.clone(), home.clone(), service.clone(), cx);
        browser_host::BrowserHost::install(&hosts, cx);
        updater::Updater::install(rt.clone(), hosts, &exe, cx);
        if let Err(error) = open_window(None, cx) {
            eprintln!("silo: cannot open window: {error:#}");
            cx.quit();
            return;
        }
        if std::env::var("SILO_OPEN").is_ok_and(|v| v == "settings") {
            show_settings(cx);
        }
        cx.spawn(async move |cx| {
            while let Some(url) = urls.recv().await {
                if !opens_settings(&url) {
                    tracing::info!(url, "ignoring URL");
                    continue;
                }
                cx.update(show_settings);
            }
        })
        .detach();
        cx.activate(true);
    });
    drop(runtime);
}

/// The frontmost Silo window, if any.
fn frontmost(cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.window_stack().unwrap_or_else(|| cx.windows()).into_iter().find_map(|w| w.downcast::<Workspace>())
}

/// Settings in the frontmost window (a new one if none is open).
fn show_settings(cx: &mut App) {
    let Some(handle) = frontmost(cx).or_else(|| open_window(None, cx).ok()) else { return };
    let _ = handle.update(cx, |workspace, window, cx| {
        window.activate_window();
        workspace.open_settings(window, cx);
    });
    cx.activate(true);
}

/// Open a window showing `session` (host index, session id), or this
/// Mac's last session. Windows cascade 28 px from the active one.
pub fn open_window(session: Option<(usize, String)>, cx: &mut App) -> anyhow::Result<WindowHandle<Workspace>> {
    let current = cx.active_window().or_else(|| cx.windows().last().copied());
    let bounds = match current.and_then(|w| w.update(cx, |_, window, _| window.bounds()).ok()) {
        Some(b) => Bounds { origin: b.origin + point(px(28.), px(28.)), size: b.size },
        None => Bounds::centered(None, size(px(1000.), px(680.)), cx),
    };
    let titlebar = TitlebarOptions { title: Some("silo".into()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(16.))) };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(titlebar),
        window_min_size: Some(size(px(420.), px(240.))),
        app_owns_titlebar_drag: true,
        ..Default::default()
    };
    let hosts = hosts_state::HostsState::global(cx);
    cx.open_window(options, |window, cx| cx.new(|cx| Workspace::new(hosts, session, window, cx)))
}

/// `SILO_LOG` (default `info`) with GPUI's per-face font warnings muted,
/// unless the spec names that target itself.
fn log_filter(spec: Option<String>) -> String {
    const FONTS: &str = "gpui_macos::text_system";
    let spec = spec.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "info".into());
    if spec.contains(FONTS) { spec } else { format!("{spec},{FONTS}=error") }
}

/// `silo://settings` (any trailing slash or query).
fn opens_settings(url: &str) -> bool {
    url.strip_prefix("silo://settings").is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?']))
}

fn reduce_motion() -> bool {
    if let Some(forced) = std::env::var_os("SILO_REDUCE_MOTION") {
        return forced == "1";
    }
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.universalaccess", "reduceMotion"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "1")
}

/// Launch the menu-bar agent unless one already runs for this state
/// directory; it outlives the app.
fn start_menubar(agent: &std::path::Path, home: &std::path::Path) {
    if std::env::var_os("SILO_NO_MENUBAR").is_some() {
        return;
    }
    let mut command = std::process::Command::new(agent);
    command.env("SILO_HOME", home).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // The agent exits at once when another instance holds its lock.
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    if let Err(error) = command.spawn() {
        tracing::warn!("menubar agent: {error}");
    }
}

#[cfg(test)]
mod url_tests {
    #[test]
    fn settings_url() {
        assert!(super::opens_settings("silo://settings"));
        assert!(super::opens_settings("silo://settings/"));
        assert!(!super::opens_settings("silo://settingsx"));
        assert!(!super::opens_settings("https://settings"));
    }
}
