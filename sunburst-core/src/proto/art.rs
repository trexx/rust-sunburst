// SPDX-License-Identifier: GPL-2.0-or-later

//! Box art for the TV's app grid: what identifies an image, how one is cut into
//! control messages, and how an app list is cut into pages.
//!
//! Art travels the **authenticated reliable control channel**, chunked like a
//! cursor shape, not over HTTP. The client already has that channel, keyed with
//! its pairing secret; the web API's bearer token is the operator's, not the
//! TV's, and would need a second client-facing auth scheme to share. Transfers
//! happen from the app grid before any stream starts, so the head-of-line
//! blocking a large reliable transfer would cause mid-session never arises —
//! and the server refuses art requests while a client's control queue is deep.
//!
//! An image is named by a [`digest`](art_digest) of its bytes, so the client
//! caches by content: an unchanged image is never fetched twice, a changed one
//! is fetched once, and a transfer is verified before it is kept.

use super::control::{ServerControl, put_str_len};

/// The largest image the server stores or sends. The web UI downscales to fit.
pub const ART_MAX_BYTES: usize = 512 * 1024;

/// Image bytes per [`ArtChunk`]. With its head this stays inside one reliable
/// frame.
pub const ART_CHUNK_MAX: usize = 1024;

/// The image formats Android decodes and browsers produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArtFormat {
    Png = 0,
    Jpeg = 1,
    Webp = 2,
}

impl ArtFormat {
    pub fn from_u8(v: u8) -> Option<ArtFormat> {
        Some(match v {
            0 => ArtFormat::Png,
            1 => ArtFormat::Jpeg,
            2 => ArtFormat::Webp,
            _ => return None,
        })
    }

    /// The format from the file's own magic bytes — never from a name or a
    /// content-type header, which say what someone claimed.
    pub fn sniff(bytes: &[u8]) -> Option<ArtFormat> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(ArtFormat::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(ArtFormat::Jpeg)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(ArtFormat::Webp)
        } else {
            None
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ArtFormat::Png => "png",
            ArtFormat::Jpeg => "jpg",
            ArtFormat::Webp => "webp",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ArtFormat::Png => "image/png",
            ArtFormat::Jpeg => "image/jpeg",
            ArtFormat::Webp => "image/webp",
        }
    }
}

/// The first 16 bytes of the image's BLAKE3 hash: its name.
pub fn art_digest(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(bytes).as_bytes()[..16]);
    out
}

/// What an [`AppListing`](super::AppListing) says about its art.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtRef {
    pub digest: [u8; 16],
    pub len: u32,
    pub format: ArtFormat,
}

impl ArtRef {
    /// Describe an image. `None` if it is not a format the client decodes, or
    /// larger than [`ART_MAX_BYTES`].
    pub fn of(bytes: &[u8]) -> Option<ArtRef> {
        if bytes.is_empty() || bytes.len() > ART_MAX_BYTES {
            return None;
        }
        Some(ArtRef {
            digest: art_digest(bytes),
            len: bytes.len() as u32,
            format: ArtFormat::sniff(bytes)?,
        })
    }
}

/// One piece of an image. Every chunk repeats the head, and the in-order
/// channel means the receiver appends. `total_len == 0` means the app has no
/// art (or not the art that was asked for any more).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtChunk {
    pub app_id: u32,
    pub digest: [u8; 16],
    pub format: ArtFormat,
    pub total_len: u32,
    pub offset: u32,
    pub data: Vec<u8>,
}

/// An image as the chunks that carry it, or the single "no art" chunk.
pub fn chunk_art(app_id: u32, art: Option<(&ArtRef, &[u8])>) -> Vec<ServerControl> {
    let Some((r, bytes)) = art else {
        return vec![ServerControl::ArtChunk(ArtChunk {
            app_id,
            digest: [0; 16],
            format: ArtFormat::Png,
            total_len: 0,
            offset: 0,
            data: Vec::new(),
        })];
    };
    bytes
        .chunks(ART_CHUNK_MAX)
        .enumerate()
        .map(|(i, data)| {
            ServerControl::ArtChunk(ArtChunk {
                app_id,
                digest: r.digest,
                format: r.format,
                total_len: bytes.len() as u32,
                offset: (i * ART_CHUNK_MAX) as u32,
                data: data.to_vec(),
            })
        })
        .collect()
}

/// One page of the app list: `apps` are entries `start..start+apps.len()` of
/// `total`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPage {
    pub total: u16,
    pub start: u16,
    pub apps: Vec<super::AppListing>,
}

/// Bytes an entry takes on the wire (see `ServerControl::AppList`).
fn listing_len(app: &super::AppListing) -> usize {
    4 + put_str_len(&app.name) + 1 + if app.art.is_some() { 16 + 4 + 1 } else { 0 }
}

/// The envelope (kind + length) and the page head (total, start, count).
const PAGE_OVERHEAD: usize = 3 + 6;

