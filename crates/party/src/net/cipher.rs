//! Session-layer encryption: X25519 ECDH key exchange + ChaCha20-Poly1305 AEAD.
//!
//! Handshake flow (all plaintext, no stream yet):
//!   1. Host embeds its ephemeral X25519 public key in HANDSHAKE.
//!   2. Client embeds its ephemeral X25519 public key in HANDSHAKE_ACK.
//!   3. Both derive two symmetric keys via HKDF-SHA-256 from the shared secret:
//!      one for host→client, one for client→host.
//!
//! Every subsequent wire message is one AEAD record:
//!   [u32 ciphertext_len LE][ChaCha20-Poly1305 ciphertext + 16-byte tag]
//! Nonces are 96-bit counters (little-endian u64 in the low 8 bytes), one per direction.
//!
//! `EncryptedWriter` / `EncryptedReader` implement `Write` / `Read` transparently —
//! all existing `proto::write_msg` / `proto::read_msg` calls work unchanged.
//! One record is produced per message because `write_msg` always ends with `flush()`.

use std::io::{self, Read, Write};

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

// ── Key exchange ──────────────────────────────────────────────────────────────

pub struct EphemeralKeypair {
    secret:           EphemeralSecret,
    pub public_bytes: [u8; 32],
}

impl EphemeralKeypair {
    pub fn generate() -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let public  = PublicKey::from(&secret);
        Self { secret, public_bytes: *public.as_bytes() }
    }

    /// Consume the keypair, perform DH with the peer's public key, and return
    /// a pair of directional ciphers ready for use.
    pub fn complete(self, peer_pubkey: &[u8; 32], role: Role) -> SessionKeys {
        let peer   = PublicKey::from(*peer_pubkey);
        let shared = self.secret.diffie_hellman(&peer);
        SessionKeys::derive(shared.as_bytes(), role)
    }
}

// ── Role-based key derivation ─────────────────────────────────────────────────

pub enum Role { Host, Client }

pub struct SessionKeys {
    pub send: SendCipher,
    pub recv: RecvCipher,
}

impl SessionKeys {
    fn derive(shared: &[u8; 32], role: Role) -> Self {
        let hk = Hkdf::<Sha256>::new(None, shared);
        let mut h2c = [0u8; 32];
        let mut c2h = [0u8; 32];
        hk.expand(b"screen-party v3 h2c", &mut h2c).expect("hkdf h2c");
        hk.expand(b"screen-party v3 c2h", &mut c2h).expect("hkdf c2h");
        let (send_key, recv_key) = match role {
            Role::Host   => (h2c, c2h),
            Role::Client => (c2h, h2c),
        };
        SessionKeys {
            send: SendCipher::new(send_key),
            recv: RecvCipher::new(recv_key),
        }
    }
}

// ── Send cipher ───────────────────────────────────────────────────────────────

pub struct SendCipher {
    cipher:  ChaCha20Poly1305,
    counter: u64,
}

impl SendCipher {
    fn new(key: [u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)), counter: 0 }
    }

    fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[..8].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        self.cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .expect("ChaCha20-Poly1305 encrypt is infallible for valid key/nonce")
    }
}

// ── Recv cipher ───────────────────────────────────────────────────────────────

pub struct RecvCipher {
    cipher:  ChaCha20Poly1305,
    counter: u64,
}

impl RecvCipher {
    fn new(key: [u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)), counter: 0 }
    }

    fn open(&mut self, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[..8].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        self.cipher
            .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "AEAD authentication failed"))
    }
}

// ── Encrypted writer ──────────────────────────────────────────────────────────

/// Buffers plaintext writes; on each `flush` seals all buffered bytes as one
/// AEAD record and writes it to the inner sink.
///
/// Wire record: `[u32 ciphertext_len LE][ciphertext + 16-byte tag]`
pub struct EncryptedWriter<W: Write> {
    inner:  W,
    cipher: SendCipher,
    buf:    Vec<u8>,
}

impl<W: Write> EncryptedWriter<W> {
    pub fn new(inner: W, cipher: SendCipher) -> Self {
        Self { inner, cipher, buf: Vec::new() }
    }
}

impl<W: Write> Write for EncryptedWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() { return Ok(()); }
        let ct = self.cipher.seal(&self.buf);
        self.buf.clear();
        self.inner.write_all(&(ct.len() as u32).to_le_bytes())?;
        self.inner.write_all(&ct)?;
        self.inner.flush()
    }
}

// ── Encrypted reader ──────────────────────────────────────────────────────────

/// Reads one AEAD record at a time from the inner source, decrypts it into an
/// internal buffer, and serves plaintext bytes from there.
pub struct EncryptedReader<R: Read> {
    inner:  R,
    cipher: RecvCipher,
    buf:    Vec<u8>,
    pos:    usize,
}

impl<R: Read> EncryptedReader<R> {
    pub fn new(inner: R, cipher: RecvCipher) -> Self {
        Self { inner, cipher, buf: Vec::new(), pos: 0 }
    }
}

impl<R: Read> Read for EncryptedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            // Fetch and decrypt the next record.
            let mut len_buf = [0u8; 4];
            self.inner.read_exact(&mut len_buf)?;
            let ct_len = u32::from_le_bytes(len_buf) as usize;
            if ct_len > 32 * 1024 * 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "encrypted record too large"));
            }
            let mut ct = vec![0u8; ct_len];
            self.inner.read_exact(&mut ct)?;
            self.buf = self.cipher.open(&ct)?;
            self.pos = 0;
        }
        let avail = &self.buf[self.pos..];
        let n = out.len().min(avail.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n;
        Ok(n)
    }
}
