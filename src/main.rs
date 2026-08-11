// Port of src/picotorrent/main.cpp and application.cpp
//
// Startup order mirrors the original:
//   1. Parse command line options (torrent files & magnet links)
//   2. Single-instance check - forward args to a running instance via IPC
//   3. Set up file logging
//   4. Open the SQLite database and run migrations
//   5. Load configuration and translations
//   6. Create the BitTorrent session
//   7. Show the main window
//
// The UI is the native Win32 one (ui_native), matching the original's
// wxWidgets-over-Win32 look.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bittorrent;
mod buildinfo;
mod core;
mod ipc;
mod ui;
#[cfg(feature = "ui-native")]
mod ui_native;
mod updatechecker;

use std::sync::{Arc, Mutex};

use crate::core::configuration::Configuration;
use crate::core::database::Database;
use crate::core::environment::Environment;
use crate::ui::translator::Translator;
use crate::updatechecker::UpdateInfo;

/// Everything the UI layer needs, assembled during startup.
pub struct AppContext {
    pub env: Arc<Environment>,
    pub db: Arc<Database>,
    pub cfg: Arc<Configuration>,
    pub session: Arc<bittorrent::session::Session>,
    pub translator: Translator,
    pub ipc: Option<ipc::Server>,
    pub args: Vec<String>,
    pub update_slot: Arc<Mutex<Option<UpdateInfo>>>,
    pub geoip: Arc<core::geoip::GeoIp>,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Port of the IPC single-instance handling in main.cpp.
    let server = match ipc::init(&args) {
        ipc::Instance::Primary(server) => Some(server),
        ipc::Instance::Secondary => {
            // Options were forwarded to the running instance.
            return Ok(());
        }
    };

    let env = Arc::new(Environment::create());

    // One-time takeover of an existing PicoTorrent data folder (settings,
    // session state) after the rename to NanoTorrent.
    env.migrate_legacy_data();

    // Basic crash reporting - the Rust take on the original's Crashpad
    // integration (minus the upload): panics are written with a backtrace
    // to the logs folder before the default handler runs.
    install_panic_hook(
        env.get_log_file_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default(),
    );

    // File logging (the original used boost::log).
    let log_path = env.get_log_file_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log_file = std::fs::File::create(&log_path).ok();

    if let Some(file) = log_file {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    tracing::info!("NanoTorrent starting up...");

    // Register our AppUserModelID so Windows shows/attributes toast
    // notifications (download complete) to NanoTorrent.
    core::toast::register();

    let db = Arc::new(Database::open(&env)?);
    db.migrate()?;

    let cfg = Arc::new(Configuration::new(db.clone()));

    // Locale: explicit setting or the OS locale, like the original.
    let locale = cfg
        .get_string("locale_name")
        .filter(|l| !l.is_empty())
        .unwrap_or_else(Environment::get_current_locale);
    let translator = Translator::load(&env.get_lang_path(), &locale);

    let session = Arc::new(bittorrent::session::Session::new(&env, db.clone(), &cfg)?);

    // Fire off the update check in the background.
    let update_slot = Arc::new(Mutex::new(None));
    updatechecker::check(&session.handle(), &cfg, update_slot.clone());

    // GeoIP database for peer countries, loaded in the background.
    let geoip = core::geoip::GeoIp::new();
    geoip.spawn_load(&session.handle(), &env, &cfg);

    let ctx = AppContext {
        env,
        db,
        cfg,
        session,
        translator,
        ipc: server,
        args,
        update_slot,
        geoip,
    };

    run_ui(ctx)
}

fn install_panic_hook(log_dir: std::path::PathBuf) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = chrono::Local::now().format("%Y%m%d%H%M%S");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::create_dir_all(&log_dir);
        let _ = std::fs::write(
            log_dir.join(format!("NanoTorrent.crash.{ts}.log")),
            format!("{info}\n\nbacktrace:\n{backtrace}\n"),
        );
        tracing::error!("panic: {info}");
        default_hook(info);
    }));
}

#[cfg(feature = "ui-native")]
fn run_ui(ctx: AppContext) -> anyhow::Result<()> {
    ui_native::run(ctx)
}

#[cfg(not(feature = "ui-native"))]
fn run_ui(_ctx: AppContext) -> anyhow::Result<()> {
    compile_error!("enable the ui-native cargo feature");
}
