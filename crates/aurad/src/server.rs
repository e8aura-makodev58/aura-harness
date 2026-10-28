//! Unix-socket server. One thread per connection; the engine sits behind a
//! mutex and each request holds it only while it runs.

use crate::api::{self, Request, Response};
use crate::engine::Engine;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

/// Binds the socket and serves requests until the listener fails.
pub fn serve(socket: &Path, engine: Arc<Mutex<Engine>>) -> std::io::Result<()> {
    // sockaddr_un holds at most 108 bytes including the terminating NUL.
    if socket.as_os_str().len() >= 108 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is too long; choose a shorter state_dir",
                socket.display()
            ),
        ));
    }
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    // Only the owner may connect.
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    for stream in listener.incoming() {
        let stream = stream?;
        let engine = Arc::clone(&engine);
        thread::spawn(move || {
            let _ = handle_connection(stream, &engine);
        });
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, engine: &Mutex<Engine>) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let mut guard = match engine.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                api::handle(&mut guard, request)
            }
            Err(err) => Response::err(format!("bad request: {err}")),
        };
        let mut text = serde_json::to_string(&response).unwrap_or_else(|_| "{\"ok\":false}".into());
        text.push('\n');
        writer.write_all(text.as_bytes())?;
    }
    Ok(())
}
