use std::io::{self, Read};
use xetfs::{
    chunker::{Chunker, MAX_CHUNK, MIN_CHUNK},
    hash, xorb,
};

fn random(n: usize) -> Vec<u8> {
    let mut s = 0x21f8_13a9_aa87_77b7u64;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}
struct Fragmented<'a> {
    data: &'a [u8],
    max: usize,
}
impl Read for Fragmented<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = out.len().min(self.max).min(self.data.len());
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}
#[test]
fn streaming_boundaries_are_independent_of_read_fragmentation() {
    let data = random(2 * 1024 * 1024 + 13);
    let baseline = Chunker::new(data.as_slice())
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    for max in [1, 13, 8191, 8192, 131071, 131072] {
        let chunks = Chunker::new(Fragmented { data: &data, max })
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(chunks, baseline);
    }
    assert_eq!(baseline.concat(), data);
    for c in baseline.iter().take(baseline.len() - 1) {
        assert!((MIN_CHUNK..=MAX_CHUNK).contains(&c.len()));
    }
}
#[test]
fn empty_tiny_and_forced_boundaries() {
    assert!(Chunker::new(io::empty()).next().is_none());
    for size in [1, 8191, 8192, 131072, 131073, 262144] {
        let data = vec![0; size];
        let chunks = Chunker::new(data.as_slice())
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(chunks.concat(), data);
        assert!(chunks.iter().all(|c| c.len() <= MAX_CHUNK));
    }
    assert_eq!(hash::file_hash(&[]), [0; 32]);
}
#[test]
fn official_796_chunk_merkle_and_file_hash_vectors() {
    let chunks: Vec<_> = include_str!("fixtures/official.chunks")
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            (
                hash::from_hex(fields.next().unwrap()).unwrap(),
                fields.next().unwrap().parse::<u64>().unwrap(),
            )
        })
        .collect();
    assert_eq!(chunks.len(), 796);
    assert_eq!(chunks.iter().map(|(_, n)| n).sum::<u64>(), 63527244);
    assert_eq!(
        hash::to_hex(&hash::xorb_hash(&chunks)),
        "eea25d6ee393ccae385820daed127b96ef0ea034dfb7cf6da3a950ce334b7632"
    );
    assert_eq!(
        hash::to_hex(&hash::file_hash(&chunks)),
        "118a53328412787fee04011dcf82fdc4acf3a4a1eddec341c910d30a306aaf97"
    );
    assert_eq!(
        hash::to_hex(&hash::verification_hash(
            &chunks.iter().map(|(h, _)| *h).collect::<Vec<_>>()
        )),
        "d81c11b1fc9bc2a25587108c675bbfe65ca2e5d350b0cd92c58329fcc8444178"
    );
}
#[test]
fn xet_hash_hex_endianness_and_validation() {
    let h = std::array::from_fn(|i| i as u8);
    let s = hash::to_hex(&h);
    assert_eq!(
        s,
        "07060504030201000f0e0d0c0b0a090817161514131211101f1e1d1c1b1a1918"
    );
    assert_eq!(hash::from_hex(&s).unwrap(), h);
    assert!(hash::from_hex(&"😅".repeat(16)).is_err());
    assert!(hash::from_hex(&"g".repeat(64)).is_err());
}
#[test]
fn xorb_codecs_roundtrip_odd_lengths_and_choose_compression() {
    for len in [1, 2, 3, 4, 5, 8191, 8192, 131071, 131072] {
        for data in [
            random(len),
            vec![0; len],
            (0..len)
                .map(|i| if i % 4 == 0 { (i / 4) as u8 } else { 0 })
                .collect(),
        ] {
            let record = xorb::encode(&data).unwrap();
            assert!(record.len() <= data.len() + 8);
            let (decoded, size) = xorb::decode(&mut record.as_slice()).unwrap().unwrap();
            assert_eq!(decoded, data);
            assert_eq!(size, record.len() as u64);
        }
    }
    assert_eq!(xorb::encode(&vec![0; 8192]).unwrap()[4], 1);
    let floats: Vec<_> = (0..8192u32)
        .flat_map(|i| (i as f32 * 0.001).to_le_bytes())
        .collect();
    assert_eq!(xorb::encode(&floats).unwrap()[4], 2);
}
#[test]
fn malformed_and_truncated_records_fail_without_unbounded_allocation() {
    assert!(xorb::decode(&mut [].as_slice()).unwrap().is_none());
    for data in [
        vec![1; 8],
        vec![0, 255, 255, 255, 1, 0, 0, 1],
        vec![0, 1, 0, 0, 3, 1, 0, 0],
        vec![0, 1, 0, 0, 0, 0, 0, 0],
    ] {
        assert!(xorb::decode(&mut data.as_slice()).is_err());
    }
    let record = xorb::encode(&random(8192)).unwrap();
    for len in [1, 7, 8, record.len() - 1] {
        assert!(xorb::decode(&mut &record[..len]).is_err());
    }
    assert!(xorb::encode(&[]).is_err());
    assert!(xorb::encode(&vec![0; MAX_CHUNK + 1]).is_err());
}

#[test]
fn official_sample_terminator_is_only_accepted_at_eof() {
    let mut record = xorb::encode(b"sample").unwrap();
    record.extend_from_slice(b"XETB");
    let mut input = record.as_slice();
    assert_eq!(xorb::decode(&mut input).unwrap().unwrap().0, b"sample");
    assert!(xorb::decode(&mut input).unwrap().is_none());
    assert!(xorb::decode(&mut b"XETBgarbage".as_slice()).is_err());
}
#[test]
fn insertion_preserves_most_chunk_identities() {
    let data = random(8 * 1024 * 1024);
    let original: std::collections::HashSet<_> = Chunker::new(data.as_slice())
        .map(|c| hash::chunk_hash(&c.unwrap()))
        .collect();
    let mut edited = data;
    edited.splice(123456..123456, b"inserted content".iter().copied());
    let chunks: Vec<_> = Chunker::new(edited.as_slice())
        .map(|c| hash::chunk_hash(&c.unwrap()))
        .collect();
    let shared = chunks.iter().filter(|h| original.contains(*h)).count();
    assert!(shared * 100 / chunks.len() > 95);
}
