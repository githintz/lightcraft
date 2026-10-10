//! The vault: the library's data key (which wraps every blob's file key) and the credentials of
//! its storage accounts, encrypted under a key derived from the user's passphrase.
//!
//! ```text
//! "LCV1" | version 1 | kdf 1 (Argon2id v1.3) | 0u16 | m_cost KiB u32 LE | t_cost u32 LE
//! | lanes u32 LE | salt (16) | nonce (12) | AES-256-GCM(JSON) with the 44 bytes before it as AAD
//! ```
//!
//! Changing the passphrase rewrites only the vault: blobs stay wrapped under the same data key.
//! The cost parameters come from the file, so they are bounded ([`VaultParams::check`]) before
//! anything is allocated.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::{Deserialize, Serialize};

use crate::crypto::{Key, Random};
use crate::{CloudError, Result};

pub const MAGIC: &[u8; 4] = b"LCV1";
const HEAD_LEN: usize = 4 + 4 + 12 + 16 + 12;
/// Largest vault accepted (credentials for many accounts fit in a few KiB).
pub const MAX_VAULT: usize = 1024 * 1024;

/// Argon2id cost. [`VaultParams::default`] takes about a second on a laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultParams {
    /// Memory in KiB.
    pub m_cost: u32,
    pub t_cost: u32,
    pub lanes: u32,
}

impl Default for VaultParams {
    fn default() -> Self {
        VaultParams { m_cost: 64 * 1024, t_cost: 3, lanes: 1 }
    }
}

impl VaultParams {
    /// The cheapest parameters accepted: for tests only.
    pub const TEST: VaultParams = VaultParams { m_cost: 8 * 1024, t_cost: 1, lanes: 1 };

    /// 8 MiB–1 GiB of memory, 1–16 passes, 1–8 lanes.
    pub fn check(&self) -> Result<()> {
        if (8 * 1024..=1024 * 1024).contains(&self.m_cost) && (1..=16).contains(&self.t_cost) && (1..=8).contains(&self.lanes) {
            Ok(())
        } else {
            Err(CloudError::Unsupported(format!("vault key-derivation parameters {self:?}")))
        }
    }

    fn derive(&self, passphrase: &str, salt: &[u8]) -> Result<Key> {
        self.check()?;
        let params = argon2::Params::new(self.m_cost, self.t_cost, self.lanes, Some(32))
            .map_err(|e| CloudError::Unsupported(format!("vault parameters: {e}")))?;
        let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut out = [0u8; 32];
        argon.hash_password_into(passphrase.as_bytes(), salt, &mut out).map_err(|e| CloudError::Io(format!("deriving the vault key: {e}")))?;
        Ok(Key::from_bytes(out))
    }
}

/// One storage account's settings and secrets (tokens, keys, passwords), as its provider
/// defines them.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSecret {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub config: serde_json::Value,
}

impl std::fmt::Debug for AccountSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSecret").field("id", &self.id).field("provider", &self.provider).field("label", &self.label).finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    format: String,
    data_key: String,
    accounts: Vec<AccountSecret>,
}

/// An unlocked vault. Its `Debug` prints no secrets.
#[derive(Clone, PartialEq)]
pub struct Vault {
    pub data_key: Key,
    pub accounts: Vec<AccountSecret>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("accounts", &self.accounts).finish_non_exhaustive()
    }
}

impl Vault {
    /// A new vault with a random data key and no accounts.
    pub fn new(rng: &mut dyn Random) -> Result<Vault> {
        Ok(Vault { data_key: Key::random(rng)?, accounts: Vec::new() })
    }

