//! Control socket: one command per connection, one line each, such as
//! `toggle` (used by `osw toggle`, so recording can also be bound in the
//! desktop's own shortcut settings) or `transcribe-file <path>` (used by the
//! UI). A live socket also marks the single running instance.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};

use crate::capture::Microphone;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpcCmd {
    Toggle,
    Start,
    Stop,
    Cancel,
    Reload,
    Quit,
    TranscribeFile(PathBuf),
    Retranscribe(String),
}

impl IpcCmd {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
        Some(match (cmd, arg) {
            ("toggle", "") => Self::Toggle,
            ("start", "") => Self::Start,
            ("stop", "") => Self::Stop,
            ("cancel", "") => Self::Cancel,
            ("reload", "") => Self::Reload,
            ("quit", "") => Self::Quit,
            ("transcribe-file", path) if !path.is_empty() => Self::TranscribeFile(path.into()),
            ("retranscribe", id) if !id.is_empty() => Self::Retranscribe(id.into()),
            _ => return None,
        })
    }
}

pub fn listen(
    microphones: Arc<Mutex<Vec<Microphone>>>,
    on_cmd: impl Fn(IpcCmd) + Send + 'static,
) -> Result<()> {
    let path = osw_core::paths::socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!("OpenSuperWhisper is already running. Use `osw toggle` to record, `osw quit` to stop it.");
    }
    let _ = std::fs::remove_file(&path); // stale socket from a crash
    let listener = UnixListener::bind(&path)?;

    std::thread::Builder::new().name("ipc".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            let reply = if line.trim() == "mics" {
                microphones_json(&microphones.lock().unwrap())
            } else {
                match IpcCmd::parse(&line) {
                    Some(cmd) => {
                        on_cmd(cmd);
                        "ok".to_string()
                    }
                    None => format!("unknown command: {}", line.trim()),
                }
            };
            let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
        }
    })?;
    Ok(())
}

fn microphones_json(mics: &[Microphone]) -> String {
    let items: Vec<serde_json::Value> = mics
        .iter()
        .map(|m| serde_json::json!({ "name": m.name, "description": m.description }))
        .collect();
    serde_json::Value::Array(items).to_string()
}

pub fn remove_socket() {
    let _ = std::fs::remove_file(osw_core::paths::socket_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(IpcCmd::parse("toggle\n"), Some(IpcCmd::Toggle));
        assert_eq!(
            IpcCmd::parse("transcribe-file /tmp/a b.mp3\n"),
            Some(IpcCmd::TranscribeFile("/tmp/a b.mp3".into()))
        );
        assert_eq!(IpcCmd::parse("retranscribe abc"), Some(IpcCmd::Retranscribe("abc".into())));
        assert_eq!(IpcCmd::parse("transcribe-file "), None);
        assert_eq!(IpcCmd::parse("toggle extra"), None);
        assert_eq!(IpcCmd::parse("bogus"), None);
    }
}
