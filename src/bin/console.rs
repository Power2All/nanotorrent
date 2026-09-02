//! `nanotorrent-cli.exe` - the console-subsystem launcher.
//!
//! This is the binary a shell finds; the application itself is
//! `nanotorrent-gui.exe`, built from `main.rs`. The split exists because cmd and
//! PowerShell decide whether to wait for a process from the PE subsystem field
//! alone, before any of its code runs, and they never wait for a GUI program.
//! `nanotorrent --help` therefore got its prompt back before the help text
//! arrived, leaving the cursor on what looked like a hung command. Attaching
//! to the parent console fixes *where* the text goes; nothing inside a GUI
//! binary can fix *when* the shell returns.
//!
//! So the name the shell resolves has to belong to a console program. It
//! forwards argv to the GUI binary, which inherits this console, and waits -
//! so the shell waits too. Python ships python.exe / pythonw.exe for exactly
//! this reason.
//!
//! Deliberately std-only: this is on the path of every command-line
//! invocation, so it has nothing to initialise and nothing to go wrong.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(mut path) => {
            // Next to this file, whatever the install directory is called.
            path.set_file_name("nanotorrent-gui.exe");
            path
        }
        Err(err) => {
            eprintln!("cannot locate nanotorrent-gui.exe: {err}");
            return ExitCode::from(1);
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Wait only for the flags, which print and exit. A torrent, a magnet or a
    // bare launch opens the window instead, and holding the console for the
    // whole session would be worse than the problem this solves.
    let is_flag_run = args.iter().any(|a| a.starts_with('-'));

    // Tell the child a console is attached. Reached this way, a startup
    // failure should print, not put up a modal box nobody asked for - and this
    // is a fact the shim knows for certain, where the GUI process can only
    // guess (GetConsoleWindow lies under MinTTY; see fatal_error).
    let mut child = match Command::new(&exe)
        .args(&args)
        .env("NANOTORRENT_CONSOLE", "1")
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("cannot run {}: {err}", exe.display());
            return ExitCode::from(1);
        }
    };

    if !is_flag_run {
        return ExitCode::SUCCESS;
    }

    match child.wait() {
        // Anything that does not fit a u8 - which on Windows includes the
        // 0xC000_0000 exception codes - is reported as a plain failure.
        Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(err) => {
            eprintln!("cannot wait for nanotorrent-gui.exe: {err}");
            ExitCode::from(1)
        }
    }
}
