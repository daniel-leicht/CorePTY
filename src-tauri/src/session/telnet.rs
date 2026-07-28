// Telnet client over raw TCP with a small IAC option-negotiation state machine.
// Negotiates SGA, ECHO (server-side), TERMINAL-TYPE, and NAWS (window size);
// strips control sequences from the byte stream; escapes 0xFF on send.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use uuid::Uuid;

use super::{
    emit_data, emit_exit, emit_status, SessionInfo, SessionInput, SessionKind, SessionManager,
};

// Telnet command bytes
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

// Options
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;
const OPT_TTYPE: u8 = 24;
const OPT_NAWS: u8 = 31;

const TERM: &[u8] = b"xterm-256color";

/// Cap on a single subnegotiation payload. Real subnegotiations (TTYPE, NAWS…)
/// are a handful of bytes; this stops a hostile server from growing the buffer
/// without bound by never sending the closing `IAC SE`.
const MAX_SUBNEG: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelnetOptions {
    #[serde(default)]
    pub id: Option<String>,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default)]
    pub title: Option<String>,
}

fn default_port() -> u16 {
    23
}
fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

pub fn connect(
    app: AppHandle,
    manager: &SessionManager,
    opts: TelnetOptions,
) -> Result<SessionInfo, String> {
    let id = opts
        .id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if manager.exists(&id) {
        return Err(format!("session id already in use: {id}"));
    }
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("telnet {}", opts.host));
    let info = SessionInfo {
        id: id.clone(),
        kind: SessionKind::Telnet,
        title,
    };

    let (tx, rx) = unbounded_channel::<SessionInput>();
    manager.register(info.clone(), tx);
    emit_status(
        &app,
        &id,
        "connecting",
        Some(format!("{}:{}", opts.host, opts.port)),
    );

    let app_task = app.clone();
    let id_task = id.clone();
    tauri::async_runtime::spawn(async move {
        match run_session(app_task.clone(), id_task.clone(), opts, rx).await {
            Ok(()) => emit_exit(&app_task, &id_task, None, None),
            Err(e) => {
                emit_status(&app_task, &id_task, "error", Some(e.clone()));
                emit_exit(&app_task, &id_task, None, Some(e));
            }
        }
        app_task.state::<SessionManager>().remove(&id_task);
    });

    Ok(info)
}

enum Action {
    Net(std::io::Result<usize>),
    Input(Option<SessionInput>),
}

async fn run_session(
    app: AppHandle,
    id: String,
    opts: TelnetOptions,
    mut rx: UnboundedReceiver<SessionInput>,
) -> Result<(), String> {
    let stream = tokio::time::timeout(
        Duration::from_secs(20),
        TcpStream::connect((opts.host.as_str(), opts.port)),
    )
    .await
    .map_err(|_| "connection timed out".to_string())?
    .map_err(|e| format!("could not connect: {e}"))?;
    let _ = stream.set_nodelay(true);

    let (mut reader, mut writer) = stream.into_split();

    let mut parser = Telnet::new();
    let mut cols = opts.cols;
    let mut rows = opts.rows;

    // Proactively offer/ request the options we care about.
    let hello = parser.hello(cols, rows);
    if writer.write_all(&hello).await.is_err() {
        return Err("connection closed during negotiation".into());
    }

    emit_status(&app, &id, "connected", None);

    let mut buf = [0u8; 8192];
    loop {
        let action = tokio::select! {
            r = reader.read(&mut buf) => Action::Net(r),
            input = rx.recv() => Action::Input(input),
        };
        match action {
            Action::Net(Ok(0)) | Action::Net(Err(_)) => break,
            Action::Net(Ok(n)) => {
                let (data, replies) = parser.feed(&buf[..n], cols, rows);
                if !data.is_empty() {
                    emit_data(&app, &id, &data);
                }
                if !replies.is_empty() && writer.write_all(&replies).await.is_err() {
                    break;
                }
            }
            Action::Input(Some(SessionInput::Data(bytes))) => {
                let out = encode_output(&bytes);
                if writer.write_all(&out).await.is_err() {
                    break;
                }
            }
            Action::Input(Some(SessionInput::Resize { cols: c, rows: r })) => {
                cols = c;
                rows = r;
                if parser.naws_active {
                    let sb = naws_subneg(c, r);
                    if writer.write_all(&sb).await.is_err() {
                        break;
                    }
                }
            }
            Action::Input(Some(SessionInput::Close)) | Action::Input(None) => break,
        }
    }

    Ok(())
}

