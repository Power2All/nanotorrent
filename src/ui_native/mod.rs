// Native Win32 UI built with native-windows-gui - the same framework family
// (Win32 common controls) the original wxWidgets client rendered through.

mod darkmode;
mod dialogs;
mod mainwindow;

use native_windows_gui as nwg;

use crate::AppContext;

pub fn run(ctx: AppContext) -> anyhow::Result<()> {
    // Make the process DPI aware, like the original (which declared it in the
    // application manifest). NWG's high-dpi feature scales all logical
    // coordinates by the real DPI, but only does anything useful when the
    // process is actually DPI aware - nwg::init() itself never sets it.
    unsafe {
        winapi::um::winuser::SetProcessDPIAware();
    }

    nwg::init().map_err(|err| anyhow::anyhow!("failed to init NWG: {err}"))?;

    // Global default font. set_global_family alone creates a font with the
    // default (small, non-DPI-scaled) size, which made controls built with
    // the default font visibly smaller than the explicit caption fonts -
    // give it the same logical size 16 (scaled by the real DPI) everywhere.
    let mut default_font = nwg::Font::default();
    nwg::Font::builder()
        .family("Segoe UI")
        .size(16)
        .build(&mut default_font)
        .map_err(|err| anyhow::anyhow!("failed to create default font: {err}"))?;
    nwg::Font::set_global_default(Some(default_font));

    let start_position = ctx.cfg.get_int("start_position").unwrap_or(0);
    let has_tray = ctx.cfg.get_bool("show_in_notification_area");

    let window = mainwindow::MainWindow::build(ctx)?;

    // Port of the start_position setting (Normal / Minimized / Hidden /
    // Maximized).
    match start_position {
        1 => window.window.minimize(),
        // Hidden is only safe when a tray icon exists to bring the window
        // back; otherwise fall through to a normal window (matches the
        // original, which showed the frame when there was no notify icon).
        2 if has_tray => window.window.set_visible(false),
        3 => window.window.maximize(),
        _ => {}
    }

    nwg::dispatch_thread_events();

    Ok(())
}
