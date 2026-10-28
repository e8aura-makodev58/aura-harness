//! Talks to `aurad` over its Unix socket: one JSON request per line, one JSON
//! response per line.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: PathBuf) -> Client {
        Client { socket }
    }

    pub fn call(&self, cmd: &str, args: Value) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket).with_context(|| {
            format!(
                "cannot reach aurad at {} (is the daemon running?)",
                self.socket.display()
            )
        })?;
        let request = json!({ "cmd": cmd, "args": args });
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.is_empty() {
            bail!("aurad closed the connection without answering");
        }
        let response: Value = serde_json::from_str(&line).context("aurad sent invalid JSON")?;
        if response["ok"].as_bool() == Some(true) {
            Ok(response["data"].clone())
        } else {
            Err(anyhow!(
                "{}",
                response["error"].as_str().unwrap_or("request failed")
            ))
        }
    }
}
