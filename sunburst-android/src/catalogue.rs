// SPDX-License-Identifier: GPL-2.0-or-later

//! The TV's app grid, fetched over the control channel — pure, host-tested.
//!
//! `AppList` arrives in pages ([`AppPages`] collects them), and each app's box
//! art arrives as in-order chunks ([`ArtAssembler`] joins and verifies them).
//! Art is cached on disk by the digest of its bytes ([`cache_name`]), so an
//! image is fetched once, a changed image is fetched once more, and anything no
//! longer referenced is evicted ([`stale`]).

use std::collections::HashSet;

use sunburst_core::proto::{ART_MAX_BYTES, AppListing, AppPage, ArtChunk, ArtFormat, art_digest};

/// Collects `AppList` pages into the whole list.
#[derive(Debug, Default)]
pub struct AppPages {
    apps: Vec<AppListing>,
    total: Option<usize>,
}

impl AppPages {
    pub fn new() -> AppPages {
        AppPages::default()
    }

    /// Take a page. A page that does not continue the list (a duplicate, or one
    /// from an earlier request) is ignored.
    pub fn push(&mut self, page: AppPage) {
        if page.start as usize != self.apps.len() {
            return;
        }
        self.total = Some(page.total as usize);
        self.apps.extend(page.apps);
    }

    /// The whole list, once every page has arrived.
    pub fn complete(&self) -> Option<&[AppListing]> {
        (self.total? <= self.apps.len()).then_some(&self.apps[..])
    }
}

/// Why a transfer was not kept.
#[derive(Debug, PartialEq, Eq)]
pub enum ArtError {
    /// Bigger than the server may send; refused rather than buffered.
    TooLarge,
    /// A chunk that does not continue the image.
    OutOfOrder,
    /// The bytes do not hash to the digest they arrived under.
    Corrupt,
}

/// Joins one app's art chunks, and verifies the result.
#[derive(Debug)]
pub struct ArtAssembler {
    app_id: u32,
    bytes: Vec<u8>,
    digest: [u8; 16],
    format: ArtFormat,
    total: usize,
}

/// What a chunk did.
#[derive(Debug, PartialEq, Eq)]
pub enum ArtProgress {
    /// More to come.
    Partial,
    /// The app has no art.
    None,
    /// The whole image, verified: its digest, format and bytes.
    Done([u8; 16], ArtFormat, Vec<u8>),
}

impl ArtAssembler {
    pub fn new(app_id: u32) -> ArtAssembler {
        ArtAssembler {
            app_id,
            bytes: Vec::new(),
            digest: [0; 16],
            format: ArtFormat::Png,
            total: 0,
        }
    }

    /// Take a chunk for this app (chunks for other apps are not this
    /// assembler's business; the caller filters by `app_id`).
    pub fn push(&mut self, c: ArtChunk) -> Result<ArtProgress, ArtError> {
        debug_assert_eq!(c.app_id, self.app_id);
        if c.total_len == 0 {
            return Ok(ArtProgress::None);
        }
        let total = c.total_len as usize;
        if total > ART_MAX_BYTES {
            return Err(ArtError::TooLarge);
        }
        if c.offset == 0 {
            // A fresh image: the first chunk says what it is.
            self.bytes = Vec::with_capacity(total);
            self.digest = c.digest;
            self.format = c.format;
            self.total = total;
        } else if c.offset as usize != self.bytes.len()
            || c.digest != self.digest
            || total != self.total
        {
            return Err(ArtError::OutOfOrder);
        }
        if self.bytes.len() + c.data.len() > self.total {
            return Err(ArtError::OutOfOrder);
        }
        self.bytes.extend_from_slice(&c.data);
        if self.bytes.len() < self.total {
            return Ok(ArtProgress::Partial);
        }
        if art_digest(&self.bytes) != self.digest {
            return Err(ArtError::Corrupt);
        }
        Ok(ArtProgress::Done(
            self.digest,
            self.format,
            std::mem::take(&mut self.bytes),
        ))
    }
}

/// The cache file an image is stored as: its digest, hex, and its extension.
pub fn cache_name(digest: &[u8; 16], format: ArtFormat) -> String {
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.{}", format.extension())
}

/// Whether a file name is one [`cache_name`] makes, so eviction never touches
/// anything else in the directory.
fn is_cache_name(name: &str) -> bool {
    let Some((hex, ext)) = name.split_once('.') else {
        return false;
    };
    hex.len() == 32
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
        && ["png", "jpg", "webp"].contains(&ext)
}

