//! PGP identity management for Screen Party.
//!
//! Primary key: EdDSA (sign + certify).
//! Encryption subkey: ECDH Curve25519.
//! Keys are saved as ASCII-armoured secret key blocks to ~/.screen-party/identity.asc.

use std::{
    io::{self, Write},
    path::PathBuf,
};

use pgp::{
    composed::{
        key::{SecretKeyParamsBuilder, SubkeyParamsBuilder},
        Deserializable, KeyType, SignedSecretKey,
    },
    crypto::{ecc_curve::ECCCurve, hash::HashAlgorithm, sym::SymmetricKeyAlgorithm},
    types::{CompressionAlgorithm, KeyTrait, SecretKeyTrait},
    ArmorOptions,
};
use smallvec::smallvec;
use zeroize::{Zeroize, Zeroizing};

// ── Identity type ─────────────────────────────────────────────────────────────

pub struct PgpIdentity {
    pub secret_key:  SignedSecretKey,
    pub fingerprint: String,
    passphrase: Zeroizing<String>,
}

impl Drop for PgpIdentity {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

impl PgpIdentity {
    /// Generate a fresh EdDSA + ECDH Curve25519 keypair.
    pub fn generate(nickname: &str, passphrase: Zeroizing<String>) -> anyhow::Result<Self> {
        let user_id = format!("{} <{}@screen-party>", nickname, nickname.to_lowercase());
        let pw_sign = passphrase.clone();

        let params = SecretKeyParamsBuilder::default()
            .key_type(KeyType::EdDSA)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id(user_id)
            .preferred_symmetric_algorithms(smallvec![SymmetricKeyAlgorithm::AES256])
            .preferred_hash_algorithms(smallvec![HashAlgorithm::SHA2_256])
            .preferred_compression_algorithms(smallvec![CompressionAlgorithm::ZLIB])
            .subkeys(vec![
                SubkeyParamsBuilder::default()
                    .key_type(KeyType::ECDH(ECCCurve::Curve25519))
                    .can_encrypt(true)
                    .build()
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            ])
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let secret = params.generate().map_err(|e| anyhow::anyhow!("{e}"))?;
        let signed_secret = secret
            .sign(move || pw_sign.as_str().to_owned())
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let fingerprint = hex::encode(signed_secret.public_key().fingerprint());

        Ok(Self { secret_key: signed_secret, fingerprint, passphrase })
    }

    /// Load from an ASCII-armoured secret key file.
    pub fn from_armored_file(path: &PathBuf, passphrase: Zeroizing<String>) -> anyhow::Result<Self> {
        use std::io::Cursor;
        let armored = std::fs::read_to_string(path)?;

        let (signed_secret, _) =
            SignedSecretKey::from_armor_single(Cursor::new(armored.as_bytes()))
                .map_err(|e| anyhow::anyhow!("{e}"))?;

        let fingerprint = hex::encode(signed_secret.public_key().fingerprint());

        Ok(Self { secret_key: signed_secret, fingerprint, passphrase })
    }

