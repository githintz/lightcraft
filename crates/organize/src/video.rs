//! When an MP4 / QuickTime video was made, from its movie header (`moov/mvhd`), reading only
//! box headers and the start of `moov`: a file whose `moov` is at the end (most cameras) costs a
//! few small ranged reads, not a download.

/// Seconds between 1904-01-01 (QuickTime's epoch) and 1970-01-01.
const QT_EPOCH: i64 = 2_082_844_800;
/// Most top-level boxes walked before giving up.
const MAX_BOXES: usize = 64;
/// Bytes of `moov` read to find `mvhd` (its first child in practice).
const MOOV_READ: u64 = 64 * 1024;

/// The creation time (seconds since 1970, as the camera wrote it: cameras often write local time
/// though the format says UTC) of a `size`-byte MP4 / MOV, reading through `read(offset, len)`.
/// `None` when the file has no usable movie header.
pub fn creation_time<E>(size: u64, read: &mut dyn FnMut(u64, u64) -> Result<Vec<u8>, E>) -> Result<Option<i64>, E> {
    let mut off = 0u64;
    for _ in 0..MAX_BOXES {
        if off.saturating_add(8) > size {
            return Ok(None);
        }
        let head = read(off, 16)?;
        let Some((len, kind, header)) = box_header(&head, size - off) else { return Ok(None) };
        if &kind == b"moov" {
            let body = read(off + header, len.saturating_sub(header).min(MOOV_READ))?;
            return Ok(mvhd_time(&body));
        }
        off = off.saturating_add(len);
    }
    Ok(None)
}

/// `(box length, type, header length)` of the box starting `head`; `left` is what remains of the
/// file (a box of length 0 runs to its end).
fn box_header(head: &[u8], left: u64) -> Option<(u64, [u8; 4], u64)> {
    let size32 = u32::from_be_bytes(head.get(0..4)?.try_into().ok()?) as u64;
    let kind: [u8; 4] = head.get(4..8)?.try_into().ok()?;
    if !kind.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        return None;
    }
    let (len, header) = match size32 {
        0 => (left, 8),
        1 => (u64::from_be_bytes(head.get(8..16)?.try_into().ok()?), 16),
        n => (n, 8),
    };
    (len >= header && len <= left).then_some((len, kind, header))
}

/// The creation time in a `moov` box's content, from its `mvhd` child.
fn mvhd_time(moov: &[u8]) -> Option<i64> {
    let mut off = 0usize;
    for _ in 0..MAX_BOXES {
        let rest = moov.get(off..)?;
        let (len, kind, header) = box_header(rest, rest.len() as u64)?;
        if &kind == b"mvhd" {
            let body = rest.get(header as usize..)?;
            let secs = match body.first()? {
                0 => u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as i64,
                1 => i64::try_from(u64::from_be_bytes(body.get(4..12)?.try_into().ok()?)).ok()?,
                _ => return None,
            };
            // 0 is "not set"; before 1970 is a camera with no clock
            return Some(secs - QT_EPOCH).filter(|t| *t > 0);
        }
        off = off.checked_add(usize::try_from(len).ok()?)?;
    }
    None
}

/// `secs` since 1970 as `YYYY-MM-DDTHH:MM:SS` (no offset: see [`creation_time`]).
pub fn iso(secs: i64) -> String {
    // civil from days (proleptic Gregorian), Howard Hinnant's public-domain algorithm
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    fn mvhd_v0(secs_1904: u32) -> Vec<u8> {
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(&secs_1904.to_be_bytes()); // creation
        body.extend_from_slice(&secs_1904.to_be_bytes()); // modification
        body.extend_from_slice(&[0u8; 88]);
        bx(b"mvhd", &body)
    }

    /// 2019-07-03T14:22:01 in QuickTime seconds.
    const T: i64 = 1_562_163_721;

    fn read_from(file: &[u8]) -> impl FnMut(u64, u64) -> Result<Vec<u8>, String> + '_ {
        |off, len| Ok(file.get(off as usize..).map(|r| r[..r.len().min(len as usize)].to_vec()).unwrap_or_default())
    }

    #[test]
    fn moov_at_the_start_or_after_the_media() {
        let moov = bx(b"moov", &mvhd_v0((T + QT_EPOCH) as u32));
        let front = [bx(b"ftyp", b"isom0000"), moov.clone(), bx(b"mdat", &[7u8; 5000])].concat();
        assert_eq!(creation_time(front.len() as u64, &mut read_from(&front)).unwrap(), Some(T));
        let back = [bx(b"ftyp", b"qt  0000"), bx(b"mdat", &[7u8; 5000]), moov].concat();
        let mut reads = 0;
        let mut counted = |off: u64, len: u64| {
            reads += 1;
            read_from(&back)(off, len)
        };
        assert_eq!(creation_time(back.len() as u64, &mut counted).unwrap(), Some(T));
        assert_eq!(reads, 4, "three box headers and the start of moov");
        assert_eq!(iso(T), "2019-07-03T14:22:01");
        assert_eq!(iso(0), "1970-01-01T00:00:00");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00");
    }

    #[test]
    fn large_boxes_version_1_and_damage() {
        // a 64-bit mdat length, then a version 1 mvhd
        let mut mdat = 1u32.to_be_bytes().to_vec();
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&(16u64 + 100).to_be_bytes());
        mdat.extend_from_slice(&[0u8; 100]);
        let mut v1 = vec![1u8, 0, 0, 0];
        v1.extend_from_slice(&((T + QT_EPOCH) as u64).to_be_bytes());
        v1.extend_from_slice(&[0u8; 100]);
        let file = [bx(b"ftyp", b"isom"), mdat, bx(b"moov", &bx(b"mvhd", &v1))].concat();
        assert_eq!(creation_time(file.len() as u64, &mut read_from(&file)).unwrap(), Some(T));
        // no clock set, cut short, junk, a box claiming more than the file holds
        let unset = bx(b"moov", &mvhd_v0(0));
        assert_eq!(creation_time(unset.len() as u64, &mut read_from(&unset)).unwrap(), None);
        let cut = &file[..30];
        assert_eq!(creation_time(file.len() as u64, &mut read_from(cut)).unwrap(), None);
        let junk = vec![0xffu8; 64];
        assert_eq!(creation_time(64, &mut read_from(&junk)).unwrap(), None);
        let mut liar = bx(b"ftyp", b"isom");
        liar[0..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(creation_time(liar.len() as u64, &mut read_from(&liar)).unwrap(), None);
        let failing = creation_time(100, &mut |_, _| Err::<Vec<u8>, _>("offline"));
        assert_eq!(failing, Err("offline"));
    }
}
