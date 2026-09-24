// SPDX-License-Identifier: GPL-2.0-or-later

//! When to reconfigure the decoder, and which frames it may see — pure,
//! host-tested.
//!
//! The server re-sends `CodecPrivate` after every encoder rebuild. Most rebuilds
//! (an `AccessLost` on a mode change or fullscreen transition) produce the same
//! parameter sets and colour, and the decoder should carry straight on. An
//! HDR↔SDR flip of the captured desktop, or a new capture size, produces a
//! different stream, and the decoder has to be reconfigured for it.
//!
//! A reconfigured decoder has no references, so it must start on a keyframe of
//! the new build. The build's IDR travels the video path and usually arrives
//! *before* its `CodecPrivate` (which goes GPU thread → channel → endpoint tick),
//! so by the time the client knows to reconfigure, that IDR may already have gone
//! to the old configuration. `CodecPrivate.first_frame` is what separates the
//! builds: the [`KeyframeGate`] drops frames older than it, holds back
//! everything until a keyframe at or after it, and asks for another IDR when the
//! one it needed was already spent.
//!
//! The gate also covers the start of a session. The server fires its startup IDR
//! the moment the pipeline spawns, which is before the client has finished
//! negotiating, so that IDR is gone. A gate starts closed and asks at once.

use sunburst_core::proto::{CodecPrivate, ColorInfo, Seq16, cicp};

/// How a newly arrived `CodecPrivate` relates to the one the decoder runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The same stream: keep decoding. (A rebuild that changed nothing the
    /// decoder sees — its forced IDR simply passes through.)
    Same,
    /// A different stream: reconfigure, then wait for a keyframe of it.
    Reconfigure,
}

/// Compare everything a decoder is configured from. `first_frame` is not part
/// of the configuration, so a rebuild that changes only it is [`Change::Same`].
pub fn classify(current: &CodecPrivate, new: &CodecPrivate) -> Change {
    let same = current.codec == new.codec
        && current.data == new.data
        && current.width == new.width
        && current.height == new.height
        && current.color == new.color
        && current.hdr == new.hdr;
    if same {
        Change::Same
    } else {
        Change::Reconfigure
    }
}

/// `MediaFormat` colour keys `(color-standard, color-transfer, color-range)` for
/// a build, or `None` for a CICP combination with no MediaFormat equivalent
/// (leave the keys unset and let the decoder read the VUI).
pub fn media_color_keys(c: &ColorInfo) -> Option<(i32, i32, i32)> {
    // MediaFormat.COLOR_STANDARD_*, COLOR_TRANSFER_*, COLOR_RANGE_*.
    const STANDARD_BT709: i32 = 1;
    const STANDARD_BT2020: i32 = 6;
    const TRANSFER_SDR_VIDEO: i32 = 3;
    const TRANSFER_ST2084: i32 = 6;
    const RANGE_FULL: i32 = 1;
    const RANGE_LIMITED: i32 = 2;

    let standard = match (c.primaries, c.matrix) {
        (cicp::PRIMARIES_BT709, cicp::MATRIX_BT709) => STANDARD_BT709,
        (cicp::PRIMARIES_BT2020, cicp::MATRIX_BT2020_NCL) => STANDARD_BT2020,
        _ => return None,
    };
    let transfer = match c.transfer {
        cicp::TRANSFER_BT709 => TRANSFER_SDR_VIDEO,
        cicp::TRANSFER_PQ => TRANSFER_ST2084,
        _ => return None,
    };
    let range = if c.full_range {
        RANGE_FULL
    } else {
        RANGE_LIMITED
    };
    Some((standard, transfer, range))
}

/// How long to wait for a keyframe before asking (again). Long enough for the
/// build's own IDR to arrive if it is still in flight; short enough that a lost
/// one is a blip rather than a stall.
pub const IDR_RETRY_MS: u64 = 500;

/// Which frames may reach the decoder. See the module docs.
#[derive(Debug)]
pub struct KeyframeGate {
    /// The first frame of the current build.
    first: Seq16,
    /// A keyframe of the current build has been admitted.
    open: bool,
    /// When a closed gate next asks for an IDR.
    ask_at_ms: u64,
}

impl KeyframeGate {
    /// A gate for the session's first build. Closed, and asking at once: the
    /// startup IDR went out during negotiation.
    pub fn new(first: Seq16, now_ms: u64) -> KeyframeGate {
        KeyframeGate {
            first,
            open: false,
            ask_at_ms: now_ms,
        }
    }

    /// A new build from `first` on, after the decoder was reconfigured for it.
    /// `last_fed` is the newest frame the old configuration consumed: if that is
    /// already `first` or later, the build's IDR was spent there, so ask now;
    /// otherwise it is still coming, so give it [`IDR_RETRY_MS`].
    pub fn close(&mut self, first: Seq16, last_fed: Option<Seq16>, now_ms: u64) {
        self.first = first;
        self.open = false;
        let spent = last_fed.is_some_and(|l| !first.is_newer_than(l));
        self.ask_at_ms = if spent { now_ms } else { now_ms + IDR_RETRY_MS };
    }

    /// Whether a released frame may be fed to the decoder.
    pub fn admit(&mut self, frame_id: Seq16, keyframe: bool) -> bool {
        if self.first.is_newer_than(frame_id) {
            // The previous build's: the decoder is no longer configured for it.
            return false;
        }
        if !self.open && keyframe {
            self.open = true;
        }
        self.open
    }

