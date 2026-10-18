//! Silo: GPUI client for the silo terminal multiplexer daemon.

mod add_host;
mod assets;
mod composition;
mod conn;
mod hosts;
mod pane;
mod switcher;
mod text_field;
mod theme;
mod titlebar;
mod workspace;

use gpui::{App, Bounds, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, point, prelude::*, px, size};

use crate::workspace::{NewSession, Quit, Workspace};

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
    let theme = theme::load_ghostty();
    tracing::info!(source = %theme.source, background = format!("#{:06x}", theme.background), font = ?theme.font_family, "theme");
    theme::install(theme);
    start_menubar(&home);

    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        let (family, font_size) = theme::font();
        let mut font = silo_render::FontSettings { size: font_size.unwrap_or(13.0), ..Default::default() };
        if let Some(family) = family {
            // Match like Ghostty: case- and space-insensitive
            // ("BerkeleyMono Nerd Font Mono" = "Berkeley Mono Nerd Font Mono").
            let key = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
            let wanted = key(&family);
            if let Some(name) = cx.text_system().all_font_names().into_iter().find(|f| key(f) == wanted) {
                font.family = name;
            } else {
                tracing::warn!("font {family:?} from the Ghostty config is not installed; using {}", font.family);
            }
        }
        silo_render::set_terminal_font(font);
        workspace::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![
            Menu { name: "Silo".into(), items: vec![MenuItem::action("Quit Silo", Quit)], disabled: false },
            Menu { name: "Session".into(), items: vec![MenuItem::action("New Session", NewSession)], disabled: false },
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
