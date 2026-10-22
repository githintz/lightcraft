//! Files laid out as cameras and converters write them, for the scan tests.

#![allow(dead_code)]

/// A TIFF entry: tag, type, count, value bytes.
pub struct E(u16, u16, u32, Vec<u8>);

pub fn ascii(tag: u16, s: &str) -> E {
    let mut d = s.as_bytes().to_vec();
    d.push(0);
    E(tag, 2, d.len() as u32, d)
}

/// A little-endian TIFF laid out like a camera raw: the header, IFD0 at offset 8, the Exif IFD
/// right after, their values, `image` bytes of image data, then a 600 KB block (a preview or
/// maker data) that a 256 KB read never reaches.
pub fn camera_raw(make: &str, model: &str, serial: &str, date: &str, subsec: &str, image: usize) -> Vec<u8> {
    let ifd0 = [ascii(271, make), ascii(272, model), E(34665, 4, 1, vec![]), E(34675, 7, 600_000, vec![9u8; 600_000])];
    let exif = [ascii(36867, date), ascii(37521, subsec), ascii(42033, serial)];
    let ifd0_at = 8u32;
    let exif_at = ifd0_at + 2 + 12 * ifd0.len() as u32 + 4;
    let mut values_at = exif_at + 2 + 12 * exif.len() as u32 + 4;
    let small: u32 = ifd0.iter().chain(exif.iter()).filter(|e| e.3.len() > 4 && e.0 != 34675).map(|e| e.3.len() as u32).sum();
    let big_at = values_at + small + image as u32;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&ifd0_at.to_le_bytes());
    let mut values = Vec::new();
    let mut table = |entries: &[E], out: &mut Vec<u8>, values_at: &mut u32| {
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for e in entries {
            out.extend_from_slice(&e.0.to_le_bytes());
            out.extend_from_slice(&e.1.to_le_bytes());
            out.extend_from_slice(&e.2.to_le_bytes());
            if e.0 == 34665 {
                out.extend_from_slice(&exif_at.to_le_bytes());
            } else if e.0 == 34675 {
                out.extend_from_slice(&big_at.to_le_bytes());
            } else if e.3.len() <= 4 {
                let mut v = e.3.clone();
                v.resize(4, 0);
                out.extend_from_slice(&v);
            } else {
                out.extend_from_slice(&values_at.to_le_bytes());
                *values_at += e.3.len() as u32;
                values.extend_from_slice(&e.3);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes());
    };
    table(&ifd0, &mut out, &mut values_at);
    table(&exif, &mut out, &mut values_at);
    out.extend_from_slice(&values);
    out.extend((0..image).map(|i| (i * 7 % 251) as u8));
    assert_eq!(out.len() as u32, big_at);
    out.extend_from_slice(&[9u8; 600_000]);
    out
}

/// A TIFF whose tag directory comes after its data (as some converters write DNGs).
pub fn directory_last_dng(date: &str) -> Vec<u8> {
    use lightcraft_tiff::{ByteOrder, IfdBuilder, TiffWriter, Value, tags as t};
    let mut ifd0 = IfdBuilder::new();
    ifd0.set(t::MAKE, Value::Ascii("NIKON CORPORATION".into()));
    ifd0.set(t::ICC_PROFILE, Value::Undefined(vec![7u8; 600_000]));
    let mut ex = IfdBuilder::new();
    ex.set(t::DATE_TIME_ORIGINAL, Value::Ascii(date.into()));
    ifd0.set_child(t::EXIF_IFD, ex);
    TiffWriter::new(ByteOrder::Little, false).write(&[ifd0]).unwrap()
}

/// A camera JPEG: its Exif in APP1, then a scan of `image` bytes.
pub fn camera_jpeg(serial: &str, date: &str, subsec: &str, image: usize) -> Vec<u8> {
    let tiff = camera_raw("NIKON CORPORATION", "NIKON Z 6", serial, date, subsec, 0);
    // the Exif block is the TIFF without the 600 KB block (an APP1 holds at most 64 KB)
    let ifd_end = tiff.len() - 600_000;
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(&tiff[..ifd_end]);
    let mut out = vec![0xff, 0xd8, 0xff, 0xe1];
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 0x3f, 0]);
    out.extend((0..image).map(|i| (i * 13 % 251) as u8));
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

/// An MP4 with its movie header after the media, made at `secs_1904`.
pub fn camera_mp4(secs_1904: u32, media: usize) -> Vec<u8> {
    let bx = |kind: &[u8; 4], body: &[u8]| {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    };
    let mut mvhd = vec![0u8; 4];
    mvhd.extend_from_slice(&secs_1904.to_be_bytes());
    mvhd.extend_from_slice(&secs_1904.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 88]);
    [bx(b"ftyp", b"isom0000"), bx(b"mdat", &vec![5u8; media]), bx(b"moov", &bx(b"mvhd", &mvhd))].concat()
}
