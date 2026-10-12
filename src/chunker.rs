//! Streaming SIMD-accelerated Gearhash CDC. Boundaries do not depend on Read buffer sizes.
use gearhash::Hasher;
use std::io::{BufRead, BufReader, Read};

pub const MIN_CHUNK: usize = 8 * 1024;
pub const MAX_CHUNK: usize = 128 * 1024;
pub const MASK: u64 = 0xffff_0000_0000_0000;
const GEAR_WINDOW: usize = 64;
const _: () = assert!(MIN_CHUNK >= GEAR_WINDOW);

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
        let mut hasher = Hasher::default();
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
            if chunk.len() < MIN_CHUNK {
                let count = (MIN_CHUNK - chunk.len()).min(buf.len());
                chunk.extend_from_slice(&buf[..count]);
                self.input.consume(count);
                if chunk.len() == MIN_CHUNK {
                    // Gearhash state depends only on the last 64 bytes. Replaying
                    // that window computes the exact hash at MIN_CHUNK without
                    // scanning the prefix where boundaries are forbidden.
                    hasher.update(&chunk[MIN_CHUNK - GEAR_WINDOW..]);
                    if hasher.is_match(MASK) {
                        return Some(Ok(chunk));
                    }
                }
                continue;
            }

            // gearhash::Hasher dispatches to NEON/SSE/AVX scanners where available.
            // Keep the scan bounded so the forced MAX_CHUNK boundary remains exact.
            let count = (MAX_CHUNK - chunk.len()).min(buf.len());
            if let Some(boundary) = hasher.next_match(&buf[..count], MASK) {
                chunk.extend_from_slice(&buf[..boundary]);
                self.input.consume(boundary);
                return Some(Ok(chunk));
            }
            chunk.extend_from_slice(&buf[..count]);
            self.input.consume(count);
            if chunk.len() == MAX_CHUNK {
                return Some(Ok(chunk));
            }
        }
    }
}
