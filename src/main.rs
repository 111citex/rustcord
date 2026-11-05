// Release builds on Windows are GUI apps: no console window behind them.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// Faster than the system's for the small, short-lived allocations of drawing a
// frame, and it hands freed memory back instead of holding it per thread.
#[global_allocator]
static ALLOCATOR: perf::CountingAlloc = perf::CountingAlloc;

mod animated;
mod add_people;
mod call_ring;
mod confirm;
mod discord_icons;
mod emoji_card;
mod font_pages;
mod image_disk;
mod places;
mod perf;
mod captcha;
mod chat_format;
mod composer_md;
mod context_menu;
mod emoji_suggest;
mod gifs;
mod invites;
mod links;
mod name_fonts;
mod avatar_colors;
mod file_preview;
mod gradient;
mod audio;
mod drafts;
mod nameplates;
mod profile_effects;
mod transition;
mod notifications;
#[cfg(debug_assertions)]
mod login_check;
mod anim;
mod assets;
mod badges;
mod markdown;
mod markdown_view;
mod members;
mod message_text;
mod nav;
mod pages;
mod profile_card;
mod reaction_names;
mod profile_preload;
mod rich_view;
mod router;
mod settings;
mod shell;
mod sidebar;
mod slider;
mod smooth_scroll;
mod stickers;
mod store;
mod theme;
mod toasts;
mod ui;
mod uploads;
mod videos;
mod viewer;
mod web_login;

use std::sync::Arc;

use cord_core::auth::token_store::KeyringStore;
use cord_core::{Core, CoreConfig};
use gpui_kit::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use reqwest_client::ReqwestClient;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::router::Router;
use crate::shell::Shell;
use crate::store::{Services, Store};
use crate::toasts::Toasts;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--captcha") => captcha::helper_main(),
        Some("--web-login") => web_login::helper_main(),
        #[cfg(debug_assertions)]
        Some("--login-check") => {
            if let Err(error) = login_check::run() {
                eprintln!("login check failed: {error:#}");
                std::process::exit(1);
            }
        }
        _ => {
            if let Err(error) = run() {
                tracing::error!("{error:#}");
                eprintln!("\nRustcord couldn't start: {error:#}\n");
                std::process::exit(1);
            }
        }
    }
}

/// Logs go to stderr and to daily files in the cache dir's `logs/`.
/// Nothing that holds a token is ever logged.
fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let (file, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(
        cord_core::paths::logs_dir(),
        "rustcord.log",
    ));
    tracing_subscriber::registry()
        .with(EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy())
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file))
        .init();
    guard
}

fn run() -> anyhow::Result<()> {
    perf::set_up_allocator();
    let _log_guard = init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Starting Rustcord");

    // Discord traffic runs on tokio; the UI runs on GPUI.
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
    let identity = runtime.block_on(cord_core::startup_identity(&cord_core::paths::cache_dir()));
    tracing::info!(build_number = identity.build_number, "client identity ready");
    let core = Core::new(identity, CoreConfig {
            profile_dir: Some(cord_core::paths::cache_dir().join("profiles")),
            ready_file: Some(cord_core::paths::cache_dir().join("ready.json")),
            ..CoreConfig::default()
        }, Arc::new(KeyringStore::new()), runtime.handle().clone());
    let token = core.saved_token();
    // RUSTCORD_PATH opens a given page at start (for screenshots and debugging).
    let initial_path = match (&token, std::env::var("RUSTCORD_PATH")) {
        (Some(_), Ok(path)) if path.starts_with("/channels/") => path,
        (Some(_), _) => places::get().path.unwrap_or_else(|| "/channels/@me".to_string()),
        (None, _) => "/login".to_string(),
    };

    // GPUI's executor polls futures that touch tokio types; keep the
    // runtime's context entered for the whole application lifetime.
    let _runtime_guard = runtime.enter();

    gpui_kit::application().with_assets(assets::AppAssets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        font_pages::start();
        cx.bind_keys([
            gpui_kit::KeyBinding::new("ctrl-i", shell::ToggleInfo, None),
            gpui_kit::KeyBinding::new("ctrl-,", shell::OpenSettings, None),
            gpui_kit::KeyBinding::new("escape", pages::settings::CloseSettings, Some("Settings")),
        ]);

        // Avatars and icons load through GPUI's HTTP client, as Chrome.
        match ReqwestClient::user_agent(&core.http().identity().user_agent()) {
            Ok(client) => cx.set_http_client(Arc::new(image_disk::DiskCached::new(Arc::new(client)))),
            Err(error) => tracing::error!(%error, "images won't load: failed to create HTTP client"),
        }

        let preferences = settings::Settings::load();
        theme::init(preferences.theme.clone(), cx);
        name_fonts::init(cx);
        let settings = cx.new(|_| settings::SettingsStore::new(preferences));

        let toasts = cx.new(|_| Toasts);
        let store = cx.new(|_| Store::new(core.clone(), toasts.clone()));
        if let Some(token) = token {
            store.update(cx, |store, cx| store.start(token, cx));
        }
        let services = Services { core: core.clone(), store, toasts, settings };
        let router = cx.new(|_| Router::new(pages::routes(), services.clone(), initial_path));
        links::init(router.clone(), cx);

        // Windows gets gpui-kit's drawn title bar; Linux keeps native decorations.
        let base = if cfg!(target_os = "windows") {
            gpui_kit::component::TitleBar::window_options()
        } else {
            WindowOptions {
                titlebar: Some(TitlebarOptions { title: Some("Rustcord".into()), ..Default::default() }),
                ..Default::default()
            }
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(800.)), cx))),
            window_min_size: Some(size(px(940.), px(560.))),
            app_id: Some("rustcord".into()),
            // Opaque: GPUI redraws the whole window every frame, and a
            // see-through window makes the compositor re-blur all of it each
            // time — frames got slower the bigger the window. RUSTCORD_TRANSPARENT=1
            // brings the see-through look back.
            window_background: if std::env::var_os("RUSTCORD_TRANSPARENT").is_some() {
                gpui_kit::WindowBackgroundAppearance::Transparent
            } else {
                gpui_kit::WindowBackgroundAppearance::Opaque
            },
            ..base
        };

        if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(router.clone(), services.clone(), window, cx))
        }) {
            tracing::error!(%error, "failed to open the window");
            cx.quit();
        }
    });

    Ok(())
}
