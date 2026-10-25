// Port of src/picotorrent/main.cpp and application.cpp
//
// Startup order mirrors the original:
//   1. Parse command line options (torrent files & magnet links)
//   2. Single-instance check - forward args to a running instance via IPC
//   3. Set up file logging
//   4. Open the SQLite database and run migrations
//   5. Load configuration and translations
//   6. Create the BitTorrent session
//   7. Show the main window - or, headless, wait for a signal to stop
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
mod plugins;
mod ui;
#[cfg(feature = "ui-slint")]
mod ui_slint;
mod updatechecker;
mod webui;

/// Set by `--headless`, before anything reads [`headless`].
static HEADLESS_FLAG: AtomicBool = AtomicBool::new(false);

/// True when this run has no window: a build without one, or `--headless`.
/// The web interface is then the only way to reach this process, which makes
/// failing to start it fatal rather than a degraded-but-usable state.
fn headless() -> bool {
    !cfg!(feature = "ui-slint") || HEADLESS_FLAG.load(Ordering::Relaxed)
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The language a fresh install comes up in. The OS locale deliberately does
/// not influence this - see the note where the startup locale is resolved.
pub const DEFAULT_LOCALE: &str = "en-US";

use crate::core::configuration::Configuration;
use crate::core::database::Database;
use crate::core::environment::Environment;
use crate::ui::translator::Translator;

/// Everything the UI layer needs, assembled during startup.
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
            "  nanotorrent --portable            {}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "\n",
            "  nanotorrent --headless            {}\n",
            "  nanotorrent --log-level LEVEL     {}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "\n",
            "{}\n",
            "{}",
            "\n{}",
        ),
        buildinfo::version(),
        tr.i18n("cli_tagline"),
        tr.i18n("cli_usage_line"),
        tr.i18n("cli_forwarded_note"),
        tr.i18n("cli_portable_header"),
        tr.i18n("cli_flag_portable"),
        tr.i18n("cli_portable_note"),
        tr.i18n("cli_headless_header"),
        tr.i18n("cli_flag_headless"),
        tr.i18n("cli_flag_log_level"),
        tr.i18n("cli_headless_note"),
        tr.i18n("cli_log_level_note"),
        cli::usage(tr),
        cli::settings_help(tr),
        webui::cli::usage(tr),
        core::dbcli::usage(tr)
    )
}

/// What is logged when neither `--log-level` nor RUST_LOG says otherwise.
const DEFAULT_LOG_FILTER: &str = "info,nanotorrent=debug";

