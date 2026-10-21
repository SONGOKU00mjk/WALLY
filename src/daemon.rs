//! The opt-in daemon's control socket: a plain Unix stream socket under
//! `$XDG_RUNTIME_DIR`, one connection per request. The wire format is one
//! line of NUL-separated argv — `set`, `query`, `restore` or `stop`,
//! followed by the arguments the client would have passed. Trivial to
//! speak with `printf`, trivial to parse with std.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// The control action a client asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Set,
    Query,
    Restore,
    Stop,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Query => "query",
            Self::Restore => "restore",
            Self::Stop => "stop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "set" => Some(Self::Set),
            "query" => Some(Self::Query),
            "restore" => Some(Self::Restore),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// Per-session socket path: `$XDG_RUNTIME_DIR/wally-<display>.sock`. The
/// display name is sanitized to a filesystem-safe slug so two sessions
/// (two users, a nested compositor) never fight over one socket.
pub fn socket_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        anyhow::anyhow!("XDG_RUNTIME_DIR is not set — cannot place the control socket")
    })?;
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let slug: String = display
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut path = PathBuf::from(runtime);
    path.push(format!("wally-{slug}.sock"));
    Ok(path)
}

/// Bind the control socket. A stale socket from a dead daemon is removed
/// first (it is in a per-session runtime directory that is wiped on
/// logout; a live daemon's socket would be actively listening and is
/// tested below).
pub fn listen() -> Result<std::os::unix::net::UnixListener> {
    let path = socket_path()?;
    if path.exists() {
        // If something is listening, refuse instead of stealing its socket.
        if UnixStream::connect(&path).is_ok() {
            bail!(
                "another wally daemon is listening on {} — use `wally stop` first",
                path.display()
            );
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("could not remove the stale socket `{}`", path.display()))?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .with_context(|| format!("could not bind the control socket `{}`", path.display()))?;
    Ok(listener)
}

/// One request off the wire: the action and its remaining argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub action: Action,
    pub args: Vec<String>,
}

pub fn encode(action: Action, args: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(action.name().as_bytes());
    for a in args {
        out.push(0);
        out.extend_from_slice(a.as_bytes());
    }
    out.push(b'\n');
    out
}

pub fn decode(bytes: &[u8]) -> Option<Request> {
    let line = bytes.split(|&b| b == b'\n').next()?;
    let mut parts = line.split(|&b| b == 0);
    let first = parts.next()?;
    let action = Action::parse(std::str::from_utf8(first).ok()?)?;
    let args = parts
        .map(|a| std::str::from_utf8(a).ok().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    Some(Request { action, args })
}

/// Client side: send one request and read the daemon's reply (a short
/// text blob, one connection per request).
pub fn transact(action: Action, args: &[String]) -> Result<String> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "no wally daemon answered on {} — start one with `wally --daemon -i ...`",
            path.display()
        )
    })?;
    stream
        .write_all(&encode(action, args))
        .context("could not send the request")?;
    let mut reply = String::new();
    stream
        .take(64 * 1024)
        .read_to_string(&mut reply)
        .context("could not read the daemon's reply")?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let args = vec![
            "-i".to_string(),
            "~/new wall.png".to_string(),
            "-m".to_string(),
            "tile".to_string(),
        ];
        let bytes = encode(Action::Set, &args);
        assert!(bytes.ends_with(b"\n"));
        let req = decode(&bytes).expect("parses");
        assert_eq!(req.action, Action::Set);
        assert_eq!(req.args, args);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"bogus\n").is_none());
        assert!(decode(b"").is_none());
        assert!(decode(b"stop").is_some()); // no trailing newline is fine
    }

    #[test]
    fn socket_path_is_display_scoped() {
        // Only this test touches XDG_RUNTIME_DIR; give it a value when
        // the sandbox has none. The assertion is about the file NAME,
        // not the directory.
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        }
        let p = socket_path().expect("XDG_RUNTIME_DIR set above");
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        let slug: String = display
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        assert!(p.file_name().unwrap().to_str().unwrap().contains(&slug));
    }
}
