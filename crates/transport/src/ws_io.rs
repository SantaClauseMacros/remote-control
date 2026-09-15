//! Adapts a `tokio-tungstenite` WebSocket into a plain `AsyncRead + AsyncWrite`
//! byte stream, so [`crate::lan::LanSession::over_stream_initiator`] can run
//! unmodified over a `wss://` relay (a Cloudflare Worker, say) exactly the way
//! it already runs over a raw TCP socket to a self-hosted `rc-relay`.
//!
//! Each WebSocket **binary** message becomes a chunk of the byte stream (in
//! order); anything else (ping/pong/text/close) is consumed here and never
//! surfaces to the caller.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

pub struct WsIo<S> {
    ws: WebSocketStream<S>,
    /// Bytes from the most recent binary message not yet handed to a reader.
    pending: Vec<u8>,
    pending_pos: usize,
}

impl<S> WsIo<S> {
    pub fn new(ws: WebSocketStream<S>) -> Self {
        Self {
            ws,
            pending: Vec::new(),
            pending_pos: 0,
        }
    }
}

// `WebSocketStream<S>` is `Unpin` whenever `S: Unpin` (it holds no
// self-references), and the rest of our fields are plain owned data — so the
// whole adapter is `Unpin` and every poll_* below can borrow `&mut self`
// fields directly instead of doing manual pin-projection.
impl<S: Unpin> Unpin for WsIo<S> {}

impl<S> AsyncRead for WsIo<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.pending_pos < self.pending.len() {
                let n = (buf.remaining()).min(self.pending.len() - self.pending_pos);
                let start = self.pending_pos;
                buf.put_slice(&self.pending[start..start + n]);
                self.pending_pos += n;
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.ws).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())), // EOF
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(io::Error::other(e)))
                }
                Poll::Ready(Some(Ok(Message::Binary(data)))) => {
                    self.pending = data;
                    self.pending_pos = 0;
                    // loop again to actually copy it into `buf`
                }
                Poll::Ready(Some(Ok(Message::Close(_)))) => return Poll::Ready(Ok(())), // EOF
                // Ping/Pong/Text/Frame: tungstenite answers pings itself; we
                // just skip anything that isn't application data.
                Poll::Ready(Some(Ok(_))) => continue,
            }
        }
    }
}

impl<S> AsyncWrite for WsIo<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.ws).poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
            Poll::Ready(Ok(())) => {}
        }
        match Pin::new(&mut self.ws).start_send(Message::Binary(buf.to_vec())) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.ws).poll_flush(cx).map_err(io::Error::other)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.ws).poll_close(cx).map_err(io::Error::other)
    }
}
