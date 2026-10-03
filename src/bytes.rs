//! Bounds-checked little-endian field access for the iPod's binary chunk formats.

use anyhow::{Result, bail};

/// A view over one chunk (`mhbd`, `mhit`, `mhod`, ...) starting at `off` in `buf`.
#[derive(Clone, Copy)]
pub struct Chunk<'a> {
    pub buf: &'a [u8],
    pub off: usize,
}

impl<'a> Chunk<'a> {
    pub fn at(buf: &'a [u8], off: usize) -> Result<Self> {
        if off + 12 > buf.len() {
            bail!("chunk at {off:#x} runs past end of file");
        }
        Ok(Self { buf, off })
    }

    pub fn tag(&self) -> &'a [u8] {
        &self.buf[self.off..self.off + 4]
    }

    pub fn expect(&self, tag: &[u8; 4]) -> Result<()> {
        if self.tag() != tag {
            bail!(
                "expected {} at {:#x}, found {:?}",
                String::from_utf8_lossy(tag),
                self.off,
                String::from_utf8_lossy(self.tag())
            );
        }
        Ok(())
    }

    pub fn header_len(&self) -> usize {
        self.u32(4) as usize
    }

    /// Third header word: total length for most chunks, item count for list chunks.
    pub fn total_len(&self) -> usize {
        self.u32(8) as usize
    }

    /// The chunk's bytes from its start up to `total_len`, clamped to the buffer.
    pub fn end(&self) -> usize {
        (self.off + self.total_len()).min(self.buf.len())
    }

    /// The first child chunk, located right after this chunk's header.
    pub fn first_child(&self) -> Result<Chunk<'a>> {
        Chunk::at(self.buf, self.off + self.header_len())
    }

    /// Fields read past the header or file end return 0, so older/shorter
    /// header variants degrade gracefully instead of failing.
    pub fn u8(&self, rel: usize) -> u8 {
        self.slice(rel, 1).map_or(0, |b| b[0])
    }

    pub fn u16(&self, rel: usize) -> u16 {
        self.slice(rel, 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&self, rel: usize) -> u32 {
        self.slice(rel, 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn u64(&self, rel: usize) -> u64 {
        self.slice(rel, 8)
            .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn slice(&self, rel: usize, len: usize) -> Option<&'a [u8]> {
        let start = self.off.checked_add(rel)?;
        self.buf.get(start..start.checked_add(len)?)
    }
}

pub fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// iPod timestamps count seconds from 1904-01-01 (classic Mac epoch).
pub fn mac_to_unix(t: u32) -> Option<i64> {
    (t != 0).then(|| t as i64 - 2_082_844_800)
}
