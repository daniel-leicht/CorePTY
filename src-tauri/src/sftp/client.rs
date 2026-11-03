//! An SFTP v3 client over any byte stream (an SSH `sftp` subsystem channel,
//! or a local `sftp-server` process in tests).
//!
//! Requests are sent the moment they're made and replies are matched by id,
//! so callers can keep many requests in flight (pipelined reads and writes).

use super::protocol::{self as fx, Attrs, Encoder, NameEntry, Response};
use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, MutexGuard,
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
};

/// A reply, or why the one for this request made no sense.
type Waiters = HashMap<u32, oneshot::Sender<io::Result<Response>>>;

struct Shared {
    waiters: Mutex<Waiters>,
    /// Why the session ended, once it has.
    closed: Mutex<Option<String>>,
    /// The same, for those waiting to hear about it.
    ended: watch::Sender<Option<String>>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            waiters: Mutex::default(),
            closed: Mutex::default(),
            ended: watch::channel(None).0,
        }
    }
}

impl Shared {
    fn waiters(&self) -> MutexGuard<'_, Waiters> {
        self.waiters.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn closed_reason(&self) -> Option<String> {
        self.closed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// End the session: every waiting request fails with `reason`.
    fn close(&self, reason: String) {
        {
            let mut closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
            if closed.is_none() {
                *closed = Some(reason.clone());
                self.ended.send_replace(Some(reason));
            }
        }
        // Dropping the senders wakes every waiter with an error.
        self.waiters().clear();
    }

    fn lost(&self) -> io::Error {
        lost_error(self.closed_reason())
    }
}

fn lost_error(reason: Option<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        reason.unwrap_or_else(|| "the SFTP session has ended".to_string()),
    )
}

/// Server extensions this client knows how to use.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extensions {
    /// `posix-rename@openssh.com`: rename that replaces the target in one step.
    pub posix_rename: bool,
    /// `limits@openssh.com`: the server's packet and read/write sizes.
    pub limits: bool,
    /// `expand-path@openssh.com`: `~` and `~user` expansion.
    pub expand_path: bool,
}

/// What `limits@openssh.com` reports (zero: no limit given).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub max_packet: u64,
    pub max_read: u64,
    pub max_write: u64,
}

pub struct SftpClient {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    next_id: AtomicU32,
    pub extensions: Extensions,
    /// Kept as long as the client (the SSH connection's lease): files still
    /// open keep the connection up after their filesystem has gone.
    kept: Mutex<Option<Box<dyn std::any::Any + Send + Sync>>>,
}

impl Drop for SftpClient {
    fn drop(&mut self) {
        self.shared.close("the SFTP session was closed".to_string());
    }
}

/// A request on its way: resolves with the server's reply. Dropping it
/// forgets the request (a late reply is ignored).
pub struct Pending {
    id: u32,
    shared: Arc<Shared>,
    reply: Option<oneshot::Receiver<io::Result<Response>>>,
}

impl Future for Pending {
    type Output = io::Result<Response>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(reply) = self.reply.as_mut() else {
            return Poll::Ready(Err(self.shared.lost()));
        };
        match Pin::new(reply).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.reply = None;
                Poll::Ready(match result {
                    Ok(reply) => reply,
                    Err(_) => Err(self.shared.lost()),
                })
            }
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.reply.is_some() {
            self.shared.waiters().remove(&self.id);
        }
    }
}

/// A boxed request future, for keeping several in flight.
pub type Request<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send>>;