/// The cached files to delete: ours, and no longer named by any listing.
pub fn stale<'a>(existing: &'a [String], listed: &[AppListing]) -> Vec<&'a str> {
    let keep: HashSet<String> = listed
        .iter()
        .filter_map(|a| a.art.map(|r| cache_name(&r.digest, r.format)))
        .collect();
    existing
        .iter()
        .map(String::as_str)
        .filter(|n| is_cache_name(n) && !keep.contains(*n))
        .collect()
}

#[cfg(test)]
mod tests {
    use sunburst_core::proto::{ArtRef, ServerControl, chunk_art};

    use super::*;

    fn listing(id: u32, art: Option<ArtRef>) -> AppListing {
        AppListing {
            id,
            name: format!("app {id}"),
            art,
        }
    }

    fn png(extra: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend((0..extra).map(|i| i as u8));
        v
    }

    fn chunks(app_id: u32, bytes: &[u8]) -> Vec<ArtChunk> {
        let r = ArtRef::of(bytes).expect("png");
        chunk_art(app_id, Some((&r, bytes)))
            .into_iter()
            .map(|c| match c {
                ServerControl::ArtChunk(c) => c,
                _ => panic!("not a chunk"),
            })
            .collect()
    }

    #[test]
    fn pages_collect_into_the_whole_list() {
        let mut pages = AppPages::new();
        assert!(pages.complete().is_none());
        let first = AppPage {
            total: 3,
            start: 0,
            apps: vec![listing(0, None), listing(1, None)],
        };
        pages.push(first.clone());
        assert!(pages.complete().is_none(), "one page still to come");
        pages.push(first); // a duplicate is ignored
        pages.push(AppPage {
            total: 3,
            start: 2,
            apps: vec![listing(2, None)],
        });
        let ids: Vec<u32> = pages
            .complete()
            .expect("complete")
            .iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, [0, 1, 2]);
    }

    #[test]
    fn an_empty_list_is_complete_at_once() {
        let mut pages = AppPages::new();
        pages.push(AppPage {
            total: 0,
            start: 0,
            apps: Vec::new(),
        });
        assert_eq!(pages.complete().map(<[_]>::len), Some(0));
    }

    #[test]
    fn art_assembles_and_is_verified() {
        let image = png(5_000);
        let mut a = ArtAssembler::new(4);
        let parts = chunks(4, &image);
        let last = parts.len() - 1;
        for (i, c) in parts.into_iter().enumerate() {
            match a.push(c).expect("in order") {
                ArtProgress::Partial => assert!(i < last),
                ArtProgress::Done(digest, format, bytes) => {
                    assert_eq!(i, last);
                    assert_eq!((digest, format), (art_digest(&image), ArtFormat::Png));
                    assert_eq!(bytes, image);
                }
                ArtProgress::None => panic!("it has art"),
            }
        }
    }

    #[test]
    fn a_corrupted_transfer_is_not_kept() {
        let image = png(3_000);
        let mut parts = chunks(4, &image);
        parts[1].data[0] ^= 0xFF;
        let mut a = ArtAssembler::new(4);
        let mut result = Ok(ArtProgress::Partial);
        for c in parts {
            result = a.push(c);
        }
        assert_eq!(result, Err(ArtError::Corrupt));
    }

    #[test]
    fn a_gap_or_an_oversized_claim_is_refused() {
        let image = png(3_000);
        let parts = chunks(4, &image);
        let mut a = ArtAssembler::new(4);
        a.push(parts[0].clone()).expect("first");
        assert_eq!(a.push(parts[2].clone()), Err(ArtError::OutOfOrder));

        let mut huge = parts[0].clone();
        huge.total_len = (ART_MAX_BYTES + 1) as u32;
        assert_eq!(ArtAssembler::new(4).push(huge), Err(ArtError::TooLarge));
    }

    #[test]
    fn no_art_is_reported_as_none() {
        let ServerControl::ArtChunk(empty) = chunk_art(4, None).remove(0) else {
            panic!("not a chunk");
        };
        assert_eq!(ArtAssembler::new(4).push(empty), Ok(ArtProgress::None));
    }

    #[test]
    fn the_cache_is_named_by_content_and_evicts_only_its_own_strays() {
        let image = png(10);
        let r = ArtRef::of(&image).expect("png");
        let name = cache_name(&r.digest, r.format);
        assert!(name.ends_with(".png") && name.len() == 36);

        let old = cache_name(&[0xAB; 16], ArtFormat::Webp);
        let existing = vec![
            name.clone(),
            old.clone(),
            "settings.json".to_string(),
            "tmp-1234.part".to_string(),
        ];
        let listed = [listing(1, Some(r)), listing(2, None)];
        assert_eq!(stale(&existing, &listed), vec![old.as_str()]);
    }
}
