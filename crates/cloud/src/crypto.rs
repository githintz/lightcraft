//! Blob encryption: AES-256-GCM over fixed-size segments, each blob under its own random file
//! key wrapped with the library's data key (`docs/cloud-format.md` → Encryption).
//!
//! ```text
//! header (80 bytes):  "LCB1" | version 1 | alg 1 (AES-256-GCM) | 0u16 | segment size u32 LE
//!                     | nonce prefix (8) | wrap nonce (12) | wrapped file key (32 + 16-byte tag)
//! segments:           ciphertext | 16-byte tag, for each `segment size` bytes of plaintext
//! ```
//!
//! Segment `i` uses nonce `prefix ‖ u32 BE (i, with the high bit set on the last segment)` and
//! the whole header as associated data, so segments can't be reordered, dropped, cut off at the
//! end, or moved to another blob's header without the tag check failing. An empty blob has one
//! empty last segment. The file key is wrapped with the first 20 header bytes as associated
//! data.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

use crate::{CloudError, Result};

pub const MAGIC: &[u8; 4] = b"LCB1";
pub const HEADER_LEN: usize = 80;
pub const TAG_LEN: usize = 16;
/// Segment size new blobs are written with.
pub const DEFAULT_SEGMENT: u32 = 4 * 1024 * 1024;
/// Segment sizes a reader accepts (a hostile header can't make it allocate more per segment).
pub const MIN_SEGMENT: u32 = 4 * 1024;
pub const MAX_SEGMENT: u32 = 16 * 1024 * 1024;
const LAST: u32 = 0x8000_0000;

/// Where random bytes come from: the OS on the desktop ([`OsRandom`]), the browser's
/// `crypto.getRandomValues` in the web host, a fixed sequence in tests.
pub trait Random {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()>;
}

/// The operating system's random source.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

#[cfg(not(target_arch = "wasm32"))]
impl Random for OsRandom {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        getrandom::getrandom(buf).map_err(|e| CloudError::Io(format!("no random numbers from the system: {e}")))
    }
}

/// A 256-bit key. Its `Debug` never prints the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Key(pub(crate) [u8; 32]);

impl Key {
    pub fn random(rng: &mut dyn Random) -> Result<Key> {
        let mut k = [0u8; 32];
        rng.fill(&mut k)?;
        Ok(Key(k))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Key {
        Key(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn cipher(&self) -> Result<Aes256Gcm> {
        Aes256Gcm::new_from_slice(&self.0).map_err(|_| CloudError::Corrupt("bad key length".into()))
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(…)")
    }
}

fn segment_nonce(prefix: &[u8], index: u32, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    if let Some(dst) = n.get_mut(..8) {
        dst.copy_from_slice(prefix.get(..8).unwrap_or(&[0; 8]));
    }
    let i = if last { index | LAST } else { index };
    if let Some(dst) = n.get_mut(8..) {
        dst.copy_from_slice(&i.to_be_bytes());
    }
    n
}

/// Length of the encrypted form of `plain_len` bytes with `segment` -byte segments, `None` when
/// it would need more than 2³¹ segments.
pub fn encrypted_len(plain_len: u64, segment: u32) -> Option<u64> {
    let segments = plain_len.div_ceil(u64::from(segment.max(1))).max(1);
    if segments > u64::from(LAST) {
        return None;
    }
    (HEADER_LEN as u64).checked_add(plain_len)?.checked_add(segments.checked_mul(TAG_LEN as u64)?)
}

/// Encrypt `plain` under a new random file key wrapped with `data_key`.
pub fn encrypt(data_key: &Key, plain: &[u8], rng: &mut dyn Random) -> Result<Vec<u8>> {
    encrypt_with_segment(data_key, plain, DEFAULT_SEGMENT, rng)
}

pub fn encrypt_with_segment(data_key: &Key, plain: &[u8], segment: u32, rng: &mut dyn Random) -> Result<Vec<u8>> {
    if !(MIN_SEGMENT..=MAX_SEGMENT).contains(&segment) {
        return Err(CloudError::Unsupported(format!("segment size {segment}")));
    }
    let total = encrypted_len(plain.len() as u64, segment).ok_or(CloudError::TooLarge { size: plain.len() as u64, max: u64::MAX })?;
    let file_key = Key::random(rng)?;
    let mut nonce_prefix = [0u8; 8];
    rng.fill(&mut nonce_prefix)?;
    let mut wrap_nonce = [0u8; 12];
    rng.fill(&mut wrap_nonce)?;

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&[1, 1, 0, 0]);
    header.extend_from_slice(&segment.to_le_bytes());
    header.extend_from_slice(&nonce_prefix);
    let wrapped = data_key
        .cipher()?
        .encrypt(Nonce::from_slice(&wrap_nonce), Payload { msg: file_key.as_bytes(), aad: &header })
        .map_err(|_| CloudError::Corrupt("could not wrap the file key".into()))?;
    header.extend_from_slice(&wrap_nonce);
    header.extend_from_slice(&wrapped);
    if header.len() != HEADER_LEN {
        return Err(CloudError::Corrupt(format!("header is {} bytes", header.len())));
    }

    let cipher = file_key.cipher()?;
    let mut out = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    out.extend_from_slice(&header);
    let seg = usize::try_from(segment).unwrap_or(usize::MAX);
    let chunks: Vec<&[u8]> = if plain.is_empty() { vec![&[][..]] } else { plain.chunks(seg).collect() };
    let count = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let index = u32::try_from(i).map_err(|_| CloudError::TooLarge { size: plain.len() as u64, max: u64::MAX })?;
        let nonce = segment_nonce(&nonce_prefix, index, i + 1 == count);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: chunk, aad: &header })
            .map_err(|_| CloudError::Corrupt("encryption failed".into()))?;
        out.extend_from_slice(&ct);
    }
    Ok(out)
}

