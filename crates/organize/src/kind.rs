//! What kind of file a name is, by its extension.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Jpeg,
    /// Nikon NEF / NRW.
    Nef,
    Dng,
    /// Another camera raw format.
    Raw,
    /// TIFF, PNG, HEIC and other still images.
    Image,
    /// MP4 / MOV / M4V (also ProRes RAW, which is a MOV).
    Video,
    /// Raw video that isn't MP4 / MOV (Nikon N-RAW `.nev`, RED, Blackmagic RAW…).
    RawVideo,
    /// An XMP sidecar.
    Sidecar,
}

impl Kind {
    /// The kind of a file name, `None` for files that are neither photos nor videos.
    pub fn of(name: &str) -> Option<Kind> {
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "jpg" | "jpeg" | "jpe" => Kind::Jpeg,
            "nef" | "nrw" => Kind::Nef,
            "dng" => Kind::Dng,
            "cr2" | "cr3" | "crw" | "arw" | "srf" | "sr2" | "raf" | "orf" | "rw2" | "pef" | "srw" | "x3f" | "3fr" | "iiq" | "erf" | "mrw" | "kdc" => {
                Kind::Raw
            }
            "tif" | "tiff" | "png" | "heic" | "heif" | "hif" | "avif" | "webp" | "jxl" => Kind::Image,
            "mp4" | "mov" | "m4v" | "3gp" => Kind::Video,
            "nev" | "r3d" | "braw" | "crm" | "ari" | "mxf" => Kind::RawVideo,
            "xmp" => Kind::Sidecar,
            _ => return None,
        })
    }

    /// A still photo (metadata read from the start of the file).
    pub fn is_photo(self) -> bool {
        matches!(self, Kind::Jpeg | Kind::Nef | Kind::Dng | Kind::Raw | Kind::Image)
    }

    /// A camera raw (paired with a JPEG of the same shot, not a duplicate of it).
    pub fn is_raw(self) -> bool {
        matches!(self, Kind::Nef | Kind::Dng | Kind::Raw)
    }

    pub fn is_video(self) -> bool {
        matches!(self, Kind::Video | Kind::RawVideo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_extension() {
        assert_eq!(Kind::of("DSC_0001.NEF"), Some(Kind::Nef));
        assert_eq!(Kind::of("a.b.JPG"), Some(Kind::Jpeg));
        assert_eq!(Kind::of("clip.MOV"), Some(Kind::Video));
        assert_eq!(Kind::of("C0001.NEV"), Some(Kind::RawVideo));
        assert_eq!(Kind::of("x.dng"), Some(Kind::Dng));
        assert_eq!(Kind::of("DSC_0001.NEF.xmp"), Some(Kind::Sidecar));
        assert_eq!(Kind::of("notes.txt"), None);
        assert_eq!(Kind::of("README"), None);
        assert!(Kind::Nef.is_raw() && Kind::Nef.is_photo() && !Kind::Jpeg.is_raw() && Kind::RawVideo.is_video());
    }
}