    pub fn secret_key_armored(&self) -> anyhow::Result<String> {
        self.secret_key
            .to_armored_string(ArmorOptions::default())
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

/// Load the fingerprint from the saved identity, if it exists.
/// Returns `None` if no identity has been generated yet.
/// If the key is passphrase-protected, prompts on the terminal.
pub fn load_fingerprint() -> Option<String> {
    let path = identity_path();
    if !path.exists() { return None; }

    // Try with an empty passphrase first (unprotected key — the common case).
    let empty = Zeroizing::new(String::new());
    if let Ok(id) = PgpIdentity::from_armored_file(&path, empty) {
        return Some(id.fingerprint.clone());
    }

    // Key is passphrase-protected — prompt the user.
    let passphrase = Zeroizing::new(
        rpassword::prompt_password("Identity passphrase: ").ok()?,
    );
    PgpIdentity::from_armored_file(&path, passphrase)
        .ok()
        .map(|id| id.fingerprint.clone())
}

pub fn identity_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".screen-party").join("identity.asc")
}

// ── Interactive flows ─────────────────────────────────────────────────────────

/// Run the keygen wizard: prompt for nickname + passphrase, save to the
/// default identity path.  Returns the generated identity.
pub fn run_keygen_wizard() -> anyhow::Result<PgpIdentity> {
    let path = identity_path();

    if path.exists() {
        print!(
            "Identity already exists at {}.\nOverwrite? [yes/no]: ",
            path.display()
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("yes") {
            anyhow::bail!("keygen cancelled");
        }
    }

    println!("\n=== Generate PGP Identity ===");
    print!("Nickname: ");
    io::stdout().flush()?;
    let mut nickname = String::new();
    io::stdin().read_line(&mut nickname)?;
    let nickname = nickname.trim().to_owned();
    if nickname.is_empty() {
        anyhow::bail!("nickname cannot be empty");
    }

    let passphrase = Zeroizing::new(
        rpassword::prompt_password("Passphrase (empty for none): ")?,
    );
    if !passphrase.is_empty() {
        let confirm = Zeroizing::new(rpassword::prompt_password("Confirm passphrase: ")?);
        if *passphrase != *confirm {
            anyhow::bail!("passphrases do not match");
        }
    }

    print!("Generating key...");
    io::stdout().flush()?;
    let identity = PgpIdentity::generate(&nickname, passphrase)?;
    println!(" done.");
    println!("Fingerprint: {}", identity.fingerprint);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, identity.secret_key_armored()?)?;
    println!("Saved to:    {}", path.display());

    Ok(identity)
}

/// Prompt the user to confirm a host's key fingerprint before connecting.
/// Returns `true` if the user types "yes".
pub fn interactive_key_confirm(host: &str, fingerprint: &str) -> io::Result<bool> {
    println!("\nHost {} presents fingerprint:", host);
    println!("  {}", fingerprint);
    print!("Connect? [yes/no]: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("yes"))
}

/// Load the existing identity fingerprint, or silently generate a new one
/// using the OS username as the nickname and no passphrase.  Always returns a
/// non-empty string (falls back to empty only if both steps fail).
pub fn ensure_identity() -> String {
    if let Some(fp) = load_fingerprint() {
        return fp;
    }
    let nickname = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "viewer".to_string());
    let passphrase = Zeroizing::new(String::new());
    match PgpIdentity::generate(&nickname, passphrase) {
        Ok(id) => {
            let path = identity_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(armored) = id.secret_key_armored() {
                let _ = std::fs::write(&path, armored);
            }
            eprintln!("[identity] generated new identity: {}", id.fingerprint);
            id.fingerprint.clone()
        }
        Err(e) => {
            eprintln!("[identity] auto-generate failed: {e}");
            String::new()
        }
    }
}

// ── Known hosts ───────────────────────────────────────────────────────────────

pub enum KnownHostStatus {
    Unknown,
    Trusted,
    FingerprintChanged,
}

pub fn known_hosts_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".screen-party").join("known_hosts")
}

/// Check if `host:port` is in the known-hosts file.
/// Returns `Trusted` if fingerprint matches, `FingerprintChanged` if it differs,
/// `Unknown` if the host has not been seen before.
pub fn check_known_host(host: &str, port: u16, fingerprint: &str) -> KnownHostStatus {
    let content = match std::fs::read_to_string(known_hosts_path()) {
        Ok(c) => c,
        Err(_) => return KnownHostStatus::Unknown,
    };
    let key = format!("{host}:{port}");
    for line in content.lines() {
        let mut parts = line.splitn(2, ' ');
        if let (Some(k), Some(fp)) = (parts.next(), parts.next()) {
            if k == key {
                return if fp.trim() == fingerprint {
                    KnownHostStatus::Trusted
                } else {
                    KnownHostStatus::FingerprintChanged
                };
            }
        }
    }
    KnownHostStatus::Unknown
}

/// Append `host:port fingerprint` to the known-hosts file.
pub fn save_known_host(host: &str, port: u16, fingerprint: &str) -> io::Result<()> {
    let path = known_hosts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(format!("{host}:{port} {fingerprint}\n").as_bytes())
}
