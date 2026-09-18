// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(windows)]

//! UDP send offload (USO) via `WSASendMsg` + `UDP_SEND_MSG_SIZE`.
//!
//! One `WSASendMsg` hands the kernel a contiguous run of equal-size datagrams
//! and a control message naming the segment size; the kernel splits it into
//! that many packets. At 4K60 packet rates this is the single biggest CPU win
//! in the transport — roughly a 30× cut in send syscalls — and it is what keeps
//! naive raw UDP ahead of a tuned QUIC stack (CLAUDE.md, ROADMAP Phase 4).
//!
//! `UDP_SEND_MSG_SIZE` needs a recent enough Windows; there is no version note
//! in the SDK header, so rather than gate on a build number this probes at
//! runtime: the first `WSASendMsg` that the option refuses flips the sender to
//! per-datagram `send_to` for the rest of the session, and [`Sender::offloaded`]
//! reports which path is live so the metrics can say so.
//!
//! Thin FFI, per CLAUDE.md — transcription from the Winsock headers, unsafe kept
//! at the boundary. Compile-verified with `cargo xwin`; the offload path itself
//! is box-to-validate (this host has no Winsock).

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;

use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_INET, AF_INET6, CMSGHDR, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0,
    IPPROTO_UDP, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKET, UDP_SEND_MSG_SIZE,
    WSABUF, WSAMSG, WSASendMsg,
};
use windows_core::PSTR;

use super::Sender;

/// A control message carrying one `UDP_SEND_MSG_SIZE` value.
///
/// `CMSGHDR` (16 bytes on x64) then the `u32` segment size. `cmsg_len` counts
/// the header plus the value (`WSA_CMSG_LEN`); the buffer length passed to the
/// kernel is the whole aligned struct (`WSA_CMSG_SPACE`).
#[repr(C)]
struct SegmentCmsg {
    hdr: CMSGHDR,
    segment_size: u32,
    _pad: u32,
}

/// Sends batches with USO, falling back to per-datagram once the option is
/// refused.
pub struct WsaSender<'a> {
    socket: &'a UdpSocket,
    handle: SOCKET,
    uso_ok: bool,
}

impl<'a> WsaSender<'a> {
    pub fn new(socket: &'a UdpSocket) -> WsaSender<'a> {
        let handle = SOCKET(socket.as_raw_socket() as usize);
        WsaSender {
            socket,
            handle,
            uso_ok: true,
        }
    }

    /// Force the per-datagram path, for the `SUNBURST_NO_USO` bring-up switch.
    pub fn without_offload(socket: &'a UdpSocket) -> WsaSender<'a> {
        WsaSender {
            socket,
            handle: SOCKET(socket.as_raw_socket() as usize),
            uso_ok: false,
        }
    }

    fn send_plain(&self, buf: &[u8], seg: usize, addr: SocketAddr) -> io::Result<()> {
        for chunk in buf.chunks(seg.max(1)) {
            self.socket.send_to(chunk, addr)?;
        }
        Ok(())
    }

    /// One `WSASendMsg` for the whole batch. Returns the OS error on failure so
    /// the caller can decide whether to fall back.
    fn send_uso(&self, buf: &[u8], seg_size: usize, addr: SocketAddr) -> io::Result<()> {
        // The destination, in a storage large enough for either family.
        let mut storage = [0u8; core::mem::size_of::<SOCKADDR_IN6>()];
        let namelen = encode_sockaddr(addr, &mut storage);

        let mut iov = WSABUF {
            len: buf.len() as u32,
            // WSASendMsg does not write through this pointer.
            buf: PSTR(buf.as_ptr() as *mut u8),
        };

        let mut cmsg = SegmentCmsg {
            hdr: CMSGHDR {
                // WSA_CMSG_LEN(sizeof(u32)): the header plus the value.
                cmsg_len: core::mem::size_of::<CMSGHDR>() + core::mem::size_of::<u32>(),
                cmsg_level: IPPROTO_UDP.0,
                cmsg_type: UDP_SEND_MSG_SIZE,
            },
            segment_size: seg_size as u32,
            _pad: 0,
        };

        let msg = WSAMSG {
            name: storage.as_mut_ptr() as *mut SOCKADDR,
            namelen,
            lpBuffers: &mut iov,
            dwBufferCount: 1,
            Control: WSABUF {
                // WSA_CMSG_SPACE: the whole aligned control struct.
                len: core::mem::size_of::<SegmentCmsg>() as u32,
                buf: PSTR(&mut cmsg as *mut _ as *mut u8),
            },
            dwFlags: 0,
        };

        let mut sent = 0u32;
        // SAFETY: `msg` points at `storage`, `iov`, `buf` and `cmsg`, all live
        // for this call; a synchronous send (no overlapped/completion) writes
        // `sent` and returns 0 on success.
        let rc = unsafe { WSASendMsg(self.handle, &msg, 0, Some(&mut sent), None, None) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Sender for WsaSender<'_> {
    fn send_batch(&mut self, buf: &[u8], seg_size: usize, addr: SocketAddr) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        // A single-segment batch has nothing to offload.
        if !self.uso_ok || buf.len() <= seg_size {
            return self.send_plain(buf, seg_size, addr);
        }
        match self.send_uso(buf, seg_size, addr) {
            Ok(()) => Ok(()),
            Err(_) => {
                // The kernel refused the option (old build, or a driver that
                // does not implement it). Stop trying and use per-datagram
                // sends for the rest of the session.
                self.uso_ok = false;
                self.send_plain(buf, seg_size, addr)
            }
        }
    }

    fn offloaded(&self) -> bool {
        self.uso_ok
    }
}

/// Write `addr` into `storage` as a `SOCKADDR_IN`/`SOCKADDR_IN6`, returning the
/// length. Ports and addresses go on the wire in network byte order.
fn encode_sockaddr(addr: SocketAddr, storage: &mut [u8]) -> i32 {
    match addr {
        SocketAddr::V4(v4) => {
            let sa = SOCKADDR_IN {
                sin_family: ADDRESS_FAMILY(AF_INET.0),
                sin_port: v4.port().to_be(),
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(v4.ip().octets()),
                    },
                },
                sin_zero: [0; 8],
            };
            let n = core::mem::size_of::<SOCKADDR_IN>();
            // SAFETY: SOCKADDR_IN is plain data and `storage` is larger than it.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    &sa as *const _ as *const u8,
                    storage.as_mut_ptr(),
                    n,
                );
            }
            n as i32
        }
        SocketAddr::V6(v6) => {
            let sa = SOCKADDR_IN6 {
                sin6_family: ADDRESS_FAMILY(AF_INET6.0),
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo().to_be(),
                sin6_addr: IN6_ADDR {
                    u: IN6_ADDR_0 {
                        Byte: v6.ip().octets(),
                    },
                },
                Anonymous: SOCKADDR_IN6_0 {
                    sin6_scope_id: v6.scope_id(),
                },
            };
            let n = core::mem::size_of::<SOCKADDR_IN6>();
            // SAFETY: SOCKADDR_IN6 is plain data and `storage` is exactly it.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    &sa as *const _ as *const u8,
                    storage.as_mut_ptr(),
                    n,
                );
            }
            n as i32
        }
    }
}