/// Take `--log-level` and its value out of `args`, as a filter for
/// tracing-subscriber's `EnvFilter`. `None` when the flag is not there.
///
/// A level - error, warn, info, debug, trace or off - is NanoTorrent's own
/// level. Finer than info, dependencies stay at info, for the reason given
/// where the default is used: `debug` for everything would be librqbit tracing
/// every block of every piece. Anything with `=` or `,` in it is a filter in
/// RUST_LOG's syntax, passed on as it is, for when a dependency is the thing
/// to watch. A bare word that is neither is refused rather than read as a
/// module name, which is what `EnvFilter` would make of a misspelt level.
fn take_log_level(args: &mut Vec<String>) -> anyhow::Result<Option<String>> {
    const LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];
    let Some(at) = args
        .iter()
        .position(|a| a == "--log-level" || a.starts_with("--log-level="))
    else {
        return Ok(None);
    };
    let flag = args.remove(at);
    let value = match flag.strip_prefix("--log-level=") {
        Some(value) => value.to_owned(),
        None if at < args.len() => args.remove(at),
        None => String::new(),
    };
    let value = value.trim().to_ascii_lowercase();

    let spec = match value.as_str() {
        "debug" | "trace" => format!("info,nanotorrent={value}"),
        level if LEVELS.contains(&level) => value.clone(),
        filter if filter.contains(['=', ',']) => {
            tracing_subscriber::EnvFilter::try_new(filter)
                .map_err(|err| anyhow::anyhow!("--log-level {filter}: {err}"))?;
            value.clone()
        }
        _ => anyhow::bail!(
            "--log-level takes one of {}, or a filter such as info,librqbit=debug - not {:?}",
            LEVELS.join(", "),
            value
        ),
    };
    Ok(Some(spec))
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
    match core::dbcli::open(&env) {
        Ok(db) => load_translator(&env, &Configuration::new(db)),
        Err(_) => Translator::load(&env.get_lang_path(), DEFAULT_LOCALE),
    }
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
//
// The early return below is load-bearing on exactly one build - the Windows
// release one, which is the only configuration where the message box after it
// is compiled in at all. Every other build ends at that return, which is why
// clippy calls it needless.
#[allow(clippy::needless_return)]
fn fatal_error(msg: &str) {
    // Free, and the right channel whenever anyone is attached to it.
    eprintln!("NanoTorrent could not start: {msg}");

    // Launched through nanotorrent-cli, which attaches a console and sets this
    // before spawning us. The eprintln above has somewhere to go, so a modal
    // box would be pure obstruction - and worse than that in a script, where
    // it blocks until somebody clicks it.
    //
    // A headless run is the same case without the shim: started from a
    // service manager or a scheduled task, a box would wait forever.
    if std::env::var_os("NANOTORRENT_CONSOLE").is_some() || headless() {
        return;
    }

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
        use crate::core::utils::wide;
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

    let mut args: Vec<String> = std::env::args().skip(1).collect();

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

    // `--headless`: no window for this run. Taken out of the arguments like
    // `--portable` below, and first, so that every failure from here on is
    // printed rather than put in a box nobody is there to dismiss.
    if let Some(at) = args.iter().position(|a| a == "--headless") {
        args.remove(at);
        HEADLESS_FLAG.store(true, Ordering::Relaxed);
    }

    // `--log-level LEVEL` or `--log-level=LEVEL`: for this run only, and
    // taken out for the same reason. A bad value is a usage error, said
    // before anything has started.
    let log_spec = match take_log_level(&mut args) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(2);
        }
    };

    // `--portable`: switch this copy to portable mode for good, then carry on
    // starting - in portable mode, since the marker is now there. Taken out of
    // the arguments first, so it is neither forwarded to a running instance nor
    // read as the name of a torrent to open.
    //
    // Before everything that picks a profile: the IPC port, the database and
    // the settings flags below all depend on which profile this is.
    if let Some(at) = args.iter().position(|a| a == "--portable") {
        args.remove(at);
        match core::portable::enable() {
            Ok(core::portable::Switched::Now(dir)) => println!(
                "NanoTorrent is portable now: its settings, torrents and logs are kept in\n  {}\n\
                 Delete portable.txt there to go back.",
                dir.display()
            ),
            Ok(core::portable::Switched::Already(dir)) => {
                println!("NanoTorrent is already portable, keeping its profile in {}", dir.display())
            }
            // Shown, not just printed: this is usually run from a shortcut,
            // where there is no terminal to print to.
            Err(err) => {
                fatal_error(&format!("{err:#}"));
                std::process::exit(2);
            }
        }
    }

    // BEFORE the IPC check below, which forwards argv to a running instance and
    // exits - a --set-web-password would otherwise be handed to the running
    // window as though it were a torrent to open.
    //
    // Their errors do NOT go through `?`. A rejected value is a usage error,
    // not a failure to start: bubbling it up would print it under "NanoTorrent
    // could not start" and, in a release GUI build, put it in a modal box that
    // a script has nobody to dismiss.
    //
    // One at a time, stopping at the first that takes the flag: each may consume
    // stdin or write a setting, so a later one must not run after an earlier
    // one handled it. dbcli comes before anything else opens the database: its
    // flags rewrite the file, and doing that under a live connection is the one
    // thing they must not do.
    for handle in [webui::cli::handle, core::dbcli::handle, cli::handle] {
        match handle(&args) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => {
                eprintln!("{err:#}");
                std::process::exit(2);
            }
        }
    }

    // Before the single-instance check: which instance to hand off to depends on
    // which profile this is - a portable copy has a port of its own.
    let env = Arc::new(Environment::create());

    // Port of the IPC single-instance handling in main.cpp.
    let server = match ipc::init(&args, &env)? {
        ipc::Instance::Primary(server) => Some(server),
        // Asked for a headless run and got a hand-off instead: say so, or the
        // command just ends as though it had started and stopped again. Any
        // torrents named went to the running copy, as they would without
        // the flag. The flag, not `headless()`: on a build without a window
        // a plain launch is a hand-off like anywhere else.
        ipc::Instance::Secondary if HEADLESS_FLAG.load(Ordering::Relaxed) => anyhow::bail!(
            "NanoTorrent is already running with this profile, so a headless one was not \
             started. Stop the running one first."
        ),
        ipc::Instance::Secondary => {
            // Options were forwarded to the running instance.
            return Ok(());
        }
    };

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

    {
        use std::io::IsTerminal;
        use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

        // Everything NanoTorrent itself does is logged at debug; dependencies
        // stay at info, because librqbit traces every chunk of every piece and
        // one busy torrent would bury the app's own activity in minutes.
        //
        // `--log-level` overrides that for one run, and RUST_LOG for any run
        // it is set for, when a specific dependency needs watching, e.g.
        // RUST_LOG=info,librqbit=debug,nanotorrent=trace. The flag wins: it is
        // the more deliberate of the two.
        let filter = match &log_spec {
            // Checked when the flag was read, so this cannot fail.
            Some(spec) => EnvFilter::new(spec),
            None => EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
        };

        // Which module a line came from - the whole point of a debug log is
        // being able to follow one subsystem through it.
        let file = log_file.map(|file| {
            tracing_subscriber::fmt::layer()
                .with_writer(Mutex::new(file))
                .with_ansi(false)
                .with_target(true)
        });
        // Headless, the console is where someone is watching - or where a
        // service manager collects it, journald for one. Stderr, so it does
        // not mix into what the run prints on stdout. Colour only on a Unix
        // terminal: a Windows console reached by AttachConsole has no VT
        // processing switched on, and shows the escapes as text.
        let console = headless().then(|| {
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(cfg!(unix) && std::io::stderr().is_terminal())
                .with_target(true)
        });

        // set_global_default rather than `init`: with tracing-subscriber's
        // default features, `init` would also route the `log` crate into
        // tracing, which this has never done and which several dependencies
        // would have plenty to say through.
        let subscriber = tracing_subscriber::registry().with(filter).with(file).with(console);
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    // Version and build first: a log that does not say which binary produced
    // it is guesswork the moment there is more than one build around.
    tracing::info!(
        "NanoTorrent {} ({}) starting up...",
        buildinfo::version(),
        buildinfo::build_stamp()
    );

    // ...and which install this is, for the same reason. A Microsoft Store
    // copy and an installer copy can sit on the same machine at once, and only
    // one of them is the one someone is looking at. It also decides where the
    // update prompt points - see updatechecker::download_url.
    if let Some(pfn) = core::environment::package_family_name() {
        tracing::info!("packaged install (Microsoft Store): {pfn}");
    }

    // A new portable copy, with an ordinary profile on this machine: offer to
    // bring it across, once. After the single-instance check, so only the copy
    // that is going to run asks; before the database is opened, because
    // opening it creates the empty one that would make the question moot.
    //
    // And after logging and the panic hook, not before: this shows a window,
    // and on a Windows release build - which has no console - anything that
    // goes wrong before those exist leaves no trace at all. The logs folder
    // it creates is not what makes a profile; the copy skips it.
    if let Some(from) = core::portable::copy_candidate(&env) {
        offer_profile_copy(&env, &from);
    }

    // Claim the identity notifications are attributed to: an AppUserModelID on
    // Windows, the bundle identifier on macOS. Nothing to do on Linux. A
    // headless run shows none, and on Windows this writes a Start menu
    // shortcut, which is not something a background service should leave.
    if !headless() {
        core::toast::register();
    }

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

    // The watched folder and moving finished downloads. Started here rather
    // than inside `Session::new` because it needs the `Arc` - it calls the
    // session's synchronous API, which cannot run on the session's own runtime.
    bittorrent::manager::spawn(&session, cfg.clone());

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
        Err(err) if headless() => {
            return Err(err.context("the web interface is the only way into a headless run"));
        }
        Err(err) => {
            tracing::error!("web interface did not start: {err:#}");
            None
        }
    };

    // Plugins last: they subscribe to session events and may call straight
    // back into it, so everything they can reach has to exist first. Off
    // unless plugins.enabled, and never fatal - see plugins::reload.
    //
    // The example is written before the host starts, but it is switched off,
    // so this start will not load it either way.
    plugins::seed_examples(&plugins::plugin_dir(&env), &cfg);
    // A grant is keyed by name, so a deleted plugin's approval must not be
    // waiting for the next file that happens to use the same one.
    plugins::prune_grants(&env, &cfg);
    plugins::reload(session.clone(), cfg.clone(), env.clone());

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

