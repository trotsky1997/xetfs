//! Hub CAS wire adapter over the existing XetFS chunks and immutable recipes.
use crate::{FileSystem, Repository, hash, xorb};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Cursor,
    path::Path,
};
use xet_core_structures::{
    merklehash::MerkleHash,
    metadata_shard::{
        file_structs::{FileDataSequenceHeader, MDBFileInfo},
        shard_format::{MDB_FILE_INFO_ENTRY_SIZE, MDBShardFileHeader, MDBShardInfo},
        shard_in_memory::MDBInMemoryShard,
        xorb_structs::{MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader},
    },
    xorb_object::XorbObject,
};

const LIMIT: u64 = 64 * 1024 * 1024;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn bounded(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::metadata(path)?.len() <= LIMIT + 1024 * 1024,
        "CAS object exceeds size limit"
    );
    Ok(fs::read(path)?)
}

pub fn import_xorb(fs: &mut FileSystem, source: &Path, id: &str) -> Result<Value> {
    let expected = hash::from_hex(id)?;
    let bytes = bounded(source)?;
    let footer =
        XorbObject::validate_xorb_object(&mut Cursor::new(&bytes), &MerkleHash::from(expected))
            .ok()
            .flatten();
    // XetFS's native uploader sends its verified chunk-record stream without a footer.
    let content_len = footer
        .as_ref()
        .map(|f| f.get_contents_length())
        .transpose()?
        .map(u64::from)
        .unwrap_or(bytes.len() as u64);
    ensure!(content_len <= LIMIT, "Xorb payload exceeds 64 MiB");
    let mut input = Cursor::new(&bytes[..content_len as usize]);
    let mut records = Vec::new();
    let mut raw = 0;
    while input.position() < content_len {
        let (data, _) = xorb::decode(&mut input)?.context("truncated Xorb")?;
        raw += data.len();
        ensure!(
            raw <= LIMIT as usize && records.len() < 8192,
            "Xorb expands beyond its bounds"
        );
        records.push(data);
    }
    ensure!(
        !records.is_empty() && input.position() == content_len,
        "invalid Xorb chunk stream"
    );
    let existed = fs
        .repository()
        .conn
        .query_row("SELECT 1 FROM xorbs WHERE id=?1", [id], |_| Ok(()))
        .optional()?
        .is_some();
    let (content, _) = fs.cas_repository_mut().import_cas_chunks(records, id)?;
    fs.mkdir_all("/hub/cas/staged")?;
    fs.pin_content(format!("/hub/cas/staged/{id}"), &content)?;
    Ok(json!({"was_inserted": !existed}))
}

fn bounded_shard(bytes: &[u8]) -> Result<(u64, Vec<MDBFileInfo>)> {
    ensure!(bytes.len() as u64 <= LIMIT, "shard exceeds 64 MiB");
    let mut reader = Cursor::new(bytes);
    MDBShardFileHeader::deserialize(&mut reader)?;
    let mut files = Vec::new();
    loop {
        let start = reader.position();
        ensure!(start + 48 <= bytes.len() as u64, "truncated file info");
        let header = FileDataSequenceHeader::deserialize(&mut reader)?;
        if header.is_bookend() {
            break;
        }
        let following = header.num_info_entry_following()? as u64;
        ensure!(
            header.num_entries <= 100000
                && start + 48 + following * MDB_FILE_INFO_ENTRY_SIZE as u64 <= bytes.len() as u64
                && files.len() < 20000,
            "invalid file info bounds"
        );
        reader.set_position(start);
        files.push(MDBFileInfo::deserialize(&mut reader)?.context("unexpected file terminator")?);
    }
    // CAS uploads contain the header and two terminated sections, without cache indexes/footer.
    Ok((reader.position(), files))
}