/// Decrypt a blob written by [`encrypt`]. Refuses (before decrypting) a blob whose plaintext
/// would exceed `max_plain`; any tampering, truncation or wrong key is [`CloudError::Corrupt`].
pub fn decrypt(data_key: &Key, data: &[u8], max_plain: u64) -> Result<Vec<u8>> {
    let corrupt = |why: &str| CloudError::Corrupt(format!("encrypted blob: {why}"));
    let header = data.get(..HEADER_LEN).ok_or_else(|| corrupt("shorter than its header"))?;
    if header.get(..4) != Some(MAGIC.as_slice()) {
        return Err(corrupt("not an encrypted blob"));
    }
    if header.get(4..8) != Some([1u8, 1, 0, 0].as_slice()) {
        return Err(CloudError::Unsupported("encrypted blob version or algorithm".into()));
    }
    let segment = header.get(8..12).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_le_bytes).ok_or_else(|| corrupt("header"))?;
    if !(MIN_SEGMENT..=MAX_SEGMENT).contains(&segment) {
        return Err(corrupt("unsupported segment size"));
    }
    let nonce_prefix = header.get(12..20).ok_or_else(|| corrupt("header"))?;
    let wrap_nonce = header.get(20..32).ok_or_else(|| corrupt("header"))?;
    let wrapped = header.get(32..80).ok_or_else(|| corrupt("header"))?;
    let aad_wrap = header.get(..20).ok_or_else(|| corrupt("header"))?;
    let file_key = data_key
        .cipher()?
        .decrypt(Nonce::from_slice(wrap_nonce), Payload { msg: wrapped, aad: aad_wrap })
        .map_err(|_| corrupt("wrong key or damaged header"))?;
    let file_key = <[u8; 32]>::try_from(file_key.as_slice()).map_err(|_| corrupt("file key"))?;
    let cipher = Key(file_key).cipher()?;

    let body = data.get(HEADER_LEN..).unwrap_or_default();
    let seg_ct = usize::try_from(segment).unwrap_or(usize::MAX).saturating_add(TAG_LEN);
    let segments = body.len().div_ceil(seg_ct).max(1);
    let plain_len = (body.len() as u64).saturating_sub((segments as u64).saturating_mul(TAG_LEN as u64));
    if plain_len > max_plain {
        return Err(CloudError::TooLarge { size: plain_len, max: max_plain });
    }
    if body.len() < TAG_LEN || u32::try_from(segments).map_or(true, |s| s > LAST) {
        return Err(corrupt("cut short"));
    }
    let mut out = Vec::with_capacity(usize::try_from(plain_len).unwrap_or(0));
    for (i, chunk) in body.chunks(seg_ct).enumerate() {
        if chunk.len() < TAG_LEN {
            return Err(corrupt("cut short"));
        }
        let index = u32::try_from(i).map_err(|_| corrupt("too many segments"))?;
        let nonce = segment_nonce(nonce_prefix, index, i + 1 == segments);
        let pt = cipher.decrypt(Nonce::from_slice(&nonce), Payload { msg: chunk, aad: header }).map_err(|_| corrupt("damaged or cut short"))?;
        out.extend_from_slice(&pt);
    }
    Ok(out)
}

