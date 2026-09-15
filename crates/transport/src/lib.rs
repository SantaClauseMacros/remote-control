//! Transport abstraction.
//!
//! A [`Session`] gives the host and clients an authenticated, encrypted,
//! bidirectional link plus a video channel — without the rest of the code
//! needing to know whether the bytes travel peer-to-peer or through a relay.
//!
//! [`lan`] is the milestone-3 implementation: a direct TCP connection secured
//! with a Noise handshake, for same-network PC↔PC. A WebRTC implementation
//! (ICE + DTLS-SRTP + data channels + congestion control) for NAT traversal
//! and the internet is added in milestone 4 behind this same [`Session`] trait.

use async_trait::async_trait;

pub mod lan;
pub mod relay;
pub mod ws_io;

/// Errors surfaced by any transport implementation.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection closed")]
    Closed,
    #[error("operation timed out")]
    Timeout,
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("transport error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Ordered, reliable channel for protocol messages (input, clipboard, control).
///
/// Implementations MUST preserve message boundaries: one `send` == one `recv`.
#[async_trait]
pub trait ControlChannel: Send + Sync {
    async fn send(&self, bytes: Vec<u8>) -> Result<()>;
    async fn recv(&self) -> Result<Vec<u8>>;
}

/// One encoded video frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key_frame: bool,
    /// Capture timestamp in microseconds since an arbitrary, monotonic epoch.
    pub timestamp_us: u64,
}

/// The video path for a session. The host calls [`VideoChannel::send`]; the
/// client calls [`VideoChannel::recv`]. Both sides read [`VideoChannel::feedback`]
/// and the key-frame request flag.
#[async_trait]
pub trait VideoChannel: Send + Sync {
    /// Host side: hand an encoded frame to the transport.
    async fn send(&self, frame: EncodedFrame) -> Result<()>;
    /// Client side: receive the next encoded frame.
    async fn recv(&self) -> Result<EncodedFrame>;

    /// Latest link estimate. The host's adaptive controller reads this each
    /// frame to choose bitrate / FPS / resolution.
    fn feedback(&self) -> LinkFeedback;

    /// Client → host: ask for a fresh keyframe (decoder lost sync).
    fn request_key_frame(&self);
    /// Host side: has the client asked for a keyframe since the last check?
    fn take_key_frame_request(&self) -> bool;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkFeedback {
    /// Smoothed round-trip time from the transport keepalive, milliseconds.
    pub rtt_ms: f32,
    /// Congestion-control bitrate estimate, or `0` when the transport has no
    /// estimate (the LAN transport; the host then uses its configured cap).
    pub target_bitrate_bps: u32,
    pub packet_loss: f32,
}

/// A negotiated connection to exactly one peer.
#[async_trait]
pub trait Session: Send + Sync {
    fn control(&self) -> &dyn ControlChannel;
    fn video(&self) -> &dyn VideoChannel;
    /// Resolves when the underlying connection is permanently gone.
    async fn closed(&self);
}