impl SftpClient {
    /// Say hello over `stream` and start the session.
    pub async fn start<S>(stream: S) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let mut init = Encoder::new(fx::FXP_INIT);
        init.u32(fx::VERSION);
        writer.write_all(&init.finish()).await?;
        writer.flush().await?;
        let body = read_packet(&mut reader).await?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the server closed the SFTP session before it started (is SFTP enabled?)",
            )
        })?;
        let (version, offered) = fx::parse_version(&body)?;
        if version < fx::VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("the server speaks SFTP version {version}; version 3 is needed"),
            ));
        }
        let mut extensions = Extensions::default();
        for (name, data) in &offered {
            match name.as_str() {
                "posix-rename@openssh.com" if data == b"1" => extensions.posix_rename = true,
                "limits@openssh.com" if data == b"1" => extensions.limits = true,
                "expand-path@openssh.com" if data == b"1" => extensions.expand_path = true,
                _ => {}
            }
        }

        let shared = Arc::new(Shared::default());
        let (outgoing, mut queue) = mpsc::unbounded_channel::<Vec<u8>>();
        let writing = shared.clone();
        let mut ended = shared.ended.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    packet = queue.recv() => {
                        let Some(packet) = packet else { break };
                        if let Err(e) = writer.write_all(&packet).await {
                            writing.close(format!("the connection was lost ({e})"));
                            return;
                        }
                    }
                    _ = async { let _ = ended.wait_for(|reason| reason.is_some()).await; } => break,
                }
            }
            // The client is gone, or the session ended: end the channel
            // politely, so the server closes its side too.
            let _ = writer.shutdown().await;
        });
        let reading = shared.clone();
        tokio::spawn(async move {
            let reason = loop {
                match read_packet(&mut reader).await {
                    Ok(Some(body)) => {
                        let (id, reply) = match fx::parse_response(&body) {
                            Ok((id, response)) => (id, Ok(response)),
                            // Packets are framed by their length, so one that
                            // makes no sense fails only its own request.
                            Err(e) => match body.get(1..5) {
                                Some(id) => {
                                    (u32::from_be_bytes([id[0], id[1], id[2], id[3]]), Err(e))
                                }
                                None => break e.to_string(),
                            },
                        };
                        if let Some(waiter) = reading.waiters().remove(&id) {
                            let _ = waiter.send(reply);
                        }
                    }
                    Ok(None) => break "the server closed the SFTP session".to_string(),
                    Err(e) => break format!("the connection was lost ({e})"),
                }
            };
            reading.close(reason);
            // Keep reading until the channel closes: SSH stops the whole
            // connection (terminals on it included) while one channel's data
            // goes unread.
            let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
        });
        Ok(Self {
            shared,
            outgoing,
            next_id: AtomicU32::new(1),
            extensions,
            kept: Mutex::default(),
        })
    }

    /// Keep `value` (a connection lease) alive as long as the session.
    pub fn hold(&self, value: Box<dyn std::any::Any + Send + Sync>) {
        *self.kept.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
    }

    /// Whether the session has ended (the connection dropped, say).
    pub fn has_ended(&self) -> bool {
        self.shared.closed_reason().is_some()
    }

    /// Resolves with the reason once the session has ended.
    pub fn ended(&self) -> impl Future<Output = String> + Send + 'static {
        let mut ended = self.shared.ended.subscribe();
        async move {
            loop {
                if let Some(reason) = ended.borrow_and_update().clone() {
                    return reason;
                }
                if ended.changed().await.is_err() {
                    return "the SFTP session was closed".to_string();
                }
            }
        }
    }

    /// Send a request now; `build` writes the fields after the request id.
    fn send(&self, kind: u8, capacity: usize, build: impl FnOnce(&mut Encoder)) -> Pending {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        let pending = Pending {
            id,
            shared: self.shared.clone(),
            reply: Some(receiver),
        };
        {
            // Check and register under one lock: `close` takes it to clear
            // the waiters, so no request can slip in after that and hang.
            let mut waiters = self.shared.waiters();
            if self.shared.closed_reason().is_some() {
                return pending.failed();
            }
            waiters.insert(id, sender);
        }
        let mut packet = Encoder::with_capacity(kind, capacity);
        packet.u32(id);
        build(&mut packet);
        if self.outgoing.send(packet.finish()).is_err() {
            self.shared.waiters().remove(&id);
        }
        pending
    }

    fn path_request(&self, kind: u8, path: &[u8]) -> Pending {
        self.send(kind, path.len(), |packet| {
            packet.bytes(path);
        })
    }

    pub async fn realpath(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        single_name(self.path_request(fx::FXP_REALPATH, path).await?)
    }

    /// `~`, `~/x` and `~user/x` as the server's shell would expand them.
    pub async fn expand_path(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        if !self.extensions.expand_path {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the server can't expand ~ paths",
            ));
        }
        let reply = self
            .send(fx::FXP_EXTENDED, path.len() + 32, |packet| {
                packet.bytes(b"expand-path@openssh.com").bytes(path);
            })
            .await?;
        single_name(reply)
    }

    pub async fn stat(&self, path: &[u8]) -> io::Result<Attrs> {
        attrs(self.path_request(fx::FXP_STAT, path).await?)
    }

    pub async fn lstat(&self, path: &[u8]) -> io::Result<Attrs> {
        attrs(self.path_request(fx::FXP_LSTAT, path).await?)
    }

    pub async fn fstat(&self, handle: &[u8]) -> io::Result<Attrs> {
        attrs(self.path_request(fx::FXP_FSTAT, handle).await?)
    }

    pub async fn setstat(&self, path: &[u8], attributes: &Attrs) -> io::Result<()> {
        let reply = self
            .send(fx::FXP_SETSTAT, path.len() + 32, |packet| {
                packet.bytes(path).attrs(attributes);
            })
            .await?;
        status(reply)
    }

    pub async fn opendir(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        handle(self.path_request(fx::FXP_OPENDIR, path).await?)
    }

    /// The next batch of entries, or `None` at the end.
    pub async fn readdir(&self, handle: &[u8]) -> io::Result<Option<Vec<NameEntry>>> {
        match self.path_request(fx::FXP_READDIR, handle).await? {
            Response::Name(entries) => Ok(Some(entries)),
            Response::Status {
                code: fx::FX_EOF, ..
            } => Ok(None),
            other => Err(unexpected(other)),
        }
    }

    pub async fn close(&self, handle: &[u8]) -> io::Result<()> {
        status(self.path_request(fx::FXP_CLOSE, handle).await?)
    }

    /// Close a handle without waiting for the reply (from `Drop`).
    pub fn close_quietly(&self, handle: &[u8]) {
        drop(self.path_request(fx::FXP_CLOSE, handle));
    }

    /// Like [`Self::stat`], but sent at once, to stat many paths in one round trip.
    pub fn stat_request(&self, path: &[u8]) -> Request<Attrs> {
        let pending = self.path_request(fx::FXP_STAT, path);
        Box::pin(async move { attrs(pending.await?) })
    }

    /// Like [`Self::remove`], but sent at once.
    pub fn remove_request(&self, path: &[u8]) -> Request<()> {
        let pending = self.path_request(fx::FXP_REMOVE, path);
        Box::pin(async move { status(pending.await?) })
    }

    /// Like [`Self::close`], but sent at once.
    pub fn close_request(&self, handle: &[u8]) -> Request<()> {
        let pending = self.path_request(fx::FXP_CLOSE, handle);
        Box::pin(async move { status(pending.await?) })
    }

    /// Every entry of a folder (without `.`, `..` and names no entry can have).
    pub async fn read_dir(&self, path: &[u8]) -> io::Result<Vec<NameEntry>> {
        let handle = self.opendir(path).await?;
        let mut entries = Vec::new();
        let listed = loop {
            match self.readdir(&handle).await {
                Ok(Some(batch)) => entries.extend(
                    batch
                        .into_iter()
                        .filter(|entry| is_child_name(&entry.filename)),
                ),
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        let closed = self.close(&handle).await;
        listed?;
        closed?;
        Ok(entries)
    }

    pub async fn open(&self, path: &[u8], flags: u32, attributes: &Attrs) -> io::Result<Vec<u8>> {
        let reply = self
            .send(fx::FXP_OPEN, path.len() + 32, |packet| {
                packet.bytes(path).u32(flags).attrs(attributes);
            })
            .await?;
        handle(reply)
    }

    /// Ask for up to `length` bytes at `offset`; `None` means end of file. The
    /// request goes out immediately; await the result later to pipeline.
    pub fn read(&self, handle: &[u8], offset: u64, length: u32) -> Request<Option<Vec<u8>>> {
        let pending = self.send(fx::FXP_READ, handle.len() + 16, |packet| {
            packet.bytes(handle).u64(offset).u32(length);
        });
        Box::pin(async move {
            match pending.await? {
                Response::Data(data) => Ok(Some(data)),
                Response::Status {
                    code: fx::FX_EOF, ..
                } => Ok(None),
                other => Err(unexpected(other)),
            }
        })
    }

    /// Write `data` at `offset`. Sent immediately, like [`Self::read`].
    pub fn write(&self, handle: &[u8], offset: u64, data: &[u8]) -> Request<()> {
        let pending = self.send(fx::FXP_WRITE, handle.len() + data.len() + 16, |packet| {
            packet.bytes(handle).u64(offset).bytes(data);
        });
        Box::pin(async move { status(pending.await?) })
    }

    pub async fn remove(&self, path: &[u8]) -> io::Result<()> {
        status(self.path_request(fx::FXP_REMOVE, path).await?)
    }

    pub async fn mkdir(&self, path: &[u8]) -> io::Result<()> {
        let reply = self
            .send(fx::FXP_MKDIR, path.len() + 8, |packet| {
                packet.bytes(path).attrs(&Attrs::default());
            })
            .await?;
        status(reply)
    }

    pub async fn rmdir(&self, path: &[u8]) -> io::Result<()> {
        status(self.path_request(fx::FXP_RMDIR, path).await?)
    }

    /// Plain v3 rename: fails if `to` exists (OpenSSH, and most servers).
    pub async fn rename(&self, from: &[u8], to: &[u8]) -> io::Result<()> {
        let reply = self
            .send(fx::FXP_RENAME, from.len() + to.len() + 8, |packet| {
                packet.bytes(from).bytes(to);
            })
            .await?;
        status(reply)
    }

    /// `rename(2)` semantics: replaces a file at `to` in one step.
    pub async fn posix_rename(&self, from: &[u8], to: &[u8]) -> io::Result<()> {
        let reply = self
            .send(fx::FXP_EXTENDED, from.len() + to.len() + 40, |packet| {
                packet
                    .bytes(b"posix-rename@openssh.com")
                    .bytes(from)
                    .bytes(to);
            })
            .await?;
        status(reply)
    }

    pub async fn readlink(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        single_name(self.path_request(fx::FXP_READLINK, path).await?)
    }

    /// Create `link` pointing at `target`. OpenSSH sends the two paths in the
    /// opposite order to the draft (target first), and nearly every server
    /// follows OpenSSH, so this does too.
    pub async fn symlink(&self, target: &[u8], link: &[u8]) -> io::Result<()> {
        let reply = self
            .send(fx::FXP_SYMLINK, target.len() + link.len() + 8, |packet| {
                packet.bytes(target).bytes(link);
            })
            .await?;
        status(reply)
    }

    /// The largest packets, reads and writes the server accepts.
    pub async fn limits(&self) -> io::Result<Limits> {
        let reply = self
            .send(fx::FXP_EXTENDED, 24, |packet| {
                packet.bytes(b"limits@openssh.com");
            })
            .await?;
        let Response::ExtendedReply(data) = reply else {
            return Err(unexpected(reply));
        };
        let mut decoder = fx::Decoder::new(&data);
        Ok(Limits {
            max_packet: decoder.u64()?,
            max_read: decoder.u64()?,
            max_write: decoder.u64()?,
        })
    }
}

impl Pending {
    fn failed(mut self) -> Self {
        self.reply = None;
        self
    }
}

/// Read one packet body; `None` on a clean end of stream.
async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > fx::MAX_PACKET {
        return Err(fx::bad_message(&format!("packet length {length}")));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

fn unexpected(response: Response) -> io::Error {
    match response {
        Response::Status { code, message } => fx::status_error(code, &message),
        _ => fx::bad_message("unexpected reply type"),
    }
}

fn status(response: Response) -> io::Result<()> {
    match response {
        Response::Status {
            code: fx::FX_OK, ..
        } => Ok(()),
        other => Err(unexpected(other)),
    }
}

fn handle(response: Response) -> io::Result<Vec<u8>> {
    match response {
        Response::Handle(handle) => Ok(handle),
        other => Err(unexpected(other)),
    }
}

fn attrs(response: Response) -> io::Result<Attrs> {
    match response {
        Response::Attrs(attrs) => Ok(attrs),
        other => Err(unexpected(other)),
    }
}

fn single_name(response: Response) -> io::Result<Vec<u8>> {
    match response {
        Response::Name(mut entries) if !entries.is_empty() => Ok(entries.swap_remove(0).filename),
        Response::Name(_) => Err(fx::bad_message("empty name reply")),
        other => Err(unexpected(other)),
    }
}

/// A name a folder entry can have: not `.` or `..`, not empty, no `/` (a
/// hostile server's `""` would make a recursive delete walk the folder again).
fn is_child_name(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::testing::{fake_server, reply};
    use std::time::Duration;

    #[tokio::test]
    async fn a_malformed_reply_fails_only_its_own_request() {
        let fake = fake_server(|request| match request.id {
            // A packet type no server sends.
            1 => vec![reply::raw(250, request.id, b"")],
            // Attributes cut short.
            2 => vec![reply::raw(fx::FXP_ATTRS, request.id, &[0, 0])],
            _ => vec![reply::attrs(request.id)],
        });
        let client = SftpClient::start(fake.stream).await.unwrap();
        for _ in 0..2 {
            let error = client.stat(b"/a").await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        }
        assert!(client.stat(b"/b").await.is_ok());
        assert!(!client.has_ended());
    }

    #[tokio::test]
    async fn after_a_broken_packet_the_channel_is_still_read_until_it_closes() {
        // A length of zero breaks the framing; then a flood the server can
        // only get out while the client keeps reading (SSH would otherwise
        // stall the whole connection, terminals on it included).
        let fake = fake_server(|_| vec![vec![0, 0, 0, 0], vec![7; 1024 * 1024]]);
        let client = SftpClient::start(fake.stream).await.unwrap();
        let error = client.stat(b"/a").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted, "{error}");
        assert!(client.has_ended());
        tokio::time::timeout(Duration::from_secs(5), fake.done)
            .await
            .expect("the server got its data out and saw the session end")
            .unwrap();
    }
}