    /// Encrypt under `passphrase` (new salt and nonce every time).
    pub fn seal(&self, passphrase: &str, params: VaultParams, rng: &mut dyn Random) -> Result<Vec<u8>> {
        if passphrase.is_empty() {
            return Err(CloudError::Unsupported("an empty passphrase".into()));
        }
        let mut salt = [0u8; 16];
        rng.fill(&mut salt)?;
        let mut nonce = [0u8; 12];
        rng.fill(&mut nonce)?;
        let kek = params.derive(passphrase, &salt)?;
        let sealed = Sealed { format: "lightcraft-vault".into(), data_key: hex(self.data_key.as_bytes()), accounts: self.accounts.clone() };
        let json = serde_json::to_vec(&sealed).map_err(|e| CloudError::Io(e.to_string()))?;
        let mut out = Vec::with_capacity(HEAD_LEN + json.len() + 16);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&[1, 1, 0, 0]);
        for v in [params.m_cost, params.t_cost, params.lanes] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        let cipher = Aes256Gcm::new_from_slice(kek.as_bytes()).map_err(|_| CloudError::Corrupt("key".into()))?;
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: &json, aad: &out })
            .map_err(|_| CloudError::Corrupt("could not encrypt the vault".into()))?;
        out.extend_from_slice(&ct);
        if out.len() > MAX_VAULT {
            return Err(CloudError::TooLarge { size: out.len() as u64, max: MAX_VAULT as u64 });
        }
        Ok(out)
    }

    /// Decrypt a sealed vault. A wrong passphrase (or a damaged vault, which can't be told
    /// apart) is [`CloudError::WrongPassphrase`].
    pub fn open(data: &[u8], passphrase: &str) -> Result<Vault> {
        let corrupt = |why: &str| CloudError::Corrupt(format!("vault: {why}"));
        if data.len() > MAX_VAULT {
            return Err(CloudError::TooLarge { size: data.len() as u64, max: MAX_VAULT as u64 });
        }
        let head = data.get(..HEAD_LEN).ok_or_else(|| corrupt("too short"))?;
        if head.get(..4) != Some(MAGIC.as_slice()) {
            return Err(corrupt("not a LightCraft vault"));
        }
        if head.get(4..8) != Some([1u8, 1, 0, 0].as_slice()) {
            return Err(CloudError::Unsupported("vault version".into()));
        }
        let word =
            |at: usize| head.get(at..at + 4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_le_bytes).ok_or_else(|| corrupt("header"));
        let params = VaultParams { m_cost: word(8)?, t_cost: word(12)?, lanes: word(16)? };
        let salt = head.get(20..36).ok_or_else(|| corrupt("header"))?;
        let nonce = head.get(36..48).ok_or_else(|| corrupt("header"))?;
        let kek = params.derive(passphrase, salt)?;
        let cipher = Aes256Gcm::new_from_slice(kek.as_bytes()).map_err(|_| corrupt("key"))?;
        let json = cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: data.get(HEAD_LEN..).unwrap_or_default(), aad: head })
            .map_err(|_| CloudError::WrongPassphrase)?;
        let sealed: Sealed = serde_json::from_slice(&json).map_err(|e| corrupt(&e.to_string()))?;
        if sealed.format != "lightcraft-vault" {
            return Err(corrupt("unknown format"));
        }
        let key = unhex32(&sealed.data_key).ok_or_else(|| corrupt("data key"))?;
        Ok(Vault { data_key: Key::from_bytes(key), accounts: sealed.accounts })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(b.get(i * 2..i * 2 + 2)?).ok()?;
        *o = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::TestRandom;

    fn sample(rng: &mut TestRandom) -> Vault {
        let mut v = Vault::new(rng).unwrap();
        v.accounts.push(AccountSecret {
            id: "nas".into(),
            provider: "webdav".into(),
            label: "NAS".into(),
            config: serde_json::json!({"url": "https://nas.local/dav", "user": "me", "password": "hunter2"}),
        });
        v
    }

    #[test]
    fn seals_and_opens() {
        let mut rng = TestRandom(7);
        let v = sample(&mut rng);
        let sealed = v.seal("correct horse", VaultParams::TEST, &mut rng).unwrap();
        assert_eq!(Vault::open(&sealed, "correct horse").unwrap(), v);
        assert_eq!(Vault::open(&sealed, "wrong horse").err(), Some(CloudError::WrongPassphrase));
        // resealing changes every byte after the header's parameters
        let again = v.seal("correct horse", VaultParams::TEST, &mut rng).unwrap();
        assert_ne!(sealed, again);
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let mut rng = TestRandom(8);
        let text = format!("{:?}", sample(&mut rng));
        assert!(!text.contains("hunter2") && text.contains("NAS"), "{text}");
    }

    #[test]
    fn hostile_vaults_are_errors_without_huge_allocations() {
        let mut rng = TestRandom(9);
        let sealed = sample(&mut rng).seal("pw", VaultParams::TEST, &mut rng).unwrap();
        // 4 GiB of memory, a billion passes, 0 lanes: refused before deriving
        for (at, v) in [(8usize, u32::MAX), (12, 1_000_000_000), (16, 0)] {
            let mut bad = sealed.clone();
            bad[at..at + 4].copy_from_slice(&v.to_le_bytes());
            assert!(matches!(Vault::open(&bad, "pw"), Err(CloudError::Unsupported(_))), "field at {at}");
        }
        for len in 0..HEAD_LEN {
            assert!(Vault::open(&sealed[..len], "pw").is_err());
        }
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert_eq!(Vault::open(&flipped, "pw").err(), Some(CloudError::WrongPassphrase));
        assert!(sample(&mut rng).seal("", VaultParams::TEST, &mut rng).is_err());
        assert!(Vault::open(&vec![0; MAX_VAULT + 1], "pw").is_err());
    }
}
