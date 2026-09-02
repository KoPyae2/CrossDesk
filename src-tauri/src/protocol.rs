//! Wire protocol for CrossDesk.
//!
//! Everything here is encoded with `postcard`, a non-self-describing binary
//! format: an input event costs a handful of bytes instead of the ~80 a JSON
//! encoding would need. Field order is part of the protocol — only ever append
//! variants, never reorder them.

use serde::{Deserialize, Serialize};

/// Bumped whenever the framing or message layout changes incompatibly.
pub const PROTOCOL_VERSION: u8 = 1;

/// Magic prefix on the very first bytes of a TCP connection, so a stray
/// connection from something else is rejected before any crypto work.
pub const MAGIC: [u8; 4] = *b"XDSK";

/// TCP port the host listens on for peers.
pub const DEFAULT_PORT: u16 = 47810;
/// UDP port used for LAN discovery.
pub const DISCOVERY_PORT: u16 = 47811;

/// Frames larger than this are refused outright (guards against a bad peer
/// making us allocate). Large clipboard images are the only big payload.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

pub type DeviceId = [u8; 16];

pub fn id_to_hex(id: &DeviceId) -> String {
    let mut s = String::with_capacity(32);
    for b in id {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

pub fn id_from_hex(s: &str) -> Option<DeviceId> {
    let b = s.as_bytes();
    if b.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in b.chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// One physical monitor, in the owning device's own virtual-desktop pixels.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub primary: bool,
}

/// Identity a peer announces once the secure channel is up.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub os: String,
    pub displays: Vec<Display>,
}

/// Mouse buttons, kept small and platform neutral.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

/// A single piece of input to replay on the client.
///
/// Coordinates are already translated into the *receiving* device's virtual
/// desktop pixels — the host owns all layout math so clients stay dumb and fast.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum InputEvent {
    /// Absolute pointer position in the client's own desktop coordinates.
    MoveAbs { x: i32, y: i32 },
    Button { button: Button, down: bool },
    /// Wheel deltas in 1/120 of a notch, matching what Windows reports, so
    /// high-resolution wheels keep their precision end to end.
    Wheel { dx: i16, dy: i16 },
    /// `scan` is the physical key (preferred, layout independent); `vk` is the
    /// Windows virtual-key code used as a fallback and for hotkey logic.
    Key {
        vk: u16,
        scan: u16,
        ext: bool,
        down: bool,
    },
    /// Type literal text (used for characters no scancode can express).
    Text(String),
    /// Release every key/button we might have left held down.
    ReleaseAll,
}

/// Clipboard payloads. Text is the common case; images are opt-in per settings.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Clip {
    Text(String),
    Image {
        width: u32,
        height: u32,
        /// Raw RGBA8, row-major, no padding.
        rgba: Vec<u8>,
    },
}

/// Host -> client.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum HostMsg {
    Welcome {
        host: DeviceInfo,
        /// Long-term key to reuse so the pairing code is only ever typed once.
        psk: Option<[u8; 32]>,
        clipboard_sync: bool,
    },
    /// Control just moved onto this client; warp its pointer here first.
    Enter { x: i32, y: i32 },
    /// Control left this client.
    Leave,
    Input(InputEvent),
    Clipboard(Clip),
    Ping { stamp: u64 },
    Bye { reason: String },
}

/// Client -> host.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClientMsg {
    Hello { device: DeviceInfo },
    DisplaysChanged { displays: Vec<Display> },
    Clipboard(Clip),
    Pong { stamp: u64 },
    Bye { reason: String },
}

/// UDP discovery beacon. Deliberately contains nothing secret: it is broadcast
/// in the clear so clients can list hosts before any pairing happens.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Beacon {
    /// Sent by a host in reply to a `Query`.
    Announce {
        protocol: u8,
        id: DeviceId,
        name: String,
        os: String,
        port: u16,
        /// True while the host is willing to pair with a device it has never
        /// seen. A client that already holds a stored key ignores this.
        needs_code: bool,
    },
    /// Sent by a client that wants hosts to answer immediately.
    Query { protocol: u8 },
}