/// Ask whether a new portable copy should start from the ordinary profile in
/// `from`, and copy it across if so.
///
/// What it has to say goes to the log - or, for the one failure that matters,
/// on screen as well.
fn offer_profile_copy(env: &Environment, from: &std::path::Path) {
    let staged = match core::portable::Staged::new(from) {
        Ok(staged) => staged,
        // Nothing that cannot be read is worth offering. A fresh start is what
        // would have happened without the question, so that is what happens.
        Err(err) => {
            tracing::warn!(
                "the profile in {} could not be read, so this portable copy starts fresh: {err:#}",
                from.display()
            );
            return;
        }
    };
    // Asked in the language that profile uses - it is the user's, and this
    // copy has no settings of its own yet to ask in.
    let locale = staged.locale().unwrap_or_else(|| String::from(DEFAULT_LOCALE));
    let tr = Translator::load(&env.get_lang_path(), &locale);
    if !ask_profile_copy(&tr, env, staged.source()) {
        return;
    }
    match staged.install(env) {
        Ok(files) => tracing::info!(
            "copied the profile from {} into {} ({files} files)",
            from.display(),
            env.get_application_data_path().display()
        ),
        // Stopping beats starting on a partial profile. The database goes in
        // last, so the next start finds none and offers the copy again.
        Err(err) => {
            tracing::error!("copying the profile from {} failed: {err:#}", from.display());
            fatal_error(&format!(
                "copying your profile from {} did not finish: {err:#}\n\n\
                 The original is untouched. Start NanoTorrent again to try once more.",
                from.display()
            ));
            std::process::exit(1);
        }
    }
}

