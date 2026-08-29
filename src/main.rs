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
// The UI is the cross-platform Slint one (ui_slint) - the same window on
// Windows, Linux and macOS. The original Win32 front end it replaced is in the
// history up to v0.2.0.

// GUI builds detach from the console; headless ones must NOT. Without a console
// a Windows process gets no CTRL_C_EVENT, so the headless run loop below could
// never be stopped cleanly - it would sit invisible in Task Manager and only die
// by being killed, which is exactly the unclean exit that forces a full recheck.
#![cfg_attr(
    all(not(debug_assertions), windows, feature = "ui-slint"),
    windows_subsystem = "windows"
)]
// A headless build compiles the UI-support layer (ui::format, ui::filters,
// core::utils) with only the web interface calling into parts of it, which
// would bury real warnings under a screenful of dead-code ones.
#![cfg_attr(not(feature = "ui-slint"), allow(dead_code))]

mod bittorrent;
mod buildinfo;
mod cli;
mod core;
mod ipc;
mod ui;
#[cfg(feature = "ui-slint")]
mod ui_slint;
mod updatechecker;
mod webui;

/// True when no native UI is compiled in. The web interface is then the only
/// way to reach this process, which makes failing to start it fatal rather
/// than a degraded-but-usable state.
const HEADLESS: bool = !cfg!(feature = "ui-slint");

use std::sync::{Arc, Mutex};

/// The language a fresh install comes up in. The OS locale deliberately does
/// not influence this - see the note where the startup locale is resolved.
pub const DEFAULT_LOCALE: &str = "en-US";

use crate::core::configuration::Configuration;
use crate::core::database::Database;
use crate::core::environment::Environment;
use crate::ui::translator::Translator;

/// Everything the UI layer needs, assembled during startup.
// A headless build reads almost none of these yet - the HTTP API is what will
// consume them. Keeping them assembled means that lands as an addition rather
// than a rework of startup.
#[cfg_attr(not(feature = "ui-slint"), allow(dead_code))]
pub struct AppContext {
    pub env: Arc<Environment>,
    pub db: Arc<Database>,
    pub cfg: Arc<Configuration>,
    pub session: Arc<bittorrent::session::Session>,
    pub translator: Translator,
    pub ipc: Option<ipc::Server>,
    pub args: Vec<String>,
    /// Where the update check leaves what it found; the UI polls it.
    pub update_slot: updatechecker::Slot,
    pub geoip: Arc<core::geoip::GeoIp>,
    /// The running web interface, handed to the UI so Preferences can restart
    /// it in place rather than asking for an app restart. `None` when it is
    /// disabled or failed to start.
    pub web: Option<actix_web::dev::ServerHandle>,
}

