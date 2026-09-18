// SPDX-License-Identifier: GPL-2.0-or-later

//! CLAUDE.md's first hot-path rule, for the receive half: after warmup the
//! reassembler, the NACK bookkeeping, the jitter buffer and the copy into the
//! decoder's buffer allocate nothing — under loss, with retransmits, so the
//! recovery paths are measured too, not just the happy one.
//!
//! Its own test binary because the `#[global_allocator]` below applies to the
//! whole process. Same pattern as `sunburst-core/tests/zero_alloc.rs`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use sunburst_core::proto::{HEADER_LEN, MAX_PAYLOAD, Seq16};
use sunburst_net::{Accept, JitterBuffer, Packetizer, Reassembler};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged, with the same contract
// and the same pointers. The only addition is a relaxed counter increment, which
// cannot affect allocation behaviour.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `layout` is forwarded unchanged from our caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` came from our `alloc` with this `layout`, forwarded
        // unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: every pointer we hand back came from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// A 4K P-frame's worth of bitstream: about 250 packets.
const FRAME_BYTES: usize = 300_000;
const MAX_PKTS: usize = 512;
const FRAME_NS: u64 = 16_666_667;

struct Bench {
    packetizer: Packetizer,
    reassembler: Reassembler,
    jitter: JitterBuffer,
    unit: Vec<u8>,
    out: Vec<u8>,
    /// Every packet of the current frame, so "loss" is just not delivering one
    /// and a "retransmit" is delivering it later.
    pkts: Vec<[u8; HEADER_LEN + MAX_PAYLOAD]>,
    lens: Vec<usize>,
    missing: [u16; 64],
    abandoned: [Seq16; 16],
    now_ns: u64,
    delivered: u32,
}

impl Bench {
    fn new() -> Bench {
        Bench {
            packetizer: Packetizer::new(),
            reassembler: Reassembler::new(),
            jitter: JitterBuffer::new(),
            unit: vec![0x5A; FRAME_BYTES],
            out: vec![0; FRAME_BYTES + MAX_PAYLOAD],
            pkts: vec![[0u8; HEADER_LEN + MAX_PAYLOAD]; MAX_PKTS],
            lens: vec![0; MAX_PKTS],
            missing: [0; 64],
            abandoned: [Seq16(0); 16],
            now_ns: 0,
            delivered: 0,
        }
    }

    /// One frame through the whole receive path with 2 % loss and a NACK
    /// round: lose every 50th packet, ask for the holes, deliver them.
    fn frame(&mut self, id: u16) {
        let mut n = 0;
        let (pkts, lens) = (&mut self.pkts, &mut self.lens);
        let mut store = |pkt: &[u8]| {
            pkts[n][..pkt.len()].copy_from_slice(pkt);
            lens[n] = pkt.len();
            n += 1;
        };
        self.packetizer.begin_frame(Seq16(id), id as u32, id == 0);
        self.packetizer.push_unit(&self.unit, &mut store);
        self.packetizer.finish_frame(&mut store);

        let mut completed = None;
        for i in 0..n {
            if i % 50 == 7 {
                continue; // lost on the wire
            }
            if let Accept::Complete(f) = self.reassembler.push(&self.pkts[i][..self.lens[i]]) {
                completed = Some(f);
            }
        }
        assert!(completed.is_none(), "frame {id} completed despite loss");

        let holes = self.reassembler.missing(Seq16(id), &mut self.missing);
        assert!(holes > 0, "frame {id}: loss left no holes to NACK");
        for k in 0..holes {
            let i = self.missing[k] as usize;
            if let Accept::Complete(f) = self.reassembler.push(&self.pkts[i][..self.lens[i]]) {
                completed = Some(f);
            }
        }
        let frame = completed.expect("frame completes once the retransmits land");

        self.now_ns += FRAME_NS;
        assert!(self.jitter.push(frame, self.now_ns).is_ok());
        while let Some(rel) = self.jitter.pop(self.now_ns + 100_000_000) {
            let len = self
                .reassembler
                .copy_into(&rel.frame, &mut self.out)
                .expect("live handle");
            assert_eq!(len, FRAME_BYTES);
            assert_eq!(rel.stepped_over, None);
            self.reassembler.release(rel.frame);
            self.delivered += 1;
        }
        assert_eq!(self.reassembler.drain_abandoned(&mut self.abandoned), 0);
    }
}

#[test]
fn the_receive_path_allocates_nothing_after_warmup() {
    let mut b = Bench::new();

    // Warmup: the pool, the frame slots and the packetizer's scratch are
    // construction-time costs by design.
    for id in 0..10u16 {
        b.frame(id);
    }

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for id in 10..610u16 {
        b.frame(id);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(
        allocations, 0,
        "the receive path allocated {allocations} times after warmup"
    );

    // Prove the run was real: every frame came out the far end intact.
    assert_eq!(b.delivered, 610);
    assert_eq!(b.out[..FRAME_BYTES], vec![0x5A; FRAME_BYTES][..]);
}
