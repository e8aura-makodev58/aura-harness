//! `aurad`: the Aura daemon. Runs the scheduler on one thread and serves the
//! local API on the socket. Meant to run as a systemd user service.

use anyhow::{Context, Result};
use aura_core::Config;
use aurad::engine::Engine;
use aurad::server;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TICK: Duration = Duration::from_millis(500);

fn usage() -> ! {
    eprintln!("usage: aurad [--config <path>]");
    std::process::exit(2);
}

/// Takes an exclusive, non-blocking lock. The returned file keeps it held.
fn acquire_lock(path: &Path) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: flock on a file we own; the descriptor stays valid while `file` lives.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        anyhow::bail!(
            "another aurad is already running on {}",
            path.parent().unwrap_or(path).display()
        );
    }
    Ok(file)
}

fn main() -> Result<()> {
    let mut config_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage()))
            }
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }

    let config = Config::load(config_path.as_deref()).context("loading configuration")?;
    let paths = config.paths();
    fs::create_dir_all(&paths.state_dir)?;
    // Held for the life of the process. A second daemon on the same state
    // would race this one for runs and merges, so it must not start.
    let _lock = acquire_lock(&paths.state_dir.join("aurad.lock"))?;
    let socket = paths.socket.clone();
    let engine = Engine::open(config).context("starting engine")?;
    eprintln!(
        "aurad: repo {}, state {}",
        engine.repo.display(),
        engine.paths.state_dir.display()
    );

    let engine = Arc::new(Mutex::new(engine));

    let ticker = Arc::clone(&engine);
    std::thread::spawn(move || loop {
        {
            let mut guard = match ticker.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Err(err) = guard.tick() {
                eprintln!("aurad: tick failed: {err:#}");
            }
        }
        std::thread::sleep(TICK);
    });

    server::serve(&socket, engine).with_context(|| format!("serving {}", socket.display()))?;
    Ok(())
}