pub fn import_shard(fs: &mut FileSystem, source: &Path, allowed_file: &Path) -> Result<Value> {
    let allowed: std::collections::HashSet<String> =
        serde_json::from_slice(&fs::read(allowed_file)?)?;
    let bytes = bounded(source)?;
    let (xorb_info_offset, files) = bounded_shard(&bytes)?;
    let mut cursor = Cursor::new(&bytes);
    // Bound CAS blocks before their vector allocation.
    cursor.set_position(xorb_info_offset);
    let mut blocks = 0;
    loop {
        let start = cursor.position();
        ensure!(start + 48 <= bytes.len() as u64, "truncated CAS block");
        let header = XorbChunkSequenceHeader::deserialize(&mut cursor)?;
        if header.is_bookend() {
            break;
        }
        ensure!(
            header.num_entries <= 8192
                && start + 48 + header.num_entries as u64 * 48 <= bytes.len() as u64
                && blocks < 20000,
            "invalid CAS block bounds"
        );
        let id = header.xorb_hash.to_string();
        ensure!(allowed.contains(&id), "Xorb is outside the upload scope");
        let actual = fs.repository().read_upload_xorb(&id)?;
        ensure!(
            actual.chunks.len() == header.num_entries as usize
                && actual.raw_bytes == header.num_bytes_in_xorb as u64,
            "CAS block size mismatch"
        );
        for expected in &actual.chunks {
            let chunk = XorbChunkSequenceEntry::deserialize(&mut cursor)?;
            ensure!(
                <[u8; 32]>::from(chunk.chunk_hash) == expected.hash
                    && chunk.chunk_byte_range_start as u64 == expected.offset
                    && chunk.unpacked_segment_bytes as u64 == expected.len,
                "CAS block chunk mismatch"
            );
        }
        blocks += 1;
    }
    let mut prepared = Vec::new();
    for file in files {
        ensure!(
            file.verification.len() == file.segments.len() && file.metadata_ext.is_some(),
            "Hub files require verification entries and SHA-256 metadata"
        );
        let mut recipe = Vec::new();
        let mut sha = Sha256::new();
        let mut total = 0u64;
        for (term, proof) in file.segments.iter().zip(&file.verification) {
            ensure!(
                allowed.contains(&term.xorb_hash.to_string()),
                "reconstruction Xorb is outside the upload scope"
            );
            let object = fs
                .repository()
                .read_upload_xorb(&term.xorb_hash.to_string())?;
            let start = term.chunk_index_start as usize;
            let end = term.chunk_index_end as usize;
            ensure!(
                start < end && end <= object.chunks.len(),
                "invalid reconstruction chunk range"
            );
            let hashes: Vec<_> = object.chunks[start..end].iter().map(|c| c.hash).collect();
            ensure!(
                hash::verification_hash(&hashes) == <[u8; 32]>::from(proof.range_hash),
                "term verification mismatch"
            );
            let mut reader = Cursor::new(&object.bytes);
            let mut length = 0u64;
            for (i, c) in object.chunks.iter().enumerate().take(end) {
                let (data, _) = xorb::decode(&mut reader)?.context("truncated stored Xorb")?;
                if i >= start {
                    sha.update(&data);
                    recipe.push((c.hash, c.len));
                    length += c.len;
                }
            }
            ensure!(
                length == term.unpacked_segment_bytes as u64,
                "term unpacked length mismatch"
            );
            total = total.checked_add(length).context("file length overflow")?;
            ensure!(total <= 1024u64.pow(4), "file exceeds 1 TiB");
        }
        ensure!(
            hash::file_hash(&recipe) == <[u8; 32]>::from(file.metadata.file_hash),
            "file Merkle hash mismatch"
        );
        let actual_sha = sha.finalize();
        ensure!(
            hex(&actual_sha) == file.metadata_ext.unwrap().sha256.to_string(),
            "file SHA-256 mismatch"
        );
        prepared.push((recipe, hex(&actual_sha)));
    }
    // Validation completes for the whole shard before exposing any file pins.
    fs.mkdir_all("/hub/cas/files")?;
    let mut output = Vec::new();
    for (recipe, sha) in prepared {
        let (id, size) = fs.cas_repository_mut().register_cas_recipe(&recipe)?;
        let path = format!("/hub/cas/files/{sha}");
        fs.pin_content(&path, &id)?;
        output.push(json!({"sha256":sha,"file_hash":id,"size":size,"path":path}));
    }
    Ok(json!({"result":1,"files":output}))
}

pub fn describe(repo: &Repository, id: &str) -> Result<Value> {
    hash::from_hex(id)?;
    let recipe = repo.reconstruction(id)?;
    let mut objects = serde_json::Map::new();
    for term in &recipe.terms {
        if objects.contains_key(&term.xorb) {
            continue;
        }
        let object = repo.read_upload_xorb(&term.xorb)?;
        let mut cursor = Cursor::new(&object.bytes);
        let mut chunks = Vec::new();
        for chunk in object.chunks {
            let start = cursor.position();
            let (_, stored) = xorb::decode(&mut cursor)?.context("truncated Xorb")?;
            chunks.push(json!({"start":start,"end":start+stored-1,"size":chunk.len}));
        }
        objects.insert(
            term.xorb.clone(),
            json!({"size":object.bytes.len(),"chunks":chunks}),
        );
    }
    Ok(
        json!({"file_hash":recipe.file_hash,"size":recipe.size,"terms":recipe.terms,"xorbs":objects}),
    )
}

pub fn export_xorb(repo: &Repository, id: &str, destination: &Path) -> Result<Value> {
    hash::from_hex(id)?;
    let object = repo.read_upload_xorb(id)?;
    fs::write(destination, &object.bytes)?;
    Ok(json!({"size":object.bytes.len()}))
}

