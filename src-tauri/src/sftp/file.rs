//! Remote files as `AsyncRead` / `AsyncWrite`, with requests pipelined: up to
//! `depth` reads or writes are in flight at once, so throughput isn't capped
//! at one chunk per network round trip.

use super::client::{Request, SftpClient};
use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Chunk size and pipeline depth for one open file.
#[derive(Debug, Clone, Copy)]
pub struct Pipeline {
    pub chunk: u32,
    pub depth: usize,
}

struct InFlight {
    offset: u64,
    length: u32,
    reply: Request<Option<Vec<u8>>>,
}

pub struct SftpReader {
    client: Arc<SftpClient>,
    handle: Vec<u8>,
    pipeline: Pipeline,
    /// Where the next new request starts.
    next_offset: u64,
    /// Where the caller expects the file to end (its size, or the end of the
    /// range it wants): no reads are sent past it but one, which finds the
    /// end of the file in the same round trip. `None` once the file turns
    /// out longer.
    expected_end: Option<u64>,
    probed: bool,
    in_flight: VecDeque<InFlight>,
    end_of_file: bool,
    chunk: Vec<u8>,
    position: usize,
}

impl SftpReader {
    pub fn new(
        client: Arc<SftpClient>,
        handle: Vec<u8>,
        offset: u64,
        expected_end: Option<u64>,
        pipeline: Pipeline,
    ) -> Self {
        Self {
            client,
            handle,
            pipeline,
            next_offset: offset,
            expected_end,
            probed: false,
            in_flight: VecDeque::new(),
            end_of_file: false,
            chunk: Vec::new(),
            position: 0,
        }
    }

    fn request(&self, offset: u64, length: u32) -> InFlight {
        InFlight {
            offset,
            length,
            reply: self.client.read(&self.handle, offset, length),
        }
    }

    fn fill_pipeline(&mut self) {
        while !self.end_of_file && self.in_flight.len() < self.pipeline.depth {
            let mut length = self.pipeline.chunk;
            if let Some(end) = self.expected_end {
                if self.next_offset >= end {
                    if self.probed {
                        break;
                    }
                    self.probed = true;
                } else {
                    length = length.min(u32::try_from(end - self.next_offset).unwrap_or(u32::MAX));
                }
            }
            let request = self.request(self.next_offset, length);
            self.next_offset += u64::from(length);
            self.in_flight.push_back(request);
        }
    }
}

impl AsyncRead for SftpReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.position < this.chunk.len() {
                let count = buf.remaining().min(this.chunk.len() - this.position);
                buf.put_slice(&this.chunk[this.position..this.position + count]);
                this.position += count;
                return Poll::Ready(Ok(()));
            }
            this.fill_pipeline();
            let Some(front) = this.in_flight.front_mut() else {
                return Poll::Ready(Ok(())); // end of file
            };
            let result = ready!(front.reply.as_mut().poll(cx));
            let InFlight { offset, length, .. } = this.in_flight.pop_front().expect("front");
            match result? {
                Some(mut data) if !data.is_empty() => {
                    // A server sending more than was asked for would shift
                    // everything after it.
                    data.truncate(length as usize);
                    let received = data.len() as u32;
                    if reaches_past(this.expected_end, offset, received) {
                        // The file is longer than expected: read on normally.
                        this.expected_end = None;
                    }
                    if received < length {
                        // A short read before the end: fetch the rest of this
                        // range before anything that was requested after it.
                        let rest = this.request(offset + u64::from(received), length - received);
                        this.in_flight.push_front(rest);
                    }
                    this.chunk = data;
                    this.position = 0;
                }
                // End of file: whatever else is in flight lies beyond it.
                _ => {
                    this.end_of_file = true;
                    this.in_flight.clear();
                }
            }
        }
    }
}

/// Whether data received at `offset` goes past where the file was expected to end.
fn reaches_past(expected_end: Option<u64>, offset: u64, received: u32) -> bool {
    expected_end.is_some_and(|end| offset + u64::from(received) > end)
}

impl Drop for SftpReader {
    fn drop(&mut self) {
        self.in_flight.clear();
        self.client.close_quietly(&self.handle);
    }
}

pub struct SftpWriter {
    client: Arc<SftpClient>,
    handle: Vec<u8>,
    pipeline: Pipeline,
    /// Where the next write goes.
    offset: u64,
    buffer: Vec<u8>,
    in_flight: VecDeque<Request<()>>,
    closing: Option<Request<()>>,
    closed: bool,
}

impl SftpWriter {
    pub fn new(client: Arc<SftpClient>, handle: Vec<u8>, offset: u64, pipeline: Pipeline) -> Self {
        Self {
            client,
            handle,
            offset,
            buffer: Vec::with_capacity(pipeline.chunk as usize),
            pipeline,
            in_flight: VecDeque::new(),
            closing: None,
            closed: false,
        }
    }

