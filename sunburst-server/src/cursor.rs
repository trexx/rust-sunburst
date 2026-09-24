// SPDX-License-Identifier: GPL-2.0-or-later

//! Cursor shape and position capture, so the client renders the pointer itself.
//!
//! Capture excludes the hardware cursor (DDA never draws it; WGC has it
//! disabled; NvFBC excludes it), so the server ships the shape and position and
//! the client draws it — removing the network round trip from perceived pointer
//! latency (CLAUDE.md: the biggest single responsiveness win).
//!
//! `GetCursorInfo` gives the current `HCURSOR` and screen position; on a change
//! of cursor the shape is read from its colour bitmap as top-down BGRA via
//! `GetDIBits` and chunked into [`CursorChunk`]s. Monochrome (colour-less)
//! cursors are rare on the target OSes and are skipped for now. Box-to-validate.

use sunburst_core::proto::{CURSOR_CHUNK_MAX, CURSOR_FORMAT_BGRA32, CursorChunk};
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW,
    HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, GetSystemMetrics, HICON, ICONINFO,
    SM_CXSCREEN, SM_CYSCREEN,
};

use sunburst_net::cursor::normalise;

/// One poll's worth of cursor state.
pub struct CursorUpdate {
    /// Chunks of a new shape (empty if the shape did not change). A hidden
    /// cursor is a single chunk with `width == 0`.
    pub shape: Vec<CursorChunk>,
    /// The pointer position, normalised to the captured output (0..65535), and
    /// whether it is visible *on that output*.
    pub position: (u16, u16, bool),
}

/// Polls the system cursor, emitting a shape only when it changes.
pub struct CursorPoller {
    last_cursor: isize,
    shape_id: u32,
    hidden_sent: bool,
    /// The captured output, `[left, top, right, bottom]` in virtual-desktop
    /// pixels. The pointer is reported relative to it.
    bounds: [i32; 4],
}

impl Default for CursorPoller {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorPoller {
    /// A poller reporting against the primary monitor at the origin, until
    /// [`set_bounds`](Self::set_bounds) names the captured output.
    pub fn new() -> CursorPoller {
        // SAFETY: GetSystemMetrics is a pure query.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        CursorPoller {
            last_cursor: 0,
            shape_id: 0,
            hidden_sent: false,
            bounds: [0, 0, w.max(1), h.max(1)],
        }
    }

    /// The captured output's rectangle, from `sunburst_capture::output`.
    pub fn set_bounds(&mut self, bounds: [i32; 4]) {
        self.bounds = bounds;
    }

    /// Poll once. Cheap unless the cursor shape changed.
    pub fn poll(&mut self) -> CursorUpdate {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `ci.cbSize` is set; GetCursorInfo fills the rest.
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            return CursorUpdate {
                shape: Vec::new(),
                position: (0, 0, false),
            };
        }
        let showing = ci.flags.0 & CURSOR_SHOWING.0 != 0;
        let mut shape = Vec::new();

        if showing {
            let handle = ci.hCursor.0 as isize;
            if handle != self.last_cursor {
                self.last_cursor = handle;
                self.hidden_sent = false;
                // SAFETY: `ci.hCursor` is the live system cursor.
                if let Some((w, h, hx, hy, bgra)) = unsafe { capture_shape(ci.hCursor) } {
                    self.shape_id = self.shape_id.wrapping_add(1);
                    shape = chunk_shape(self.shape_id, w, h, hx, hy, &bgra);
                }
            }
        } else if !self.hidden_sent {
            self.hidden_sent = true;
            self.last_cursor = 0;
            self.shape_id = self.shape_id.wrapping_add(1);
            shape = vec![CursorChunk {
                shape_id: self.shape_id,
                width: 0,
                height: 0,
                hotspot_x: 0,
                hotspot_y: 0,
                format: CURSOR_FORMAT_BGRA32,
                total_len: 0,
                offset: 0,
                data: Vec::new(),
            }];
        }

        let (x, y, on_output) = normalise(ci.ptScreenPos.x, ci.ptScreenPos.y, self.bounds);
        CursorUpdate {
            shape,
            position: (x, y, showing && on_output),
        }
    }
}

/// Split a BGRA shape into wire chunks (every chunk repeats the head).
fn chunk_shape(shape_id: u32, w: u16, h: u16, hx: u16, hy: u16, bgra: &[u8]) -> Vec<CursorChunk> {
    let total_len = bgra.len() as u32;
    let mut chunks = Vec::new();
    let mut offset = 0usize;
    // A zero-size shape still needs one chunk to announce it.
    loop {
        let end = (offset + CURSOR_CHUNK_MAX).min(bgra.len());
        chunks.push(CursorChunk {
            shape_id,
            width: w,
            height: h,
            hotspot_x: hx,
            hotspot_y: hy,
            format: CURSOR_FORMAT_BGRA32,
            total_len,
            offset: offset as u32,
            data: bgra[offset..end].to_vec(),
        });
        offset = end;
        if offset >= bgra.len() {
            break;
        }
    }
    chunks
}

/// Read a cursor's colour bitmap as top-down BGRA. Returns
/// `(width, height, hotspot_x, hotspot_y, bgra)`, or `None` for a monochrome
/// cursor (no colour bitmap) or on any GDI failure.
///
/// # Safety
/// `hcursor` must be a live cursor handle.
unsafe fn capture_shape(
    hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
) -> Option<(u16, u16, u16, u16, Vec<u8>)> {
    let mut info = ICONINFO::default();
    // SAFETY: caller guarantees a live cursor; GetIconInfo fills `info` and
    // creates the bitmaps we free below.
    unsafe { GetIconInfo(HICON(hcursor.0), &mut info) }.ok()?;
    let color: HBITMAP = info.hbmColor;
    let result = (!color.is_invalid()).then(|| {
        // SAFETY: `color` is a live bitmap from GetIconInfo.
        unsafe { read_bgra(color, info.xHotspot, info.yHotspot) }
    });
    // Always free both bitmaps GetIconInfo created.
    // SAFETY: both handles came from GetIconInfo and are freed once.
    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
    }
    result.flatten()
}

/// # Safety
/// `color` is a live colour bitmap.
unsafe fn read_bgra(color: HBITMAP, hx: u32, hy: u32) -> Option<(u16, u16, u16, u16, Vec<u8>)> {
    let mut bm = BITMAP::default();
    // SAFETY: `color` is live; `bm` is correctly sized.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(color.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down, so row 0 is the top
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: a screen DC for the format conversion; released below.
    let hdc = unsafe { GetDC(None) };
    // SAFETY: `color` is live, `bgra` holds w*h*4 bytes, `bmi` is 32bpp top-down.
    let lines = unsafe {
        GetDIBits(
            hdc,
            color,
            0,
            h as u32,
            Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: `hdc` came from GetDC(None) and is released once.
    unsafe {
        ReleaseDC(None, hdc);
    }
    if lines == 0 {
        return None;
    }
    // Some cursors carry no per-pixel alpha; if every alpha byte is zero, treat
    // the shape as fully opaque rather than invisible. (Proper mask handling for
    // those is box-to-validate.)
    if bgra.iter().skip(3).step_by(4).all(|a| *a == 0) {
        for a in bgra.iter_mut().skip(3).step_by(4) {
            *a = 0xFF;
        }
    }
    Some((w as u16, h as u16, hx as u16, hy as u16, bgra))
}
