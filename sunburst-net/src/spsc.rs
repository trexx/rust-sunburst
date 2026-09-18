// SPDX-License-Identifier: GPL-2.0-or-later

//! A bounded, lock-free single-producer/single-consumer ring of wire packets.
//!
//! The seam between the two hot-path stages the server splits across threads:
//! the GPU thread (capture → convert → encode → packetize) is the sole producer,
//! the network thread (send) the sole consumer. CLAUDE.md forbids locks on the
//! frame path, so this is head/tail atomics over preallocated fixed slots — no
//! `Mutex`, and no allocation after construction. A full ring **drops** rather
//! than blocks: stalling the encoder to wait on the socket is the one thing a
//! low-latency path must never do, and a dropped frame is recovered by the next
//! keyframe, not by adding latency to every frame behind it.
//!
//! Each slot holds one whole packet (header + payload, up to
//! [`MAX_PACKET`](crate::video)); packets are already MTU-bounded, so a slot is a
//! fixed array and there is no byte-level wraparound to reason about.
//!
//! It is deliberately plain `std` and platform-free, so it is exercised on the
//! Linux host — including the two-thread test below — even though its only user
//! is the Windows server pipeline.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sunburst_core::proto::{HEADER_LEN, MAX_PAYLOAD};

/// The largest packet a slot holds: the common header plus one MTU payload.
pub const SLOT_BYTES: usize = HEADER_LEN + MAX_PAYLOAD;

struct Slot {
    /// Written by the producer before it publishes `tail`, read by the consumer
    /// after it observes `tail`; the atomic ordering is what makes that safe.
    data: UnsafeCell<[u8; SLOT_BYTES]>,
    len: UnsafeCell<usize>,
}

struct Inner {
    slots: Box<[Slot]>,
    /// `capacity - 1`; capacity is a power of two so wrap is a mask.
    mask: usize,
    /// Next index to write. Only the producer stores it.
    tail: AtomicUsize,
    /// Next index to read. Only the consumer stores it.
    head: AtomicUsize,
    /// Packets dropped because the ring was full. Producer-only, for the drain.
    dropped: AtomicUsize,
}

// SAFETY: the slots are shared, but access is disciplined by `head`/`tail`: the
// producer only ever writes the slot at `tail` (which the consumer has already
// passed), the consumer only ever reads the slot at `head` (which the producer
// has already published), and the release/acquire pairing on those indices
// establishes the happens-before that makes the slot writes visible. This is the
// same pattern as `sunburst_core::instr::ring`.
unsafe impl Send for Inner {}
// SAFETY: see the `Send` impl above — the head/tail discipline makes concurrent
// producer and consumer access race-free, so `&Inner` is safe to share.
unsafe impl Sync for Inner {}

/// The producing half. Lives on the GPU thread.
pub struct Producer {
    inner: Arc<Inner>,
}

/// The consuming half. Lives on the network (send) thread.
pub struct Consumer {
    inner: Arc<Inner>,
}

