//! Xet's keyed BLAKE3 hashes and content-dependent Merkle reduction.
//! Hex strings use four little-endian u64 words, as in the Xet API.
use crate::{Error, Result};
use std::fmt::Write;

pub type Hash = [u8; 32];
const DATA_KEY: Hash = [
    102, 151, 245, 119, 91, 149, 80, 222, 49, 53, 203, 172, 165, 151, 24, 28, 157, 228, 33, 16,
    155, 235, 43, 88, 180, 208, 176, 75, 147, 173, 242, 41,
];
const NODE_KEY: Hash = [
    1, 126, 197, 199, 165, 71, 41, 150, 253, 148, 102, 102, 180, 138, 2, 230, 93, 221, 83, 111, 55,
    199, 109, 210, 248, 99, 82, 230, 74, 83, 113, 63,
];
pub const VERIFICATION_KEY: Hash = [
    127, 24, 87, 214, 206, 86, 237, 102, 18, 127, 249, 19, 231, 165, 195, 243, 164, 205, 38, 213,
    181, 219, 73, 230, 65, 36, 152, 127, 40, 251, 148, 195,
];

pub fn chunk_hash(data: &[u8]) -> Hash {
    *blake3::keyed_hash(&DATA_KEY, data).as_bytes()
}
pub fn to_hex(hash: &Hash) -> String {
    let mut s = String::with_capacity(64);
    for word in hash.as_chunks::<8>().0 {
        write!(s, "{:016x}", u64::from_le_bytes(*word)).unwrap();
    }
    s
}
pub fn from_hex(s: &str) -> Result<Hash> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid("expected a 64-character Xet hash".into()));
    }
    let mut h = [0u8; 32];
    for (i, word) in h.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        let n = u64::from_str_radix(&s[i * 16..i * 16 + 16], 16)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        word.copy_from_slice(&n.to_le_bytes());
    }
    Ok(h)
}
pub fn xorb_hash(chunks: &[(Hash, u64)]) -> Hash {
    if chunks.is_empty() {
        return [0; 32];
    }
    let mut nodes = chunks.to_vec();
    while nodes.len() > 1 {
        let mut parents = Vec::with_capacity(nodes.len().div_ceil(2));
        let mut start = 0;
        while start < nodes.len() {
            let tail = &nodes[start..];
            let mut count = tail.len().min(9);
            for (i, (hash, _)) in tail.iter().enumerate().take(count).skip(2) {
                if u64::from_le_bytes(hash[24..32].try_into().unwrap()) % 4 == 0 {
                    count = i + 1;
                    break;
                }
            }
            let mut text = String::with_capacity(count * 88);
            let mut size = 0;
            for (h, len) in &tail[..count] {
                writeln!(text, "{} : {}", to_hex(h), len).unwrap();
                size += len;
            }
            parents.push((
                *blake3::keyed_hash(&NODE_KEY, text.as_bytes()).as_bytes(),
                size,
            ));
            start += count;
        }
        nodes = parents;
    }
    nodes[0].0
}
pub fn file_hash(chunks: &[(Hash, u64)]) -> Hash {
    if chunks.is_empty() {
        [0; 32]
    } else {
        *blake3::keyed_hash(&[0; 32], &xorb_hash(chunks)).as_bytes()
    }
}
pub fn verification_hash(chunks: &[Hash]) -> Hash {
    let mut h = blake3::Hasher::new_keyed(&VERIFICATION_KEY);
    for chunk in chunks {
        h.update(chunk);
    }
    *h.finalize().as_bytes()
}
