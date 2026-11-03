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
    in_flight: VecDeque<InFlight>,
    end_of_file: bool,
    chunk: Vec<u8>,
    position: usize,
}

impl SftpReader {
    pub fn new(client: Arc<SftpClient>, handle: Vec<u8>, offset: u64, pipeline: Pipeline) -> Self {
        Self {
            client,
            handle,
            pipeline,
            next_offset: offset,
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
            let request = self.request(self.next_offset, self.pipeline.chunk);
            self.next_offset += u64::from(self.pipeline.chunk);
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
                Some(data) if !data.is_empty() => {
                    let received = data.len() as u32;
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
