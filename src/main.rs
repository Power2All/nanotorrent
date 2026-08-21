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

// GUI builds detach from the console; headless ones must NOT. Without a console
// a Windows process gets no CTRL_C_EVENT, so the headless run loop below could
// never be stopped cleanly - it would sit invisible in Task Manager and only die
// by being killed, which is exactly the unclean exit that forces a full recheck.
#![cfg_attr(
    all(not(debug_assertions), feature = "ui-native", windows),
    windows_subsystem = "windows"
)]
// A headless build compiles the whole UI-support layer (ui::format, ui::filters,
// core::utils) with nothing calling it yet, which buries real warnings under ~27
// dead-code ones. The HTTP API is what consumes these, so this comes back out in
// Phase 1 rather than growing per-item attributes now.
#![cfg_attr(not(any(all(feature = "ui-native", windows), feature = "ui-slint")), allow(dead_code))]

mod bittorrent;
mod buildinfo;
mod core;
mod ipc;
mod ui;
// `windows` as well as the feature: ui-native is on by default, but the Win32
// UI only exists on Windows. Linux/macOS builds fall through to run_ui's
// headless arm instead of failing to compile.
#[cfg(all(feature = "ui-native", windows))]
mod ui_native;
#[cfg(feature = "ui-slint")]
mod ui_slint;
mod updatechecker;
mod webui;

/// True when no native UI is compiled in. The web interface is then the only
/// way to reach this process, which makes failing to start it fatal rather
/// than a degraded-but-usable state.
const HEADLESS: bool =
    !cfg!(any(all(feature = "ui-native", windows), feature = "ui-slint"));

use std::sync::{Arc, Mutex};

/// The language a fresh install comes up in. The OS locale deliberately does
/// not influence this - see the note where the startup locale is resolved.
pub const DEFAULT_LOCALE: &str = "en-US";

use crate::core::configuration::Configuration;
use crate::core::database::Database;
use crate::core::environment::Environment;
use crate::ui::translator::Translator;
use crate::updatechecker::UpdateInfo;

/// Everything the UI layer needs, assembled during startup.
// A headless build reads almost none of these yet - the HTTP API is what will
// consume them. Keeping them assembled means that lands as an addition rather
// than a rework of startup.
#[cfg_attr(not(any(all(feature = "ui-native", windows), feature = "ui-slint")), allow(dead_code))]
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

fn main() {
    if let Err(err) = run() {
        // The release build is a GUI subsystem binary, so a returned Err would
        // be printed to a stderr nobody is attached to and the process would
        // just vanish. Put it on screen instead.
        tracing::error!("startup failed: {err:#}");
        fatal_error(&format!("{err:#}"));
        std::process::exit(1);
    }
}

/// Show a startup failure before the UI exists (nwg isn't initialised yet).
fn fatal_error(msg: &str) {
    // Free, and the right channel whenever anyone is attached to it.
    eprintln!("NanoTorrent could not start: {msg}");

    // The cfg here MUST stay identical to the `windows_subsystem` attribute at
    // the top of this file: that build, and only that build, has nowhere for
    // the eprintln above to go, which is the entire reason a dialog exists.
    //
    // This was briefly a runtime `GetConsoleWindow()` check, which is wrong.
    // MinTTY (Git Bash, and any terminal not using a Win32 console) talks over
    // pipes, so that call returns NULL while stderr works perfectly - and a
    // headless build would put up a modal box that blocks forever waiting for
    // a click nobody is there to make. A compile-time fact deserves a
    // compile-time test.
    #[cfg(all(not(debug_assertions), feature = "ui-native", windows))]
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        let wide = |s: &str| -> Vec<u16> {
            std::ffi::OsStr::new(s)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let text = wide(msg);
        let caption = wide("NanoTorrent could not start");
        winapi::um::winuser::MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            winapi::um::winuser::MB_OK
                | winapi::um::winuser::MB_ICONERROR
                | winapi::um::winuser::MB_SETFOREGROUND
                | winapi::um::winuser::MB_TOPMOST,
        );
    }
}

fn run() -> anyhow::Result<()> {
    // Before ANY TLS happens - reqwest (update check, GeoIP) and the web
    // interface both reach for rustls, which panics rather than erroring if no
    // provider has been chosen. See the function for why it cannot self-select.
    webui::tls::ensure_crypto_provider();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // BEFORE the IPC check below, which forwards argv to a running instance and
    // exits - a --set-web-password would otherwise be handed to the running
    // window as though it were a torrent to open.
    if webui::cli::handle(&args)? {
        return Ok(());
    }

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

    // Locale: whatever the user picked in Preferences, otherwise English.
    //
    // Deliberately NOT the OS locale (which is what the original did): a first
    // run always comes up in English and the user changes it if they want to.
    // This also matches the Preferences dialog, which already fell back to
    // en-US - so an unset locale_name used to start the app in the system
    // language while the picker claimed English was selected.
    let locale = cfg
        .get_string("locale_name")
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_LOCALE));
    let translator = Translator::load(&env.get_lang_path(), &locale);

    let session = Arc::new(bittorrent::session::Session::new(&env, db.clone(), &cfg)?);

    // Fire off the update check in the background.
    let update_slot = Arc::new(Mutex::new(None));
    updatechecker::check(&session.handle(), &cfg, update_slot.clone());

    // GeoIP database for peer countries, loaded in the background.
    let geoip = core::geoip::GeoIp::new();
    geoip.spawn_load(&session.handle(), &env, &cfg);

    // Optional web interface (off unless webui.enabled). Held for the lifetime
    // of the process; the server runs on its own thread and its System is torn
    // down at exit.
    // ponytail: no graceful stop on shutdown - in-flight requests are cut when
    // the process exits. Wire ServerHandle::stop through run_ui if that starts
    // mattering (it will once prefs can restart the server in place).
    let _web = match webui::spawn(session.clone(), cfg.clone(), env.clone()) {
        Ok(handle) => handle,
        Err(err) if HEADLESS => {
            return Err(err.context("the web interface is the only interface on this build"));
        }
        Err(err) => {
            tracing::error!("web interface did not start: {err:#}");
            None
        }
    };

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

#[cfg(all(feature = "ui-native", windows))]
fn run_ui(ctx: AppContext) -> anyhow::Result<()> {
    ui_native::run(ctx)
}

/// The cross-platform UI. Reached when ui-native is not available - which is
/// every non-Windows build, and a Windows build asked for it explicitly with
/// `--no-default-features --features ui-slint`.
#[cfg(all(feature = "ui-slint", not(all(feature = "ui-native", windows))))]
fn run_ui(ctx: AppContext) -> anyhow::Result<()> {
    ui_slint::run(ctx)
}

/// Headless: this target has no UI yet (Linux/macOS, or a Windows build with
/// --no-default-features). Keep the session running until asked to stop, then
/// shut it down cleanly so fastresume state is flushed - a torrent client that
/// exits without that makes every torrent recheck on next start.
///
/// This is the loop the HTTP API will attach to, so it is deliberately a real
/// run loop rather than a stub that returns.
#[cfg(not(any(all(feature = "ui-native", windows), feature = "ui-slint")))]
fn run_ui(ctx: AppContext) -> anyhow::Result<()> {
    tracing::info!("no UI on this target - running headless, Ctrl-C to stop");

    // Handle::block_on, not a nested runtime: the session already owns one and
    // main() is not itself a runtime thread.
    ctx.session.handle().block_on(async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!("could not listen for Ctrl-C ({err}) - parking instead");
            std::future::pending::<()>().await;
        }
    });

    tracing::info!("shutting down");
    ctx.session.stop();
    Ok(())
}
