//! Silo: GPUI client for the silo terminal multiplexer daemon.

mod add_host;
mod appearance;
mod assets;
mod blink;
mod commands;
mod composition;
mod controls;
mod conn;
mod focus;
mod fonts;
mod hosts;
mod keys;
mod layout;
mod motion;
mod palette;
mod pane;
mod radius;
mod settings;
mod settings_panel;
mod switcher;
mod terminal_prefs;
mod text_field;
mod theme;
mod titlebar;
mod tooltip;
mod workspace;

use gpui::{App, Bounds, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px, size};

use crate::{
    commands::act::{CommandPalette, NewSession, OpenSettings, Quit},
    workspace::Workspace,
};

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("SILO_LOG").unwrap_or_else(|_| "info".into());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("silo-net")
        .enable_all()
        .build()
        .expect("tokio runtime");
    let rt = runtime.handle().clone();
    let home = silo_proto::paths::home();
    let exe = conn::server_exe();
    tracing::info!(home = %home.display(), server = %exe.display(), "starting Silo");
    let loaded = terminal_prefs::load(&home);
    if let Some(problem) = &loaded.problem {
        tracing::warn!("{problem}");
    }
    tracing::info!(theme = %loaded.prefs.theme, font = %loaded.prefs.font_family, "terminal preferences");
    theme::install(loaded.prefs, loaded.problem);
    start_menubar(&home);

    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        theme::apply_font(cx);
        // System Settings › Accessibility › Display › Reduce motion, read
        // once at launch (toggling it needs an app restart).
        cx.set_reduce_motion(reduce_motion());
        commands::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu {
                name: "Silo".into(),
                items: vec![MenuItem::action("Settings…", OpenSettings), MenuItem::separator(), MenuItem::action("Quit Silo", Quit)],
                disabled: false,
            },
            Menu {
                name: "Session".into(),
                items: vec![MenuItem::action("New Session", NewSession), MenuItem::action("Command Palette", CommandPalette)],
                disabled: false,
            },
        ]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1000.), px(680.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("silo".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.), px(16.))),
                }),
                window_min_size: Some(size(px(420.), px(240.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(rt.clone(), home.clone(), exe.clone(), window, cx)),
        );
        if let Err(error) = opened {
            eprintln!("silo: cannot open window: {error:#}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
    drop(runtime);
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

/// Launch the bundled menu-bar agent (`silo-menubar`) if it is next to us
/// and not already running for this state directory; it outlives the app.
fn start_menubar(home: &std::path::Path) {
    if std::env::var_os("SILO_NO_MENUBAR").is_some() {
        return;
    }
    let Some(agent) = conn::sibling(&["silo-menubar"]) else { return };
    let mut command = std::process::Command::new(agent);
    command
        .env("SILO_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The agent exits at once when another instance holds its lock.
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    if let Err(error) = command.spawn() {
        tracing::warn!("menubar agent: {error}");
    }
}