    fn dispatch(&mut self) {
        let data = std::mem::take(&mut self.buffer);
        self.in_flight
            .push_back(self.client.write(&self.handle, self.offset, &data));
        self.offset += data.len() as u64;
        self.buffer = data;
        self.buffer.clear();
    }

    /// Wait for the oldest write; its error (a full disk, say) surfaces here.
    fn poll_oldest(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(front) = self.in_flight.front_mut() else {
            return Poll::Ready(Ok(()));
        };
        let result = ready!(front.as_mut().poll(cx));
        self.in_flight.pop_front();
        Poll::Ready(result)
    }

    /// Retire the writes that already finished, without waiting.
    fn reap(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        while !self.in_flight.is_empty() {
            match self.poll_oldest(cx) {
                Poll::Ready(result) => result?,
                Poll::Pending => break,
            }
        }
        Ok(())
    }

    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.buffer.is_empty() {
            while self.in_flight.len() >= self.pipeline.depth {
                ready!(self.poll_oldest(cx))?;
            }
            self.dispatch();
        }
        while !self.in_flight.is_empty() {
            ready!(self.poll_oldest(cx))?;
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SftpWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if this.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the remote file is closed",
            )));
        }
        this.reap(cx)?;
        let chunk = this.pipeline.chunk as usize;
        loop {
            if this.buffer.len() < chunk {
                let count = data.len().min(chunk - this.buffer.len());
                this.buffer.extend_from_slice(&data[..count]);
                if this.buffer.len() == chunk && this.in_flight.len() < this.pipeline.depth {
                    this.dispatch();
                }
                return Poll::Ready(Ok(count));
            }
            // The buffer is a full chunk waiting for a free slot.
            if this.in_flight.len() < this.pipeline.depth {
                this.dispatch();
            } else {
                ready!(this.poll_oldest(cx))?;
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.closed {
            return Poll::Ready(Ok(()));
        }
        self.poll_drain(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.closed {
            return Poll::Ready(Ok(()));
        }
        ready!(this.poll_drain(cx))?;
        let closing = this
            .closing
            .get_or_insert_with(|| this.client.close_request(&this.handle));
        let result = ready!(closing.as_mut().poll(cx));
        this.closing = None;
        this.closed = true;
        Poll::Ready(result)
    }
}

impl Drop for SftpWriter {
    fn drop(&mut self) {
        if !self.closed && self.closing.is_none() {
            self.in_flight.clear();
            self.client.close_quietly(&self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::{
        protocol::{self as fx, Attrs, Decoder},
        testing::{fake_server, reply, FakeServer},
    };
    use tokio::io::AsyncReadExt;

    /// A server with one 100-byte file whose full-length DATA replies carry
    /// five bytes too many.
    fn overlong_server() -> FakeServer {
        let file: Vec<u8> = (0..100).collect();
        fake_server(move |request| match request.kind {
            fx::FXP_OPEN => vec![reply::handle(request.id, b"h")],
            fx::FXP_READ => {
                let mut fields = Decoder::new(&request.fields);
                fields.bytes().unwrap();
                let offset = fields.u64().unwrap() as usize;
                let length = fields.u32().unwrap() as usize;
                if offset >= file.len() {
                    return vec![reply::status(request.id, fx::FX_EOF)];
                }
                let mut data = file[offset..(offset + length).min(file.len())].to_vec();
                if data.len() == length {
                    data.extend_from_slice(b"junk!");
                }
                vec![reply::data(request.id, &data)]
            }
            _ => vec![reply::status(request.id, fx::FX_OK)],
        })
    }

    async fn read(
        client: &Arc<SftpClient>,
        expected_end: Option<u64>,
        pipeline: Pipeline,
    ) -> Vec<u8> {
        let handle = client
            .open(b"/file", fx::FXF_READ, &Attrs::default())
            .await
            .unwrap();
        let mut reader = SftpReader::new(client.clone(), handle, 0, expected_end, pipeline);
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        data
    }

    #[tokio::test]
    async fn reads_take_no_more_than_was_asked_and_stop_at_the_expected_end() {
        let fake = overlong_server();
        let seen = fake.seen.clone();
        let client = Arc::new(SftpClient::start(fake.stream).await.unwrap());
        let file: Vec<u8> = (0..100).collect();
        let pipeline = Pipeline {
            chunk: 32,
            depth: 16,
        };
        let reads = || {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|request| request.kind == fx::FXP_READ)
                .count()
        };

        // Expecting 100 bytes: reads at 0, 32, 64 and 96, and one at 100
        // that finds the end, all at once; not a pipeline's worth.
        assert_eq!(read(&client, Some(100), pipeline).await, file);
        assert_eq!(reads(), 5);

        // A file longer than expected is still read to its end.
        assert_eq!(read(&client, Some(50), pipeline).await, file);
        // No hint: the same bytes.
        assert_eq!(read(&client, None, pipeline).await, file);
    }
}
