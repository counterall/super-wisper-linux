//! Control socket: one-line commands such as `toggle`, used by `osw toggle`
//! so recording can also be bound in the desktop's own shortcut settings.
//! A live socket also marks the single running instance.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};

use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcCmd {
    Toggle,
    Start,
    Stop,
    Cancel,
    Reload,
    Quit,
}

impl IpcCmd {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "toggle" => Self::Toggle,
            "start" => Self::Start,
            "stop" => Self::Stop,
            "cancel" => Self::Cancel,
            "reload" => Self::Reload,
            "quit" => Self::Quit,
            _ => return None,
        })
    }
}

pub fn listen(on_cmd: impl Fn(IpcCmd) + Send + 'static) -> Result<()> {
    let path = osw_core::paths::socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!("OpenSuperWhisper is already running. Use `osw toggle` to record, `osw quit` to stop it.");
    }
    let _ = std::fs::remove_file(&path); // stale socket from a crash
    let listener = UnixListener::bind(&path)?;

    std::thread::Builder::new().name("ipc".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            let mut reader = BufReader::new(&stream);
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let reply = match IpcCmd::parse(&line) {
                Some(cmd) => {
                    on_cmd(cmd);
                    "ok\n".to_string()
                }
                None => format!("unknown command: {}\n", line.trim()),
            };
            let _ = (&stream).write_all(reply.as_bytes());
        }
    })?;
    Ok(())
}

pub fn remove_socket() {
    let _ = std::fs::remove_file(osw_core::paths::socket_path());
}