/// Build a ring holding up to `capacity` packets (rounded up to a power of two,
/// minimum 2). Returns the producer and consumer halves.
pub fn packet_ring(capacity: usize) -> (Producer, Consumer) {
    let cap = capacity.max(2).next_power_of_two();
    let slots = (0..cap)
        .map(|_| Slot {
            data: UnsafeCell::new([0u8; SLOT_BYTES]),
            len: UnsafeCell::new(0),
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let inner = Arc::new(Inner {
        slots,
        mask: cap - 1,
        tail: AtomicUsize::new(0),
        head: AtomicUsize::new(0),
        dropped: AtomicUsize::new(0),
    });
    (
        Producer {
            inner: Arc::clone(&inner),
        },
        Consumer { inner },
    )
}

impl Producer {
    /// Copy `packet` into the ring. Returns `false` — and counts a drop — if the
    /// ring is full or the packet does not fit a slot. Never blocks.
    pub fn push(&self, packet: &[u8]) -> bool {
        if packet.len() > SLOT_BYTES {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let tail = self.inner.tail.load(Ordering::Relaxed);
        let head = self.inner.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) > self.inner.mask {
            // Full: the one free slot the mask reserves is the head's.
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let slot = &self.inner.slots[tail & self.inner.mask];
        // SAFETY: this slot sits at `tail`, which the consumer has not reached
        // (the fullness check above guarantees it), so we hold it exclusively
        // until the release-store of `tail` publishes it.
        unsafe {
            let buf = &mut *slot.data.get();
            buf[..packet.len()].copy_from_slice(packet);
            *slot.len.get() = packet.len();
        }
        self.inner
            .tail
            .store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Packets dropped so far because the ring was full.
    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

impl Consumer {
    /// Hand the next packet to `send` in place (no copy) and free its slot.
    /// Returns `false` if the ring is empty. `send` sees exactly the bytes the
    /// producer wrote.
    pub fn pop_with(&self, send: impl FnOnce(&[u8])) -> bool {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);
        if head == tail {
            return false; // empty
        }
        let slot = &self.inner.slots[head & self.inner.mask];
        // SAFETY: this slot sits at `head`, which the producer published (the
        // acquire-load of `tail` above observed the matching release-store), and
        // the producer will not reuse it until we advance `head`. So the bytes
        // are valid and unaliased for the duration of `send`.
        unsafe {
            let len = *slot.len.get();
            let buf = &*slot.data.get();
            send(&buf[..len]);
        }
        self.inner
            .head
            .store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Copy the next packet into `out`, returning its length, or `None` if empty.
    /// A convenience over [`pop_with`](Self::pop_with) for callers that want an
    /// owned copy; the send path prefers `pop_with`.
    pub fn pop(&self, out: &mut [u8]) -> Option<usize> {
        let mut copied = None;
        self.pop_with(|pkt| {
            let n = pkt.len().min(out.len());
            out[..n].copy_from_slice(&pkt[..n]);
            copied = Some(n);
        });
        copied
    }

    /// Whether the ring currently holds no packets.
    pub fn is_empty(&self) -> bool {
        self.inner.head.load(Ordering::Relaxed) == self.inner.tail.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(tag: u8, len: usize) -> Vec<u8> {
        vec![tag; len]
    }

    #[test]
    fn fifo_order_is_preserved() {
        let (tx, rx) = packet_ring(8);
        assert!(tx.push(&pkt(1, 10)));
        assert!(tx.push(&pkt(2, 20)));
        assert!(tx.push(&pkt(3, 30)));

        let mut out = [0u8; SLOT_BYTES];
        assert_eq!(rx.pop(&mut out), Some(10));
        assert_eq!(out[0], 1);
        assert_eq!(rx.pop(&mut out), Some(20));
        assert_eq!(out[0], 2);
        assert_eq!(rx.pop(&mut out), Some(30));
        assert_eq!(out[0], 3);
        assert_eq!(rx.pop(&mut out), None, "empty after draining");
    }

    #[test]
    fn a_full_ring_drops_rather_than_blocks() {
        // capacity 4 holds exactly 4 (separate head/tail counters distinguish
        // full from empty, so no slot is sacrificed).
        let (tx, rx) = packet_ring(4);
        for tag in 1..=4u8 {
            assert!(tx.push(&pkt(tag, 1)), "{tag} should fit");
        }
        assert!(!tx.push(&pkt(5, 1)), "fifth overflows a full ring");
        assert_eq!(tx.dropped(), 1);
        // Freeing one slot lets one more in.
        let mut out = [0u8; SLOT_BYTES];
        assert_eq!(rx.pop(&mut out), Some(1));
        assert!(tx.push(&pkt(5, 1)));
    }

    #[test]
    fn an_oversize_packet_is_refused() {
        let (tx, _rx) = packet_ring(4);
        let too_big = vec![0u8; SLOT_BYTES + 1];
        assert!(!tx.push(&too_big));
        assert_eq!(tx.dropped(), 1);
    }

    #[test]
    fn indices_wrap_without_losing_packets() {
        let (tx, rx) = packet_ring(4);
        let mut out = [0u8; SLOT_BYTES];
        // Push/pop far more than capacity to force the head/tail wrap.
        for i in 0..1000u32 {
            let tag = (i & 0xFF) as u8;
            assert!(tx.push(&pkt(tag, 4)));
            assert_eq!(rx.pop(&mut out), Some(4));
            assert_eq!(out[0], tag);
        }
    }

    #[test]
    fn pop_with_sees_the_exact_bytes_and_frees_the_slot() {
        let (tx, rx) = packet_ring(4);
        tx.push(&[9, 8, 7]);
        let mut seen = Vec::new();
        assert!(rx.pop_with(|p| seen.extend_from_slice(p)));
        assert_eq!(seen, vec![9, 8, 7]);
        assert!(rx.is_empty());
        assert!(!rx.pop_with(|_| unreachable!("ring is empty")));
    }

    #[test]
    fn survives_a_producer_and_consumer_on_two_threads() {
        // The property that matters: everything pushed is received once, in
        // order, with no torn packets, across a real thread boundary.
        const N: u32 = 200_000;
        let (tx, rx) = packet_ring(64);
        let producer = std::thread::spawn(move || {
            for i in 0..N {
                // Encode the counter into the payload so the consumer can verify
                // both order and integrity; retry on a full ring.
                let bytes = i.to_le_bytes();
                let mut p = [0u8; 8];
                p[..4].copy_from_slice(&bytes);
                p[4..].copy_from_slice(&bytes); // duplicate: catches torn writes
                while !tx.push(&p) {
                    std::thread::yield_now();
                }
            }
        });
        let consumer = std::thread::spawn(move || {
            let mut next = 0u32;
            let mut out = [0u8; SLOT_BYTES];
            while next < N {
                match rx.pop(&mut out) {
                    Some(8) => {
                        let a = u32::from_le_bytes(out[..4].try_into().unwrap());
                        let b = u32::from_le_bytes(out[4..8].try_into().unwrap());
                        assert_eq!(a, b, "torn packet at {next}");
                        assert_eq!(a, next, "out of order");
                        next += 1;
                    }
                    Some(n) => panic!("wrong length {n}"),
                    None => std::thread::yield_now(),
                }
            }
            next
        });
        producer.join().unwrap();
        assert_eq!(consumer.join().unwrap(), N);
    }
}
