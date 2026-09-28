// SPDX-License-Identifier: GPL-2.0-or-later

//! Kernel socket buffer sizing.
//!
//! A 4K frame is 100–250 KB — 80 to 200 datagrams sent as one paced burst — and
//! the OS defaults (~200 KB receive on Android/Linux, 64 KB on Windows) hold
//! about one. Any stall in the receive loop longer than a burst then drops the
//! tail of a frame, which costs a NACK round at best and a keyframe at worst.
//! So both ends ask for several frames' worth. The request is best-effort: the
//! kernel clamps it (`net.core.rmem_max` on Linux), and a refusal leaves the
//! default in place rather than failing the session.
//!
//! Control-plane only: called once when a socket is set up.

use std::net::UdpSocket;

/// What each end asks for: several 4K frames' worth.
pub const RECV_BUFFER_BYTES: usize = 8 << 20;
pub const SEND_BUFFER_BYTES: usize = 4 << 20;

/// Which buffer to size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buffer {
    Recv,
    Send,
}

/// Ask for `bytes` of `which` buffer on `socket`, returning the size the kernel
/// actually granted (as `getsockopt` reports it; Linux reports double the
/// usable size for its bookkeeping), or `None` if it could not be read.
pub fn request(socket: &UdpSocket, which: Buffer, bytes: usize) -> Option<usize> {
    imp::request(socket, which, bytes.min(i32::MAX as usize) as i32)
}

#[cfg(unix)]
mod imp {
    use std::os::fd::AsRawFd;

    use super::{Buffer, UdpSocket};

    pub fn request(socket: &UdpSocket, which: Buffer, bytes: i32) -> Option<usize> {
        let opt = match which {
            Buffer::Recv => libc::SO_RCVBUF,
            Buffer::Send => libc::SO_SNDBUF,
        };
        let fd = socket.as_raw_fd();
        let len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `fd` is a live socket owned by `socket`; `bytes` is a c_int
        // and `len` its size. A failure leaves the default in place.
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, opt, (&raw const bytes).cast(), len);
        }
        let mut granted: libc::c_int = 0;
        let mut out_len = len;
        // SAFETY: as above; `granted` is a c_int and `out_len` its size.
        let status = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw mut granted).cast(),
                &mut out_len,
            )
        };
        (status == 0).then_some(granted.max(0) as usize)
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawSocket;

    use windows::Win32::Networking::WinSock::{
        SO_RCVBUF, SO_SNDBUF, SOCKET, SOL_SOCKET, getsockopt, setsockopt,
    };
    use windows::core::PSTR;

    use super::{Buffer, UdpSocket};

    pub fn request(socket: &UdpSocket, which: Buffer, bytes: i32) -> Option<usize> {
        let opt = match which {
            Buffer::Recv => SO_RCVBUF,
            Buffer::Send => SO_SNDBUF,
        };
        let handle = SOCKET(socket.as_raw_socket() as usize);
        // SAFETY: `handle` is a live socket owned by `socket`; the option value
        // is a 4-byte int. A failure leaves the default in place.
        unsafe {
            setsockopt(handle, SOL_SOCKET, opt, Some(&bytes.to_ne_bytes()));
        }
        let mut granted = [0u8; 4];
        let mut len = granted.len() as i32;
        // SAFETY: as above; `granted` is 4 writable bytes and `len` says so.
        let status = unsafe {
            getsockopt(
                handle,
                SOL_SOCKET,
                opt,
                PSTR(granted.as_mut_ptr()),
                &mut len,
            )
        };
        (status == 0).then(|| i32::from_ne_bytes(granted).max(0) as usize)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::{Buffer, UdpSocket};

    pub fn request(_: &UdpSocket, _: Buffer, _: i32) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_reports_what_was_granted() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Clamped by rmem_max/wmem_max on a stock host, so only "something".
        assert!(request(&socket, Buffer::Recv, 1 << 20).is_some_and(|n| n > 0));
        assert!(request(&socket, Buffer::Send, 1 << 20).is_some_and(|n| n > 0));
    }
}
