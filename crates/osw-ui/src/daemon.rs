//! Client for the daemon's control socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Sends one command and returns the daemon's reply line.
pub fn send(cmd: &str) -> Result<String, String> {
    let path = osw_core::paths::socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|_| "OpenSuperWhisper is not running".to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    stream.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).map_err(|e| e.to_string())?;
    let reply = reply.trim().to_string();
    if reply.starts_with("unknown command") {
        Err(reply)
    } else {
        Ok(reply)
    }
}

pub fn is_running() -> bool {
    UnixStream::connect(osw_core::paths::socket_path()).is_ok()
}

/// Starts the daemon next to this binary, or from PATH.
pub fn start() -> std::io::Result<()> {
    let sibling = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("osw-daemon")));
    let program = sibling.filter(|p| p.is_file()).unwrap_or_else(|| "osw-daemon".into());
    std::process::Command::new(program).spawn().map(drop)
}

/// Microphones as (PipeWire node name, description).
pub fn microphones() -> Vec<(String, String)> {
    let Ok(reply) = send("mics") else { return Vec::new() };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&reply) else { return Vec::new() };
    let mut mics: Vec<(String, String)> = items
        .iter()
        .filter_map(|m| Some((m["name"].as_str()?.to_string(), m["description"].as_str()?.to_string())))
        .collect();
    mics.sort_by(|a, b| a.1.cmp(&b.1));
    mics
}