/// Borrow the launching terminal's console, on Windows.
///
/// A release GUI build is a `windows` subsystem binary (see the attribute at
/// the top), which starts with no console at all - so `--help`, `--version`
/// and `--webui-status` printed into nothing and looked broken. Attaching to
/// the parent gives those flags somewhere to print when the app was started
/// from a terminal. Failure is the normal case (launched from Explorer, or a
/// console is already attached) and is ignored: the flags then print nowhere,
/// exactly as before, and Help > Command line covers that reader instead.
#[cfg(windows)]
fn attach_parent_console() {
    // SAFETY: no arguments, no output parameters; the only effect is to give
    // this process the parent's console handles, and a failure is a no-op.
    unsafe {
        winapi::um::wincon::AttachConsole(winapi::um::wincon::ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

/// The `--help` text.
///
/// A function rather than a const because the version is in it, and one text
/// rather than two because Help > Command line shows this exact string - a
/// window that disagreed with the terminal would be worse than no window.
pub fn help_text(tr: &Translator) -> String {
    format!(
        concat!(
            "NanoTorrent {} - {}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "{}",
        ),
        buildinfo::version(),
        tr.i18n("cli_tagline"),
        tr.i18n("cli_usage_line"),
        tr.i18n("cli_forwarded_note"),
        cli::usage(tr),
        cli::settings_help(tr),
        webui::cli::usage(tr)
    )
}

/// The translator for the configured language.
///
/// Shared by startup and by the command-line paths, which each open their own
/// database and would otherwise each decide what "no locale set" means.
pub fn load_translator(env: &Environment, cfg: &Configuration) -> Translator {
    let locale = cfg
        .get_string("locale_name")
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_LOCALE));
    Translator::load(&env.get_lang_path(), &locale)
}

/// The same, for a path that has no database open yet - `--help` runs before
/// one exists. A database that will not open falls back to English rather than
/// stopping the flag from printing.
fn help_translator() -> Translator {
    let env = Environment::create();
    let translator = Database::open(&env).ok().and_then(|db| {
        let db = Arc::new(db);
        db.migrate().ok()?;
        Some(load_translator(&env, &Configuration::new(db)))
    });
    translator.unwrap_or_else(|| Translator::load(&env.get_lang_path(), DEFAULT_LOCALE))
}

/// Thin wrapper around [`run`]: its job is to turn a startup failure into
/// something visible, since a GUI build has no console to print to.
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

/// Show a startup failure before the UI exists.
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
    #[cfg(all(not(debug_assertions), windows, feature = "ui-slint"))]
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

/// Startup, in the order listed at the top of this file, ending in the UI
/// event loop and a clean session shutdown.
fn run() -> anyhow::Result<()> {
    // Before ANY TLS happens - reqwest (update check, GeoIP) and the web
    // interface both reach for rustls, which panics rather than erroring if no
    // provider has been chosen. See the function for why it cannot self-select.
    webui::tls::ensure_crypto_provider();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Before the database is opened and before the single-instance socket:
    // `nanotorrent --version` has to print and exit, not hand itself to a
    // running window as though it were a torrent to open.
    //
    // The console has to be borrowed first on Windows, or every print below
    // this line goes nowhere in a release GUI build.
    if !args.is_empty() {
        attach_parent_console();
    }

    if let Some(flag) = args.first().map(String::as_str)
        && matches!(flag, "--version" | "-V" | "--help" | "-h")
    {
        if flag == "--version" || flag == "-V" {
            println!("NanoTorrent {}", buildinfo::version());
        } else {
            println!("{}", help_text(&help_translator()));
        }
        return Ok(());
    }

    // BEFORE the IPC check below, which forwards argv to a running instance and
    // exits - a --set-web-password would otherwise be handed to the running
    // window as though it were a torrent to open.
    //
    // Their errors do NOT go through `?`. A rejected value is a usage error,
    // not a failure to start: bubbling it up would print it under "NanoTorrent
    // could not start" and, in a release GUI build, put it in a modal box that
    // a script has nobody to dismiss.
    // Sequential, not an array of both calls: each one may consume stdin or
    // write a setting, so the second must not run when the first took the flag.
    let usage_error = |err: anyhow::Error| -> ! {
        eprintln!("{err:#}");
        std::process::exit(2);
    };
    match webui::cli::handle(&args) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(err) => usage_error(err),
    }
    match cli::handle(&args) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(err) => usage_error(err),
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
        // Everything NanoTorrent itself does is logged at debug; dependencies
        // stay at info, because librqbit traces every chunk of every piece and
        // one busy torrent would bury the app's own activity in minutes.
        //
        // RUST_LOG overrides the lot when a specific dependency needs
        // watching, e.g. RUST_LOG=info,librqbit=debug,nanotorrent=trace
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,nanotorrent=debug"));

        let subscriber = tracing_subscriber::fmt()
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .with_env_filter(filter)
            // Which module a line came from - the whole point of a debug log
            // is being able to follow one subsystem through it.
            .with_target(true)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    // Version and build first: a log that does not say which binary produced
    // it is guesswork the moment there is more than one build around.
    tracing::info!(
        "NanoTorrent {} ({}) starting up...",
        buildinfo::version(),
        buildinfo::build_stamp()
    );

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
    let translator = load_translator(&env, &cfg);

    let session = Arc::new(bittorrent::session::Session::new(&env, db.clone(), &cfg)?);

    // Fire off the update check in the background.
    let update_slot = Arc::new(Mutex::new(None));
    // false: the automatic check. It stays quiet unless there is something
    // newer - Help > Check for update passes true and reports either way.
    updatechecker::check(&session.handle(), &cfg, update_slot.clone(), false);

    // GeoIP database for peer countries, loaded in the background.
    let geoip = core::geoip::GeoIp::new();
    geoip.spawn_load(&session.handle(), &env, &cfg);

    // Optional web interface (off unless webui.enabled). Held for the lifetime
    // of the process; the server runs on its own thread and its System is torn
    // down at exit.
    // Handed to the UI below, which can stop and respawn it when the settings
    // change. In-flight requests are still cut when the process exits.
    let web = match webui::spawn(session.clone(), cfg.clone(), env.clone()) {
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
        web,
    };

    run_ui(ctx)
}

/// Write a panic and its backtrace to the logs folder before the default hook
/// runs.
///
/// The original used Crashpad; this keeps the local report and drops the
/// upload. Without it a GUI build's panic goes to an stderr nobody is reading.
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

/// Show the main window and run the UI event loop until it closes.
#[cfg(feature = "ui-slint")]
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
#[cfg(not(feature = "ui-slint"))]
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
