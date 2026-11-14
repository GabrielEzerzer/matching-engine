//! Reads an ITCH file in large chunks so that the bytes a caller parses are already resident in
//! memory: no file I/O and no page faults happen while a chunk is being parsed.
//!
//! The buffer is allocated once. Each [`ChunkReader::fill`] moves the unparsed tail of the
//! previous chunk (a frame cut by the chunk boundary) to the front and reads until the buffer is
//! full or the file ends; the read writes every byte it returns, so those pages are resident
//! before the caller parses them.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const DEFAULT_CHUNK: usize = 64 << 20;

pub struct ChunkReader {
    file: File,
    buf: Vec<u8>,
    /// Valid bytes in `buf`.
    len: usize,
    eof: bool,
}

impl ChunkReader {
    pub fn open(path: &Path, chunk: usize) -> io::Result<Self> {
        // Every frame is at most 2 + 65535 bytes, so a chunk must hold at least one.
        assert!(chunk > 1 << 17, "chunk too small for a frame");
        Ok(Self {
            file: File::open(path)?,
            // A zeroed allocation, mapped lazily: the first fill's reads fault its pages in.
            buf: vec![0u8; chunk],
            len: 0,
            eof: false,
        })
    }

    /// Reads more of the file after the bytes still in the buffer. Returns how many bytes it
    /// read; they are the last that many bytes of [`ChunkReader::data`].
    pub fn fill(&mut self) -> io::Result<usize> {
        let start = self.len;
        while !self.eof && self.len < self.buf.len() {
            match self.file.read(&mut self.buf[self.len..]) {
                Ok(0) => self.eof = true,
                Ok(n) => self.len += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(self.len - start)
    }

    /// The bytes read and not yet consumed.
    pub fn data(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Drops the first `n` bytes of [`ChunkReader::data`], keeping the rest for the next fill.
    pub fn consume(&mut self, n: usize) {
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }

    /// Whether the whole file has been read into the buffer at some point.
    pub fn eof(&self) -> bool {
        self.eof
    }
}
