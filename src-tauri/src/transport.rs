//! Framing and the pairing handshake over plain TCP.
//!
//! Frames are `u32` little-endian length + ChaCha20-Poly1305 ciphertext. The
//! sockets run with `TCP_NODELAY`, because a mouse-move packet delayed by
//! Nagle's algorithm is exactly the stutter this app exists to avoid.

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::crypto::{CryptoError, Handshake, Sealer, Secret, SessionKeys};
use crate::protocol::{DeviceId, MAGIC, MAX_FRAME, PROTOCOL_VERSION};

const INTRO_LEN: usize = 4 + 1 + 1 + 16 + 32;
const REPLY_LEN: usize = 4 + 1 + 32;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Codec(#[from] postcard::Error),
    #[error("peer is not CrossDesk or speaks an incompatible version")]
    Handshake,
    #[error("this device has no key here and the host is not accepting new pairings")]
    Rejected,
    #[error("peer sent an oversized frame ({0} bytes)")]
    FrameTooLarge(usize),
}

/// How the client wants to authenticate on this connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthMode {
    /// First contact: the user typed the host's pairing code.
    Code,
    /// Returning device: use the key stored during a previous pairing.
    Stored,
}

impl AuthMode {
    fn as_byte(self) -> u8 {
        match self {
            AuthMode::Code => 0,
            AuthMode::Stored => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(AuthMode::Code),
            1 => Some(AuthMode::Stored),
            _ => None,
        }
    }
}

/// What the host learned from a connecting client before keys were derived, so
/// it can look up the right authentication secret.
pub struct ClientIntent {
    pub device: DeviceId,
    pub mode: AuthMode,
    client_public: [u8; 32],
}

pub struct FrameWriter {
    inner: OwnedWriteHalf,
    sealer: Sealer,
    scratch: Vec<u8>,
}

pub struct FrameReader {
    inner: OwnedReadHalf,
    sealer: Sealer,
    scratch: Vec<u8>,
}

impl FrameWriter {
    /// Serialises, encrypts and writes one message as a single `write_all`, so
    /// a small event stays in one TCP segment.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), TransportError> {
        let mut body = std::mem::take(&mut self.scratch);
        body.clear();
        body = postcard::to_extend(msg, body)?;
        self.sealer.seal(&mut body)?;

        let len =
            u32::try_from(body.len()).map_err(|_| TransportError::FrameTooLarge(body.len()))?;
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&body);

        self.scratch = body;
        self.inner.write_all(&frame).await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.inner.shutdown().await;
    }
}

impl FrameReader {
    /// Reads, decrypts and decodes one message.
    pub async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, TransportError> {
        let mut len = [0u8; 4];
        self.inner.read_exact(&mut len).await?;
        let len = u32::from_le_bytes(len) as usize;
        // Every frame carries a 16-byte tag, so anything shorter is nonsense.
        if len > MAX_FRAME || len < 16 {
            return Err(TransportError::FrameTooLarge(len));
        }

        self.scratch.clear();
        self.scratch.resize(len, 0);
        self.inner.read_exact(&mut self.scratch).await?;
        self.sealer.open(&mut self.scratch)?;
        Ok(postcard::from_bytes(&self.scratch)?)
    }
}

fn split(stream: TcpStream, keys: SessionKeys, is_host: bool) -> (FrameReader, FrameWriter) {
    let (read, write) = stream.into_split();
    let (send_key, recv_key) = if is_host {
        (keys.host_to_client, keys.client_to_host)
    } else {
        (keys.client_to_host, keys.host_to_client)
    };
    (
        FrameReader {
            inner: read,
            sealer: Sealer::new(&recv_key),
            scratch: Vec::with_capacity(1024),
        },
        FrameWriter {
            inner: write,
            sealer: Sealer::new(&send_key),
            scratch: Vec::with_capacity(1024),
        },
    )
}

/// Client side of the handshake.
///
/// `auth` is derived from the pairing code on first contact, or is the stored
/// key from a previous pairing. A wrong code produces different session keys,
/// so it fails at the confirmation step rather than in a code comparison.
pub async fn client_handshake(
    mut stream: TcpStream,
    device: DeviceId,
    mode: AuthMode,
    auth: &Secret,
) -> Result<(FrameReader, FrameWriter), TransportError> {
    stream.set_nodelay(true)?;

    let hs = Handshake::new();
    let client_public = hs.public;

    let mut intro = Vec::with_capacity(INTRO_LEN);
    intro.extend_from_slice(&MAGIC);
    intro.push(PROTOCOL_VERSION);
    intro.push(mode.as_byte());
    intro.extend_from_slice(&device);
    intro.extend_from_slice(&client_public);
    stream.write_all(&intro).await?;

    let mut reply = [0u8; REPLY_LEN];
    stream.read_exact(&mut reply).await?;
    if reply[..4] != MAGIC || reply[4] != PROTOCOL_VERSION {
        return Err(TransportError::Handshake);
    }
    let mut host_public = [0u8; 32];
    host_public.copy_from_slice(&reply[5..]);

    let keys = hs.finish(&host_public, &client_public, &host_public, auth);

    stream.write_all(&keys.client_confirmation()).await?;
    let mut host_tag = [0u8; 32];
    stream.read_exact(&mut host_tag).await?;
    keys.verify_host(&host_tag)?;

    Ok(split(stream, keys, false))
}

/// Host side, step one: read the client's intro so the caller can pick between
/// the pairing code and a stored key before any key material is derived.
pub async fn host_read_intent(stream: &mut TcpStream) -> Result<ClientIntent, TransportError> {
    stream.set_nodelay(true)?;

    let mut intro = [0u8; INTRO_LEN];
    stream.read_exact(&mut intro).await?;
    if intro[..4] != MAGIC || intro[4] != PROTOCOL_VERSION {
        return Err(TransportError::Handshake);
    }

    let mode = AuthMode::from_byte(intro[5]).ok_or(TransportError::Handshake)?;
    let mut device = [0u8; 16];
    device.copy_from_slice(&intro[6..22]);
    let mut client_public = [0u8; 32];
    client_public.copy_from_slice(&intro[22..]);

    Ok(ClientIntent {
        device,
        mode,
        client_public,
    })
}

/// Host side, step two: finish the handshake with the chosen secret.
pub async fn host_finish(
    mut stream: TcpStream,
    intent: &ClientIntent,
    auth: &Secret,
) -> Result<(FrameReader, FrameWriter), TransportError> {
    let hs = Handshake::new();
    let host_public = hs.public;

    let mut reply = Vec::with_capacity(REPLY_LEN);
    reply.extend_from_slice(&MAGIC);
    reply.push(PROTOCOL_VERSION);
    reply.extend_from_slice(&host_public);
    stream.write_all(&reply).await?;

    let keys = hs.finish(
        &intent.client_public,
        &intent.client_public,
        &host_public,
        auth,
    );

    let mut client_tag = [0u8; 32];
    stream.read_exact(&mut client_tag).await?;
    keys.verify_client(&client_tag)?;
    stream.write_all(&keys.host_confirmation()).await?;

    Ok(split(stream, keys, true))
}