/// A deterministic byte sequence, for tests only (never for real keys).
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct TestRandom(pub u64);

impl Random for TestRandom {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        for b in buf {
            // splitmix64
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *b = (z ^ (z >> 31)) as u8;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> Key {
        Key([n; 32])
    }

    #[test]
    fn round_trips_every_size_around_segment_edges() {
        let seg = MIN_SEGMENT;
        let mut rng = TestRandom(1);
        for len in [0usize, 1, 4095, 4096, 4097, 8192, 10_000] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let ct = encrypt_with_segment(&key(1), &plain, seg, &mut rng).unwrap();
            assert_eq!(ct.len() as u64, encrypted_len(len as u64, seg).unwrap(), "len {len}");
            assert_eq!(decrypt(&key(1), &ct, len as u64).unwrap(), plain, "len {len}");
        }
    }

    #[test]
    fn detects_wrong_key_tampering_truncation_and_reordering() {
        let mut rng = TestRandom(2);
        let plain = vec![5u8; 3 * MIN_SEGMENT as usize];
        let ct = encrypt_with_segment(&key(1), &plain, MIN_SEGMENT, &mut rng).unwrap();
        assert!(matches!(decrypt(&key(2), &ct, u64::MAX), Err(CloudError::Corrupt(_))));
        let seg_ct = MIN_SEGMENT as usize + TAG_LEN;
        // a flipped bit in a segment, in the header, in the segment size
        for pos in [HEADER_LEN + 10, 13, 40] {
            let mut bad = ct.clone();
            bad[pos] ^= 1;
            assert!(decrypt(&key(1), &bad, u64::MAX).is_err(), "flip at {pos}");
        }
        // the last segment dropped: the new last segment lacks the "last" flag
        let cut = &ct[..HEADER_LEN + 2 * seg_ct];
        assert!(matches!(decrypt(&key(1), cut, u64::MAX), Err(CloudError::Corrupt(_))));
        // segments swapped
        let mut swapped = ct[..HEADER_LEN].to_vec();
        swapped.extend_from_slice(&ct[HEADER_LEN + seg_ct..HEADER_LEN + 2 * seg_ct]);
        swapped.extend_from_slice(&ct[HEADER_LEN..HEADER_LEN + seg_ct]);
        swapped.extend_from_slice(&ct[HEADER_LEN + 2 * seg_ct..]);
        assert!(matches!(decrypt(&key(1), &swapped, u64::MAX), Err(CloudError::Corrupt(_))));
        // a header moved onto another blob's segments
        let other = encrypt_with_segment(&key(1), &plain, MIN_SEGMENT, &mut rng).unwrap();
        let mut mixed = other[..HEADER_LEN].to_vec();
        mixed.extend_from_slice(&ct[HEADER_LEN..]);
        assert!(matches!(decrypt(&key(1), &mixed, u64::MAX), Err(CloudError::Corrupt(_))));
    }

    #[test]
    fn hostile_inputs_are_errors() {
        let mut rng = TestRandom(3);
        let ct = encrypt_with_segment(&key(1), &[1; 100], MIN_SEGMENT, &mut rng).unwrap();
        for len in 0..HEADER_LEN + TAG_LEN {
            assert!(decrypt(&key(1), &ct[..len.min(ct.len())], u64::MAX).is_err(), "prefix {len}");
        }
        assert!(matches!(decrypt(&key(1), &ct, 99), Err(CloudError::TooLarge { size: 100, max: 99 })));
        let mut huge_seg = ct.clone();
        huge_seg[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decrypt(&key(1), &huge_seg, u64::MAX).is_err());
        assert!(encrypt_with_segment(&key(1), &[], 1, &mut rng).is_err());
        assert_eq!(encrypted_len(u64::MAX, MIN_SEGMENT), None);
    }

    #[test]
    fn same_plaintext_encrypts_differently() {
        let mut rng = TestRandom(4);
        let a = encrypt(&key(1), b"photo", &mut rng).unwrap();
        let b = encrypt(&key(1), b"photo", &mut rng).unwrap();
        assert_ne!(a, b);
        assert_eq!(format!("{:?}", key(9)), "Key(…)");
    }
}
