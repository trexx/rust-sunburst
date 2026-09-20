// SPDX-License-Identifier: GPL-2.0-or-later

//! A minimal IVF writer, so an AV1 dump plays in ffmpeg/VLC on the TVs.
//!
//! IVF is the simplest container AV1 tools accept: a 32-byte file header, then
//! each temporal unit prefixed by its size and a presentation timestamp. The
//! frame count in the header is patched on close.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};

pub struct IvfWriter {
    file: File,
    frames: u32,
}

impl IvfWriter {
    pub fn create(path: &str, width: u16, height: u16, fps: u32) -> io::Result<IvfWriter> {
        let mut file = File::create(path)?;
        let mut header = [0u8; 32];
        header[0..4].copy_from_slice(b"DKIF");
        // version 0, header length 32.
        header[6..8].copy_from_slice(&32u16.to_le_bytes());
        header[8..12].copy_from_slice(b"AV01");
        header[12..14].copy_from_slice(&width.to_le_bytes());
        header[14..16].copy_from_slice(&height.to_le_bytes());
        // Timebase = fps : 1, so a timestamp is a frame index.
        header[16..20].copy_from_slice(&fps.max(1).to_le_bytes());
        header[20..24].copy_from_slice(&1u32.to_le_bytes());
        // header[24..28] frame count, patched on close.
        file.write_all(&header)?;
        Ok(IvfWriter { file, frames: 0 })
    }

    pub fn write_frame(&mut self, data: &[u8]) -> io::Result<()> {
        let mut hdr = [0u8; 12];
        hdr[0..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        hdr[4..12].copy_from_slice(&(self.frames as u64).to_le_bytes());
        self.file.write_all(&hdr)?;
        self.file.write_all(data)?;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<u32> {
        self.file.seek(SeekFrom::Start(24))?;
        self.file.write_all(&self.frames.to_le_bytes())?;
        self.file.flush()?;
        Ok(self.frames)
    }
}