/// Translate outgoing keystrokes: escape IAC, and send CR as CR LF.
fn encode_output(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + 8);
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            IAC => {
                out.push(IAC);
                out.push(IAC);
            }
            b'\r' => {
                out.push(b'\r');
                out.push(b'\n');
                if i + 1 < input.len() && input[i + 1] == b'\n' {
                    i += 1; // collapse an existing CRLF
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

fn naws_subneg(cols: u16, rows: u16) -> Vec<u8> {
    let mut v = vec![IAC, SB, OPT_NAWS];
    for byte in [
        (cols >> 8) as u8,
        (cols & 0xff) as u8,
        (rows >> 8) as u8,
        (rows & 0xff) as u8,
    ] {
        if byte == IAC {
            v.push(IAC); // escape within subnegotiation
        }
        v.push(byte);
    }
    v.push(IAC);
    v.push(SE);
    v
}

#[derive(Clone, Copy)]
enum State {
    Data,
    Iac,
    Will,
    Wont,
    Do,
    Dont,
    Sb,
    SbData,
    SbIac,
}

struct Telnet {
    state: State,
    sub_opt: u8,
    sub: Vec<u8>,
    /// Options we have offered (WILL) and the peer accepted / we committed to.
    local: HashSet<u8>,
    /// Options the peer offered (WILL) that we enabled (DO).
    remote: HashSet<u8>,
    naws_active: bool,
}

impl Telnet {
    fn new() -> Self {
        Self {
            state: State::Data,
            sub_opt: 0,
            sub: Vec::new(),
            local: HashSet::new(),
            remote: HashSet::new(),
            naws_active: false,
        }
    }

    /// Initial proactive negotiation offers.
    fn hello(&mut self, cols: u16, rows: u16) -> Vec<u8> {
        let mut out = Vec::new();
        // We WILL: terminal-type, NAWS, SGA
        for opt in [OPT_TTYPE, OPT_NAWS, OPT_SGA] {
            self.local.insert(opt);
            out.extend_from_slice(&[IAC, WILL, opt]);
        }
        // We want the server to: echo, suppress go-ahead
        for opt in [OPT_ECHO, OPT_SGA] {
            self.remote.insert(opt);
            out.extend_from_slice(&[IAC, DO, opt]);
        }
        self.naws_active = true;
        out.extend_from_slice(&naws_subneg(cols, rows));
        out
    }

    /// Feed received bytes; returns (terminal data, bytes to send back).
    fn feed(&mut self, input: &[u8], cols: u16, rows: u16) -> (Vec<u8>, Vec<u8>) {
        let mut data = Vec::with_capacity(input.len());
        let mut reply = Vec::new();

        for &b in input {
            match self.state {
                State::Data => {
                    if b == IAC {
                        self.state = State::Iac;
                    } else {
                        data.push(b);
                    }
                }
                State::Iac => match b {
                    IAC => {
                        data.push(IAC); // escaped literal 0xFF
                        self.state = State::Data;
                    }
                    WILL => self.state = State::Will,
                    WONT => self.state = State::Wont,
                    DO => self.state = State::Do,
                    DONT => self.state = State::Dont,
                    SB => self.state = State::Sb,
                    _ => self.state = State::Data, // GA/NOP/DM/etc. — ignore
                },
                State::Will => {
                    self.on_will(b, &mut reply);
                    self.state = State::Data;
                }
                State::Wont => {
                    if self.remote.remove(&b) {
                        reply.extend_from_slice(&[IAC, DONT, b]);
                    }
                    self.state = State::Data;
                }
                State::Do => {
                    self.on_do(b, &mut reply, cols, rows);
                    self.state = State::Data;
                }
                State::Dont => {
                    if self.local.remove(&b) {
                        reply.extend_from_slice(&[IAC, WONT, b]);
                    }
                    if b == OPT_NAWS {
                        self.naws_active = false;
                    }
                    self.state = State::Data;
                }
                State::Sb => {
                    self.sub_opt = b;
                    self.sub.clear();
                    self.state = State::SbData;
                }
                State::SbData => {
                    if b == IAC {
                        self.state = State::SbIac;
                    } else if self.sub.len() < MAX_SUBNEG {
                        self.sub.push(b);
                    }
                    // Overflow bytes are dropped; a well-formed subnegotiation is tiny.
                }
                State::SbIac => match b {
                    SE => {
                        self.on_subneg(&mut reply);
                        self.state = State::Data;
                    }
                    IAC => {
                        if self.sub.len() < MAX_SUBNEG {
                            self.sub.push(IAC);
                        }
                        self.state = State::SbData;
                    }
                    _ => self.state = State::SbData,
                },
            }
        }

        (data, reply)
    }

    fn on_will(&mut self, opt: u8, reply: &mut Vec<u8>) {
        // Peer offers to enable `opt` on its side.
        let want = matches!(opt, OPT_ECHO | OPT_SGA);
        if want {
            if self.remote.insert(opt) {
                reply.extend_from_slice(&[IAC, DO, opt]);
            }
        } else {
            reply.extend_from_slice(&[IAC, DONT, opt]);
        }
    }

    fn on_do(&mut self, opt: u8, reply: &mut Vec<u8>, cols: u16, rows: u16) {
        // Peer asks us to enable `opt` on our side.
        let support = matches!(opt, OPT_SGA | OPT_TTYPE | OPT_NAWS);
        if support {
            if self.local.insert(opt) {
                reply.extend_from_slice(&[IAC, WILL, opt]);
            }
            if opt == OPT_NAWS {
                self.naws_active = true;
                reply.extend_from_slice(&naws_subneg(cols, rows));
            }
        } else {
            reply.extend_from_slice(&[IAC, WONT, opt]);
        }
    }

    fn on_subneg(&mut self, reply: &mut Vec<u8>) {
        // TERMINAL-TYPE SEND -> IS xterm-256color
        if self.sub_opt == OPT_TTYPE && self.sub.first() == Some(&1) {
            reply.extend_from_slice(&[IAC, SB, OPT_TTYPE, 0]); // 0 = IS
            for &b in TERM {
                if b == IAC {
                    reply.push(IAC);
                }
                reply.push(b);
            }
            reply.extend_from_slice(&[IAC, SE]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_iac_cannot_grow_subnegotiation_past_the_cap() {
        let mut parser = Telnet::new();
        let mut input = vec![IAC, SB, OPT_TTYPE];
        input.extend(std::iter::repeat_n(b'x', MAX_SUBNEG));
        for _ in 0..MAX_SUBNEG {
            input.extend_from_slice(&[IAC, IAC]);
        }
        let _ = parser.feed(&input, 80, 24);
        assert_eq!(parser.sub.len(), MAX_SUBNEG);
    }

    #[test]
    fn terminal_type_subnegotiation_is_answered() {
        let mut parser = Telnet::new();
        let (_, reply) = parser.feed(&[IAC, SB, OPT_TTYPE, 1, IAC, SE], 80, 24);
        let mut expected = vec![IAC, SB, OPT_TTYPE, 0];
        expected.extend_from_slice(TERM);
        expected.extend_from_slice(&[IAC, SE]);
        assert_eq!(reply, expected);
    }

    #[test]
    fn outgoing_data_escapes_iac_and_normalizes_crlf() {
        assert_eq!(
            encode_output(&[b'a', IAC, b'\r', b'\n']),
            vec![b'a', IAC, IAC, b'\r', b'\n']
        );
    }
}
