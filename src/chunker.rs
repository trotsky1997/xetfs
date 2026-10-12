//! Streaming scalar Gearhash CDC. Boundaries do not depend on Read buffer sizes.
use std::io::{BufRead, BufReader, Read};

pub const MIN_CHUNK: usize = 8 * 1024;
pub const MAX_CHUNK: usize = 128 * 1024;
pub const MASK: u64 = 0xffff_0000_0000_0000;

pub struct Chunker<R> {
    input: BufReader<R>,
    done: bool,
}
impl<R: Read> Chunker<R> {
    pub fn new(input: R) -> Self {
        Self {
            input: BufReader::with_capacity(MAX_CHUNK, input),
            done: false,
        }
    }
}
impl<R: Read> Iterator for Chunker<R> {
    type Item = std::io::Result<Vec<u8>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mut chunk = Vec::with_capacity(MAX_CHUNK);
        let mut hash = 0u64;
        loop {
            let buf = match self.input.fill_buf() {
                Ok(b) => b,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            if buf.is_empty() {
                self.done = true;
                return (!chunk.is_empty()).then_some(Ok(chunk));
            }
            let mut count = 0;
            let mut boundary = false;
            for &byte in buf {
                hash = hash
                    .wrapping_shl(1)
                    .wrapping_add(gearhash::DEFAULT_TABLE[byte as usize]);
                chunk.push(byte);
                count += 1;
                if chunk.len() >= MAX_CHUNK || (chunk.len() >= MIN_CHUNK && hash & MASK == 0) {
                    boundary = true;
                    break;
                }
            }
            self.input.consume(count);
            if boundary {
                return Some(Ok(chunk));
            }
        }
    }
}
