//! Reads an ITCH file in large chunks so that the bytes a caller parses are already resident in memory.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const DEFAULT_CHUNK: usize = 64 << 20;

pub struct ChunkReader {
    file: File,
    buf: Vec<u8>,
    len: usize,
    eof: bool,
}

impl ChunkReader {
    pub fn open(path: &Path, chunk: usize) -> io::Result<Self> {
        assert!(chunk > 1 << 17, "chunk too small for a frame");
        Ok(Self {
            file: File::open(path)?,
            buf: vec![0u8; chunk],
            len: 0,
            eof: false,
        })
    }

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

    pub fn data(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn consume(&mut self, n: usize) {
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }

    pub fn eof(&self) -> bool {
        self.eof
    }
}
