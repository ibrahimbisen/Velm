//! Vellum — the board editor's entry point.
//!
//! Everything of substance is in the library beside this file; `main` parses the
//! command line, starts the event loop and re-raises whatever the loop could not.
//!
//! Nothing here is macOS-specific. It runs on Metal today and on DX12 or Vulkan by
//! changing nothing — `WGPU_BACKEND=vulkan` picks a different one on the same binary
//! — because Windows is a hard requirement later and a late port is how that
//! requirement gets missed.

use anyhow::{Context, Result};
use vellum_app::options::{Command, HELP, parse_args};
use winit::event_loop::{ControlFlow, EventLoop};

fn main() -> Result<()> {
    // Default to info so the adapter, the backend and the frame statistics are
    // visible without the user having to know about RUST_LOG.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let options = match parse_args(std::env::args().skip(1))? {
        Command::ShowHelp => {
            print!("{HELP}");
            return Ok(());
        }
        Command::Run(options) => *options,
    };

    // ⚠ **Here, and it has to be here.** `take_sync_token` calls `std::env::remove_var`,
    // which is undefined behaviour if another thread reads the environment concurrently — and
    // `getenv` is called by more things than it looks, SQLite's temporary-file lookup among
    // them. This process is single-threaded at exactly this point and stops being so a few
    // lines below, once a board is opened and its autosave writer starts.
    //
    // Why it is removed at all: an environment is inherited, and this application launches
    // `claude`, `codex` and a PTY the user can type into. See the function's own note.
    let _ = vellum_app::options::take_sync_token();

    let event_loop = EventLoop::new().context("could not create the event loop")?;
    // Poll rather than Wait: the canvas redraws continuously so that a pan reaches
    // the screen on the next vblank rather than on the next event, and so an
    // inertial glide has a clock to run against.
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut vellum = vellum_app::Vellum::new(options);
    event_loop.run_app(&mut vellum).context("event loop failed")?;

    // Startup errors cannot escape `ApplicationHandler`, so they are re-raised here
    // and become the process exit code. Without this a failed GPU init would exit 0
    // with nothing but a log line.
    match vellum.take_startup_error() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