/// Headless, there is nobody to ask, and copying unasked would be the one
/// thing worse than not copying. Says how to do it by hand instead.
#[cfg_attr(not(feature = "ui-slint"), allow(unused_variables))]
fn ask_profile_copy(tr: &Translator, env: &Environment, from: &std::path::Path) -> bool {
    #[cfg(feature = "ui-slint")]
    if !headless() {
        return ui_slint::ask_profile_copy(tr, &env.get_application_data_path(), from);
    }
    println!(
        "This portable copy starts with a new profile. To use the one in\n  {}\n\
         instead, stop NanoTorrent, delete NanoTorrent.sqlite in\n  {}\n\
         and start it again with its window, which offers to copy it.",
        from.display(),
        env.get_application_data_path().display()
    );
    false
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

/// Show the main window and run the UI event loop until it closes - or, for a
/// headless run, [`run_headless`].
fn run_ui(ctx: AppContext) -> anyhow::Result<()> {
    #[cfg(feature = "ui-slint")]
    if !headless() {
        // Every way out of the window - its X, File > Exit, the tray - ends
        // the event loop and comes back here, so this is the one place to
        // stop the session. It was not done at all: the window has exited
        // without `stop` since v0.2.0, leaving the engine's resume state as
        // of its last periodic save rather than as of the exit.
        let session = ctx.session.clone();
        let result = ui_slint::run(ctx);
        tracing::info!("window closed, shutting down");
        session.stop();
        return result;
    }
    run_headless(ctx)
}

/// How often a headless run does what the window's refresh tick does.
const HEADLESS_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// No window: a build without one, or `--headless`. Keep the session running
/// until asked to stop, then shut it down cleanly so fastresume state is
/// flushed - a torrent client that exits without that makes every torrent
/// recheck on next start.
///
/// The tick stands in for the window's refresh timer, for the two jobs on it
/// that are not drawing: the queue limits, and the arguments a later launch
/// hands over.
fn run_headless(ctx: AppContext) -> anyhow::Result<()> {
    let session = &ctx.session;
    tracing::info!("running headless - Ctrl-C to stop");
    println!("{}", ctx.translator.i18n("cli_headless_running"));
    if ctx.web.is_none() {
        println!("{}", ctx.translator.i18n("cli_headless_no_webui"));
    }

    // Before the first add, so that add is reported too.
    let events = session.subscribe();
    add_from_args(session, &ctx.args);

    // The signals are awaited on the session's runtime and reported over a
    // channel, so this thread stays outside it: the session's synchronous
    // API, which the tick calls, cannot be called from inside its own runtime.
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    spawn_stop_signals(&session.handle(), stop_tx);

    // Ends on a signal, or with every listener gone - which would leave
    // nothing able to stop this cleanly, so it stops while it still can.
    use std::sync::mpsc::RecvTimeoutError;
    while let Err(RecvTimeoutError::Timeout) = stop_rx.recv_timeout(HEADLESS_TICK) {
        // A later launch hands over its arguments rather than starting a
        // second session on the same profile - see ipc::init.
        while let Some(forwarded) = ctx.ipc.as_ref().and_then(|s| s.try_recv().ok()) {
            if forwarded.is_empty() {
                tracing::info!("another launch asked for the window; this run has none");
            }
            add_from_args(session, &forwarded);
        }

        // What the window says in a toast, said on the console: with no
        // window, an add that failed would otherwise only be in the log.
        while let Ok(event) = events.try_recv() {
            use bittorrent::session::SessionEvent;
            let tr = &ctx.translator;
            match event {
                SessionEvent::TorrentAdded { name, .. } => {
                    println!("{}", tr.i18n1("torrent_added", &name))
                }
                SessionEvent::TorrentDuplicate { name, .. } => {
                    println!("{}", tr.i18n1("torrents_duplicate", &name))
                }
                SessionEvent::TorrentCompleted { name, .. } => {
                    println!("{}: {name}", tr.i18n("download_complete"))
                }
                SessionEvent::Error(err) => eprintln!("{}", err.text(tr)),
                SessionEvent::TorrentRemoved { .. } => {}
            }
        }

        // `torrents` is also what stamps a completion time, which the
        // window's list does for it. And the queue limits run on what it
        // returns - see the same call in ui_slint's refresh.
        let rows = session.torrents(&session.label_names());
        session.enforce_queue(
            &rows,
            ctx.cfg.get_int("libtorrent.active_limit").unwrap_or(0),
            ctx.cfg.get_int("libtorrent.active_downloads").unwrap_or(0),
            ctx.cfg.get_int("libtorrent.active_seeds").unwrap_or(0),
        );
    }

    tracing::info!("shutting down");
    println!("{}", ctx.translator.i18n("cli_headless_stopping"));
    session.stop();
    Ok(())
}

/// Add the `.torrent` files and magnet links in `args`, as the Add dialog
/// would with "skip this dialog" ticked: the default save path, started.
/// Anything else in `args` is ignored, as the window ignores it.
fn add_from_args(session: &bittorrent::session::Session, args: &[String]) {
    use bittorrent::session::{AddParams, AddTorrentSource};
    for arg in args {
        let source = if arg.starts_with("magnet:") {
            AddTorrentSource::MagnetUri(arg.clone())
        } else if arg.to_lowercase().ends_with(".torrent") {
            match std::fs::read(arg) {
                Ok(bytes) => AddTorrentSource::TorrentFileBytes(bytes),
                Err(err) => {
                    tracing::error!("cannot read {arg}: {err}");
                    eprintln!("{arg}: {err}");
                    continue;
                }
            }
        } else {
            continue;
        };
        let params = AddParams {
            start_torrent: true,
            ..AddParams::default()
        };
        session.add_torrent(source, params);
    }
}

/// Send on `tx` when this process is asked to stop: Ctrl-C everywhere, and
/// what a service manager or a closing terminal sends instead - SIGTERM and
/// SIGHUP on Unix, Ctrl-Break and the console closing on Windows.
///
/// One task per signal rather than a `select!`, which would need tokio's
/// macros for this one place. A listener that cannot be set up is logged and
/// parks holding its sender, so a missing one never reads as a request to stop.
fn spawn_stop_signals(rt: &tokio::runtime::Handle, tx: std::sync::mpsc::Sender<()>) {
    fn on<F, E>(
        rt: &tokio::runtime::Handle,
        tx: &std::sync::mpsc::Sender<()>,
        name: &'static str,
        wait: F,
    ) where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send,
    {
        let tx = tx.clone();
        rt.spawn(async move {
            match wait.await {
                Ok(()) => {
                    tracing::info!("{name} received");
                    let _ = tx.send(());
                }
                Err(err) => {
                    tracing::warn!("could not listen for {name} ({err})");
                    std::future::pending::<()>().await;
                }
            }
        });
    }

    on(rt, &tx, "Ctrl-C", tokio::signal::ctrl_c());

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // `signal` registers with the runtime, so it is called on it.
        let kinds = [("SIGTERM", SignalKind::terminate()), ("SIGHUP", SignalKind::hangup())];
        for (name, kind) in kinds {
            on(rt, &tx, name, async move {
                signal(kind)?.recv().await;
                Ok::<(), std::io::Error>(())
            });
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close};
        on(rt, &tx, "Ctrl-Break", async {
            ctrl_break()?.recv().await;
            Ok::<(), std::io::Error>(())
        });
        // Windows ends the process a few seconds after this, whatever it is
        // doing; started now, the shutdown at least gets those seconds.
        on(rt, &tx, "console close", async {
            ctrl_close()?.recv().await;
            Ok::<(), std::io::Error>(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(args: &[&str]) -> (anyhow::Result<Option<String>>, Vec<String>) {
        let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let spec = take_log_level(&mut args);
        (spec, args)
    }

    /// Both spellings are taken out with their value, and nothing else is.
    #[test]
    fn log_level_is_taken_out_of_the_arguments() {
        let (spec, rest) = take(&["--headless", "--log-level", "warn", "a.torrent"]);
        assert_eq!(spec.unwrap().as_deref(), Some("warn"));
        assert_eq!(rest, ["--headless", "a.torrent"]);

        let (spec, rest) = take(&["--log-level=ERROR", "a.torrent"]);
        assert_eq!(spec.unwrap().as_deref(), Some("error"));
        assert_eq!(rest, ["a.torrent"]);

        let (spec, rest) = take(&["a.torrent"]);
        assert!(spec.unwrap().is_none());
        assert_eq!(rest, ["a.torrent"]);
    }

    /// debug and trace raise NanoTorrent alone: the engine at debug would
    /// bury it.
    #[test]
    fn a_fine_level_is_for_nanotorrent_only() {
        let (spec, _) = take(&["--log-level", "trace"]);
        assert_eq!(spec.unwrap().as_deref(), Some("info,nanotorrent=trace"));
    }

    /// A filter goes through as written; a bad one, a misspelt level and a
    /// missing value are refused rather than logging something unexpected.
    #[test]
    fn a_filter_passes_and_nonsense_does_not() {
        let (spec, _) = take(&["--log-level", "info,librqbit=debug"]);
        assert_eq!(spec.unwrap().as_deref(), Some("info,librqbit=debug"));

        for bad in [&["--log-level", "loud"][..], &["--log-level", "librqbit=loud"], &["--log-level"]] {
            assert!(take(bad).0.is_err(), "{bad:?} was accepted");
        }
    }
}
