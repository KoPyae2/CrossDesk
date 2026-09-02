//! Pairing and transport keys.
//!
//! There is no server and no account: a client proves it is allowed in by
//! knowing the host's short pairing code. The code is mixed into the key
//! derivation, so a wrong code produces a different session key and the
//! handshake simply fails to authenticate — we never compare codes directly.
//!
//! After a successful pairing the host hands the client a random 32-byte
//! pre-shared key over the encrypted channel, so the code only ever has to be
//! typed once per device.

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use x25519_dalek::{EphemeralSecret, PublicKey};

const CTX_CODE: &str = "crossdesk.v1.code";
const CTX_SESSION: &str = "crossdesk.v1.session";
const CTX_C2H: &str = "crossdesk.v1.client-to-host";
const CTX_H2C: &str = "crossdesk.v1.host-to-client";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("peer failed authentication (wrong pairing code?)")]
    Auth,
    #[error("message could not be decrypted; connection is out of sync")]
    Decrypt,
    #[error("message was too large to encode")]
    TooLarge,
}

pub type Secret = [u8; 32];

/// Turn a human pairing code into the shared secret used for a first pairing.
/// Whitespace and case are ignored so "1234 56" and "123456" behave the same.
pub fn secret_from_code(code: &str) -> Secret {
    let normalised: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(|c| c.to_uppercase())
        .collect();
    blake3::derive_key(CTX_CODE, normalised.as_bytes())
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("system randomness is unavailable");
    out
}

/// Six digits, grouped as `123 456` in the UI. Short enough to read aloud.
pub fn random_pairing_code() -> String {
    let raw: [u8; 8] = random_bytes();
    let mut n = u64::from_le_bytes(raw) % 1_000_000;
    let mut digits = [0u8; 6];
    for slot in digits.iter_mut().rev() {
        *slot = b'0' + (n % 10) as u8;
        n /= 10;
    }
    String::from_utf8(digits.to_vec()).unwrap()
}

/// One side's half of the key agreement.
pub struct Handshake {
    secret: EphemeralSecret,
    pub public: [u8; 32],
}

/// Everything derived once both public keys and the auth secret are known.
pub struct SessionKeys {
    pub client_to_host: Secret,
    pub host_to_client: Secret,
    confirm_client: blake3::Hash,
    confirm_host: blake3::Hash,
}

impl Handshake {
    pub fn new() -> Self {
        let secret = EphemeralSecret::random();
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// Consumes the ephemeral secret; `auth` is the pairing-code secret or a
    /// stored pre-shared key. Binding both public keys and `auth` into the
    /// derivation is what authenticates the peer.
    pub fn finish(
        self,
        peer_public: &[u8; 32],
        client_public: &[u8; 32],
        host_public: &[u8; 32],
        auth: &Secret,
    ) -> SessionKeys {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer_public));

        let mut material = Vec::with_capacity(32 * 4);
        material.extend_from_slice(shared.as_bytes());
        material.extend_from_slice(client_public);
        material.extend_from_slice(host_public);
        material.extend_from_slice(auth);
        let session = blake3::derive_key(CTX_SESSION, &material);

        SessionKeys {
            client_to_host: blake3::derive_key(CTX_C2H, &session),
            host_to_client: blake3::derive_key(CTX_H2C, &session),
            confirm_client: blake3::keyed_hash(&session, b"client-confirm"),
            confirm_host: blake3::keyed_hash(&session, b"host-confirm"),
        }
    }
}

impl SessionKeys {
    pub fn client_confirmation(&self) -> [u8; 32] {
        *self.confirm_client.as_bytes()
    }

    pub fn host_confirmation(&self) -> [u8; 32] {
        *self.confirm_host.as_bytes()
    }

    /// Both comparisons run in constant time (`blake3::Hash` compares that way).
    pub fn verify_client(&self, tag: &[u8; 32]) -> Result<(), CryptoError> {
        (self.confirm_client == blake3::Hash::from(*tag))
            .then_some(())
            .ok_or(CryptoError::Auth)
    }

    pub fn verify_host(&self, tag: &[u8; 32]) -> Result<(), CryptoError> {
        (self.confirm_host == blake3::Hash::from(*tag))
            .then_some(())
            .ok_or(CryptoError::Auth)
    }
}

/// A one-directional AEAD channel.
///
/// The nonce is a local counter that is never transmitted, so a replayed,
/// dropped or reordered frame fails authentication and kills the connection.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    pub fn new(key: &Secret) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(&Key::from(*key)),
            counter: 0,
        }
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut raw = [0u8; 12];
        raw[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        Nonce::from(raw)
    }

    /// Encrypts in place, appending the 16-byte tag.
    pub fn seal(&mut self, buf: &mut Vec<u8>) -> Result<(), CryptoError> {
        let nonce = self.next_nonce();
        self.cipher
            .encrypt_in_place(&nonce, &[], buf)
            .map_err(|_| CryptoError::TooLarge)
    }

    /// Decrypts in place, removing the tag.
    pub fn open(&mut self, buf: &mut Vec<u8>) -> Result<(), CryptoError> {
        let nonce = self.next_nonce();
        self.cipher
            .decrypt_in_place(&nonce, &[], buf)
            .map_err(|_| CryptoError::Decrypt)
    }
}