pub fn dedupe(repo: &Repository, chunk: &str, destination: &Path) -> Result<Value> {
    hash::from_hex(chunk)?;
    let id: String =
        repo.conn
            .query_row("SELECT xorb FROM chunks WHERE hash=?1", [chunk], |r| {
                r.get(0)
            })?;
    let object = repo.read_upload_xorb(&id)?;
    let mut header = XorbChunkSequenceHeader::new(
        MerkleHash::from(object.hash),
        object.chunks.len() as u32,
        object.raw_bytes as u32,
    );
    header.num_bytes_on_disk = object.bytes.len() as u32;
    let block = MDBXorbInfo {
        metadata: header,
        chunks: object
            .chunks
            .iter()
            .map(|c| {
                XorbChunkSequenceEntry::new(MerkleHash::from(c.hash), c.len as u32, c.offset as u32)
            })
            .collect(),
    };
    let mut shard = MDBInMemoryShard::default();
    shard.add_xorb_block(block)?;
    MDBShardInfo::serialize_from(&mut File::create(destination)?, &shard, None)?;
    Ok(json!({"xorb":id}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xet_core_structures::metadata_shard::file_structs::{
        FileDataSequenceEntry, FileMetadataExt, FileVerificationEntry,
    };
    #[test]
    fn native_xorbs_shard_proofs_scope_and_cold_reconstruction() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut source = FileSystem::new(Repository::init(dir.path().join("source"))?);
        let data: Vec<u8> = (0..400000)
            .map(|i| ((i * 31 + i / 177) % 251) as u8)
            .collect();
        let node = source.put_file("/seed", Cursor::new(&data))?;
        let file_hash = node.content.unwrap();
        let recipe = source.repository().reconstruction(&file_hash)?;
        let mut target = FileSystem::new(Repository::init(dir.path().join("target"))?);
        let mut allowed = Vec::new();
        let mut shard = MDBInMemoryShard::default();
        for term in &recipe.terms {
            if allowed.contains(&term.xorb) {
                continue;
            }
            let object = source.repository().read_upload_xorb(&term.xorb)?;
            let wire = dir.path().join("wire");
            fs::write(&wire, &object.bytes)?;
            assert!(
                import_xorb(&mut target, &wire, &term.xorb)?["was_inserted"]
                    .as_bool()
                    .unwrap()
            );
            assert!(
                !import_xorb(&mut target, &wire, &term.xorb)?["was_inserted"]
                    .as_bool()
                    .unwrap()
            );
            assert!(import_xorb(&mut target, &wire, &"0".repeat(64)).is_err());
            let mut header = XorbChunkSequenceHeader::new(
                MerkleHash::from(object.hash),
                object.chunks.len() as u32,
                object.raw_bytes as u32,
            );
            header.num_bytes_on_disk = object.bytes.len() as u32;
            shard.add_xorb_block(MDBXorbInfo {
                metadata: header,
                chunks: object
                    .chunks
                    .iter()
                    .map(|c| {
                        XorbChunkSequenceEntry::new(
                            MerkleHash::from(c.hash),
                            c.len as u32,
                            c.offset as u32,
                        )
                    })
                    .collect(),
            })?;
            allowed.push(term.xorb.clone());
        }
        let info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(
                MerkleHash::from(hash::from_hex(&file_hash)?),
                recipe.terms.len() as u32,
                true,
                true,
            ),
            segments: recipe
                .terms
                .iter()
                .map(|t| {
                    FileDataSequenceEntry::new(
                        MerkleHash::from(hash::from_hex(&t.xorb).unwrap()),
                        t.size as u32,
                        t.chunk_start as u32,
                        t.chunk_end as u32,
                    )
                })
                .collect(),
            verification: recipe
                .terms
                .iter()
                .map(|t| {
                    FileVerificationEntry::new(MerkleHash::from(
                        hash::from_hex(&t.verification_hash).unwrap(),
                    ))
                })
                .collect(),
            metadata_ext: Some(FileMetadataExt::new(MerkleHash::from_be_bytes(
                <[u8; 32]>::from(Sha256::digest(&data)),
            ))),
        };
        shard.add_file_reconstruction_info(info)?;
        let wire = dir.path().join("shard");
        MDBShardInfo::serialize_from(&mut File::create(&wire)?, &shard, None)?;
        let scope = dir.path().join("scope");
        fs::write(&scope, "[]")?;
        assert!(import_shard(&mut target, &wire, &scope).is_err());
        fs::write(&scope, serde_json::to_vec(&allowed)?)?;
        let mut bad = shard.clone();
        bad.file_content.values_mut().next().unwrap().verification[0].range_hash =
            MerkleHash::default();
        let bad_wire = dir.path().join("bad");
        MDBShardInfo::serialize_from(&mut File::create(&bad_wire)?, &bad, None)?;
        assert!(import_shard(&mut target, &bad_wire, &scope).is_err());
        let result = import_shard(&mut target, &wire, &scope)?;
        assert_eq!(result["files"][0]["sha256"], hex(&Sha256::digest(&data)));
        let mut recovered = Vec::new();
        target
            .repository()
            .export_content(&file_hash, &mut recovered)?;
        assert_eq!(recovered, data);
        drop(target);
        let reopened = Repository::open(dir.path().join("target"))?;
        assert_eq!(reopened.reconstruction(&file_hash)?.size, data.len() as u64);
        Ok(())
    }
}
