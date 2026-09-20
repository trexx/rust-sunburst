// SPDX-License-Identifier: GPL-2.0-or-later

//! A single-producer/single-consumer ring of interleaved i16 PCM.
//!
//! The client thread decodes Opus and pushes samples; AAudio's real-time data
//! callback pops them. Lock-free so the callback never blocks: the producer only
//! advances the write cursor, the consumer only the read cursor, and the buffer
//! is a power-of-two so the wrapping cursors index correctly even across a
//! `usize` overflow (which matters on the 32-bit `armeabi-v7a` client).
//!
//! Bounded by design: a full ring drops the newest samples on push, and an empty
//! ring returns fewer than requested on pop (the callback zero-fills the rest as
//! silence). Both bound the audio buffer, so A/V offset cannot grow without
//! limit — see the sync notes in `audio.rs`. This module is pure and host-tested.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Inner {
    buf: UnsafeCell<Box<[i16]>>,
    mask: usize,
    read: AtomicUsize,
    write: AtomicUsize,
}

// SAFETY: the producer only writes `write` and the slots ahead of it; the
// consumer only writes `read` and reads the slots behind `write`. The two never
// touch the same slot or cursor, so the shared `UnsafeCell` is sound for one
// producer and one consumer — which the split into `PcmProducer`/`PcmConsumer`
// enforces (neither is `Clone`).
unsafe impl Send for Inner {}
// SAFETY: as for `Send` above — disjoint cursors and slots make concurrent
// access by the single producer and single consumer sound.
unsafe impl Sync for Inner {}

/// The producer half — held by the client (decoder) thread.
pub struct PcmProducer {
    inner: Arc<Inner>,
}

/// The consumer half — moved into the AAudio data callback.
pub struct PcmConsumer {
    inner: Arc<Inner>,
}

/// Create a ring holding `capacity` interleaved samples (rounded up to a power
/// of two, minimum 2), split into a producer and a consumer.
pub fn pcm_ring(capacity: usize) -> (PcmProducer, PcmConsumer) {
    let cap = capacity.max(2).next_power_of_two();
    let inner = Arc::new(Inner {
        buf: UnsafeCell::new(vec![0i16; cap].into_boxed_slice()),
        mask: cap - 1,
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    (
        PcmProducer {
            inner: Arc::clone(&inner),
        },
        PcmConsumer { inner },
    )
}

impl PcmProducer {
    /// Push as many of `src`'s samples as fit, returning the count written. The
    /// remainder is dropped (a full ring means the consumer has fallen behind).
    pub fn push(&self, src: &[i16]) -> usize {
        let inner = &*self.inner;
        let cap = inner.mask + 1;
        let w = inner.write.load(Ordering::Relaxed);
        let r = inner.read.load(Ordering::Acquire);
        let free = cap - w.wrapping_sub(r);
        let n = free.min(src.len());
        // SAFETY: this thread is the sole writer; it only writes the `n` slots
        // starting at `w`, which are free (not yet read), and publishes them
        // with the Release store below.
        let buf = unsafe { &mut *inner.buf.get() };
        for (i, &s) in src.iter().take(n).enumerate() {
            buf[w.wrapping_add(i) & inner.mask] = s;
        }
        inner.write.store(w.wrapping_add(n), Ordering::Release);
        n
    }

    /// Samples currently buffered (written but not yet consumed).
    pub fn available(&self) -> usize {
        let inner = &*self.inner;
        inner
            .write
            .load(Ordering::Relaxed)
            .wrapping_sub(inner.read.load(Ordering::Acquire))
    }
}

impl PcmConsumer {
    /// Pop up to `dst.len()` samples into `dst`, returning the count read. The
    /// caller zero-fills `dst[count..]` (an underrun plays as silence).
    pub fn pop(&mut self, dst: &mut [i16]) -> usize {
        let inner = &*self.inner;
        let r = inner.read.load(Ordering::Relaxed);
        let w = inner.write.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r);
        let n = avail.min(dst.len());
        // SAFETY: this thread is the sole reader; it only reads the `n` slots
        // starting at `r`, which the producer has published (Acquire above).
        let buf = unsafe { &*inner.buf.get() };
        for (i, slot) in dst.iter_mut().take(n).enumerate() {
            *slot = buf[r.wrapping_add(i) & inner.mask];
        }
        inner.read.store(r.wrapping_add(n), Ordering::Release);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_rounds_up_to_a_power_of_two() {
        let (p, _c) = pcm_ring(100);
        // 100 -> 128; one slot is always usable, so free starts at 128.
        assert_eq!(p.push(&[7; 200]), 128);
    }

    #[test]
    fn push_then_pop_preserves_order() {
        let (p, mut c) = pcm_ring(16);
        assert_eq!(p.push(&[1, 2, 3, 4]), 4);
        assert_eq!(p.available(), 4);
        let mut out = [0i16; 4];
        assert_eq!(c.pop(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);
        assert_eq!(p.available(), 0);
    }

    #[test]
    fn a_full_ring_drops_the_overflow() {
        let (p, _c) = pcm_ring(4); // holds 4
        assert_eq!(p.push(&[1, 2, 3, 4, 5, 6]), 4, "only 4 fit");
        assert_eq!(p.available(), 4);
    }

    #[test]
    fn an_empty_ring_returns_a_short_read() {
        let (p, mut c) = pcm_ring(8);
        p.push(&[9, 8]);
        let mut out = [0i16; 4];
        assert_eq!(c.pop(&mut out), 2);
        assert_eq!(&out[..2], &[9, 8]);
    }

    #[test]
    fn wraps_around_repeatedly() {
        let (p, mut c) = pcm_ring(4);
        let mut out = [0i16; 3];
        // Many small push/pop cycles force the cursors past the buffer end.
        for k in 0..1000i16 {
            assert_eq!(p.push(&[k, k + 1, k + 2]), 3);
            assert_eq!(c.pop(&mut out), 3);
            assert_eq!(out, [k, k + 1, k + 2]);
        }
    }
}