    /// Whether to send `RequestIdr` now. While closed, every [`IDR_RETRY_MS`].
    pub fn tick(&mut self, now_ms: u64) -> bool {
        if self.open || now_ms < self.ask_at_ms {
            return false;
        }
        self.ask_at_ms = now_ms + IDR_RETRY_MS;
        true
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

#[cfg(test)]
mod tests {
    use sunburst_core::proto::{HdrMastering, StreamCodec};

    use super::*;

    fn build(first: u16) -> CodecPrivate {
        CodecPrivate {
            codec: StreamCodec::Hevc,
            data: vec![0, 0, 0, 1, 0x40],
            width: 3840,
            height: 2160,
            first_frame: Seq16(first),
            color: ColorInfo::SDR_709_10,
            hdr: None,
        }
    }

    #[test]
    fn a_rebuild_that_changes_nothing_the_decoder_sees_is_the_same() {
        assert_eq!(classify(&build(10), &build(500)), Change::Same);
    }

    #[test]
    fn anything_the_decoder_is_configured_from_reconfigures() {
        let base = build(10);
        let hdr = CodecPrivate {
            color: ColorInfo::HDR10,
            hdr: Some(HdrMastering::default()),
            ..build(11)
        };
        let resized = CodecPrivate {
            width: 2560,
            height: 1440,
            ..build(11)
        };
        let headers = CodecPrivate {
            data: vec![0, 0, 0, 1, 0x42],
            ..build(11)
        };
        let mastering = CodecPrivate {
            hdr: Some(HdrMastering {
                max_cll: 600,
                ..HdrMastering::default()
            }),
            ..hdr.clone()
        };
        for (what, new) in [("colour", &hdr), ("size", &resized), ("headers", &headers)] {
            assert_eq!(classify(&base, new), Change::Reconfigure, "{what}");
        }
        assert_eq!(classify(&hdr, &mastering), Change::Reconfigure, "mastering");
    }

    #[test]
    fn colour_keys_follow_the_build() {
        assert_eq!(media_color_keys(&ColorInfo::SDR_709_8), Some((1, 3, 2)));
        assert_eq!(media_color_keys(&ColorInfo::SDR_709_10), Some((1, 3, 2)));
        assert_eq!(media_color_keys(&ColorInfo::HDR10), Some((6, 6, 2)));
        let full = ColorInfo {
            full_range: true,
            ..ColorInfo::SDR_709_8
        };
        assert_eq!(media_color_keys(&full), Some((1, 3, 1)));
        let odd = ColorInfo {
            transfer: 18, // HLG: not something this server sends
            ..ColorInfo::HDR10
        };
        assert_eq!(media_color_keys(&odd), None);
    }

    #[test]
    fn a_session_starts_closed_and_asks_at_once() {
        let mut gate = KeyframeGate::new(Seq16(7), 1_000);
        assert!(gate.tick(1_000), "the startup IDR is gone; ask immediately");
        assert!(!gate.tick(1_100), "and not again straight away");
        assert!(!gate.admit(Seq16(8), false), "no P-frame before a keyframe");
        assert!(gate.tick(1_500), "still closed after the retry interval");
        assert!(gate.admit(Seq16(9), true));
        assert!(gate.admit(Seq16(10), false), "open: everything after flows");
        assert!(!gate.tick(5_000), "an open gate never asks");
    }

    #[test]
    fn frames_of_the_previous_build_are_dropped() {
        let mut gate = KeyframeGate::new(Seq16(100), 0);
        assert!(!gate.admit(Seq16(99), true), "a keyframe of the old build");
        assert!(gate.admit(Seq16(100), true));
        assert!(
            !gate.admit(Seq16(98), false),
            "late, and still the old build's"
        );
    }

    #[test]
    fn an_idr_still_in_flight_is_waited_for() {
        // CodecPrivate outran the IDR: the old configuration never saw frame 50.
        let mut gate = KeyframeGate::new(Seq16(0), 0);
        gate.admit(Seq16(0), true);
        gate.close(Seq16(50), Some(Seq16(49)), 10_000);
        assert!(!gate.tick(10_000), "the build's own IDR is on its way");
        assert!(gate.admit(Seq16(50), true), "and it opens the gate");
        assert!(!gate.tick(20_000));
    }

    #[test]
    fn an_idr_already_spent_on_the_old_configuration_is_asked_for_again() {
        // The usual order: the IDR (frame 50) reached the decoder before the
        // CodecPrivate that says it belongs to a different stream.
        let mut gate = KeyframeGate::new(Seq16(0), 0);
        gate.admit(Seq16(0), true);
        gate.close(Seq16(50), Some(Seq16(52)), 10_000);
        assert!(gate.tick(10_000), "ask now");
        assert!(
            !gate.admit(Seq16(53), false),
            "P-frames need references it lacks"
        );
        assert!(gate.admit(Seq16(60), true), "the requested IDR");
    }

    #[test]
    fn the_gate_works_across_the_frame_id_wrap() {
        let mut gate = KeyframeGate::new(Seq16(65_530), 0);
        assert!(!gate.admit(Seq16(65_529), true));
        assert!(gate.admit(Seq16(65_535), true));
        assert!(
            gate.admit(Seq16(2), false),
            "past the wrap is newer, not older"
        );
        gate.close(Seq16(5), Some(Seq16(3)), 0);
        assert!(!gate.admit(Seq16(65_535), true), "before the wrap is older");
        assert!(gate.admit(Seq16(5), true));
    }
}