/// The list cut into `AppList` messages, each at most `max_message` bytes
/// encoded. A list that did not fit one reliable frame used to be dropped
/// whole, silently. Always at least one page, so an empty list is still an
/// answer.
pub fn app_list_pages(apps: &[super::AppListing], max_message: usize) -> Vec<ServerControl> {
    let total = u16::try_from(apps.len()).unwrap_or(u16::MAX);
    let apps = &apps[..total as usize];
    let mut pages = Vec::new();
    let mut start = 0usize;
    while start < apps.len() || pages.is_empty() {
        let mut size = PAGE_OVERHEAD;
        let mut end = start;
        while end < apps.len() && size + listing_len(&apps[end]) <= max_message {
            size += listing_len(&apps[end]);
            end += 1;
        }
        // An entry too big for any page on its own cannot be sent; skip it
        // rather than loop forever. (A 255-byte name is ~300 bytes, far under
        // a frame, so this is a guard, not a case.)
        if end == start && start < apps.len() {
            start += 1;
            continue;
        }
        pages.push(ServerControl::AppList(AppPage {
            total,
            start: start as u16,
            apps: apps[start..end].to_vec(),
        }));
        start = end;
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::super::AppListing;
    use super::*;

    fn png(extra: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.resize(8 + extra, 0xAB);
        v
    }

    #[test]
    fn formats_are_sniffed_from_the_bytes() {
        assert_eq!(ArtFormat::sniff(&png(0)), Some(ArtFormat::Png));
        assert_eq!(
            ArtFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ArtFormat::Jpeg)
        );
        assert_eq!(
            ArtFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ArtFormat::Webp)
        );
        assert_eq!(ArtFormat::sniff(b"GIF89a"), None);
        assert_eq!(ArtFormat::sniff(b"RIFF\0\0\0\0WAVE"), None);
        assert_eq!(ArtFormat::sniff(&[]), None);
    }

    #[test]
    fn an_art_ref_names_the_bytes_and_refuses_what_it_cannot_send() {
        let a = ArtRef::of(&png(100)).expect("a png");
        assert_eq!(a.len, 108);
        assert_eq!(a.digest, art_digest(&png(100)));
        assert_ne!(a.digest, art_digest(&png(101)));
        assert!(ArtRef::of(b"not an image").is_none());
        assert!(ArtRef::of(&png(ART_MAX_BYTES)).is_none(), "over the cap");
        assert!(
            ArtRef::of(&png(ART_MAX_BYTES - 8)).is_some(),
            "exactly the cap"
        );
    }

    #[test]
    fn chunks_cover_the_image_in_order_and_fit_a_frame() {
        let bytes = png(3 * ART_CHUNK_MAX);
        let r = ArtRef::of(&bytes).expect("png");
        let chunks = chunk_art(7, Some((&r, &bytes)));
        assert_eq!(chunks.len(), 4);
        let mut joined = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            let ServerControl::ArtChunk(c) = c else {
                panic!("not a chunk");
            };
            assert_eq!(c.offset as usize, i * ART_CHUNK_MAX);
            assert_eq!(c.total_len as usize, bytes.len());
            joined.extend_from_slice(&c.data);
            assert!(
                ServerControl::ArtChunk(c.clone())
                    .encode()
                    .expect("encode")
                    .len()
                    <= 1179
            );
        }
        assert_eq!(joined, bytes);
    }

    #[test]
    fn no_art_is_one_empty_chunk() {
        let chunks = chunk_art(3, None);
        assert_eq!(chunks.len(), 1);
        let ServerControl::ArtChunk(c) = &chunks[0] else {
            panic!("not a chunk");
        };
        assert_eq!((c.app_id, c.total_len), (3, 0));
    }

    fn listing(id: u32, name_len: usize, art: bool) -> AppListing {
        AppListing {
            id,
            name: "n".repeat(name_len),
            art: art.then(|| ArtRef::of(&png(10)).expect("png")),
        }
    }

    #[test]
    fn a_long_list_is_paged_and_every_page_fits() {
        // 60 entries with long names and art: well past one frame.
        let apps: Vec<_> = (0..60).map(|i| listing(i, 200, i % 2 == 0)).collect();
        let pages = app_list_pages(&apps, 1179);
        assert!(pages.len() > 1);
        let mut got = Vec::new();
        for p in &pages {
            assert!(p.encode().expect("encode").len() <= 1179);
            let ServerControl::AppList(page) = p else {
                panic!("not a page");
            };
            assert_eq!(page.total, 60);
            assert_eq!(page.start as usize, got.len(), "pages are contiguous");
            got.extend(page.apps.iter().cloned());
        }
        assert_eq!(got, apps);
    }

    #[test]
    fn an_empty_list_is_still_one_page() {
        let pages = app_list_pages(&[], 1179);
        assert_eq!(
            pages,
            vec![ServerControl::AppList(AppPage {
                total: 0,
                start: 0,
                apps: Vec::new(),
            })]
        );
    }
}
