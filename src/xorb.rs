//! Xet v0 chunk records: 8-byte header, raw/LZ4-frame/BG4+LZ4 payload.
use crate::{Error, Result, chunker::MAX_CHUNK};
use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use std::io::{Read, Write};

pub const MAX_XORB: usize = 64 * 1024 * 1024;

fn group(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for lane in 0..4 {
        out.extend(data.iter().skip(lane).step_by(4));
    }
    out
}
fn ungroup(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0; data.len()];
    let mut source = 0;
    for lane in 0..4 {
        for i in (lane..data.len()).step_by(4) {
            out[i] = data[source];
            source += 1;
        }
    }
    out
}
fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = FrameEncoder::new(Vec::new());
    enc.write_all(data)?;
    enc.finish()
        .map_err(|e| Error::Io(std::io::Error::other(e)))
}
pub fn encode(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() > MAX_CHUNK {
        return Err(Error::Invalid("invalid chunk size".into()));
    }
    let plain = compress(data)?;
    let bg4 = compress(&group(data))?;
    let (scheme, payload) = if plain.len() < data.len() && plain.len() <= bg4.len() {
        (1, plain.as_slice())
    } else if bg4.len() < data.len() {
        (2, bg4.as_slice())
    } else {
        (0, data)
    };
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes()[..3]);
    out.push(scheme);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes()[..3]);
    out.extend_from_slice(payload);
    Ok(out)
}
pub fn decode<R: Read>(input: &mut R) -> Result<Option<(Vec<u8>, u64)>> {
    let mut hdr = [0; 8];
    loop {
        match input.read(&mut hdr[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    input.read_exact(&mut hdr[1..4])?;
    // The pinned official reference artifact ends its chunk stream with XETB.
    // Accept this observed terminator only at EOF, never as a chunk header.
    if &hdr[..4] == b"XETB" {
        let mut trailing = [0u8; 1];
        if input.read(&mut trailing)? != 0 {
            return Err(Error::Corrupt("bytes after XETB terminator".into()));
        }
        return Ok(None);
    }
    input.read_exact(&mut hdr[4..])?;
    let packed = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], 0]) as usize;
    let raw = u32::from_le_bytes([hdr[5], hdr[6], hdr[7], 0]) as usize;
    if hdr[0] != 0
        || hdr[4] > 2
        || raw == 0
        || raw > MAX_CHUNK
        || packed == 0
        || packed > MAX_CHUNK * 2
    {
        return Err(Error::Corrupt("invalid Xorb chunk header".into()));
    }
    let mut payload = vec![0; packed];
    input.read_exact(&mut payload)?;
    let data = if hdr[4] == 0 {
        payload
    } else {
        let mut out = Vec::with_capacity(raw);
        FrameDecoder::new(payload.as_slice())
            .take((raw + 1) as u64)
            .read_to_end(&mut out)?;
        if hdr[4] == 2 { ungroup(&out) } else { out }
    };
    if data.len() != raw {
        return Err(Error::Corrupt("Xorb decompression length mismatch".into()));
    }
    Ok(Some((data, (packed + 8) as u64)))
}
