//! Content-addressed chunks for remote sets.
//!
//! A remote set is `sets/<set id>/complete.json` and `sets/<set id>/index.json.age`.
//! The index, sealed with the set's own identity, holds the set's manifest
//! and, for each artifact, the ordered chunks of its stored stream. The
//! stored stream is the artifact's plaintext, except that a zstd project
//! archive is stored as its tar: compressing the whole archive would change
//! every byte after an edit, so no chunk would match the previous run.
//!
//! Cut points come from FastCDC, so an insertion changes only the chunks
//! around it. A chunk is stored once at `chunks/<key id>/<id>` as
//! age(zstd(bytes)), so each master key only ever reads, checks, or sweeps
//! its own chunks.
//! Its id is an HMAC-SHA-256 of its bytes keyed from `SECRET_KEY`: a plain
//! hash would let anyone who can list the bucket test whether it holds a
//! known file. Chunks use their own key, derived like a set's identity with
//! another purpose, because every set shares them.

use std::io::{BufReader, Read, Write};
use std::path::Path;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{Artifact, BackupError, Manifest, Sealed};
use crate::credentials::SecretKey;

pub const INDEX: &str = "index.json.age";
pub const INDEX_VERSION: u32 = 1;
pub const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
pub const CHUNKS: &str = "chunks/";
const MIN_CHUNK: usize = 1024 * 1024;
const AVG_CHUNK: usize = 4 * 1024 * 1024;
pub const MAX_CHUNK: usize = 16 * 1024 * 1024;
/// Ciphertext bound of one chunk: zstd's worst case, age's 16 bytes per
/// 64 KiB, and its header.
pub const MAX_SEALED_CHUNK: u64 =
    (MAX_CHUNK + MAX_CHUNK / 128 + MAX_CHUNK / 4096 + 64 * 1024) as u64;
const LEVEL: i32 = 3;
const CHUNK_PURPOSE: &str = "else.bot/backups/chunks/v1";
const CHUNK_ID_PURPOSE: &[u8] = b"else.bot/backups/chunk-id/v1";

/// How an artifact's stored stream relates to its plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encoding {
    /// The plaintext itself.
    Plain,
    /// The tar inside a zstd project archive, compressed again on restore.
    ZstdTar,
}

impl Encoding {
    pub fn of(artifact: &Artifact, schema_version: u32) -> Self {
        if artifact.kind == "project"
            && super::projects::Format::of(schema_version) == super::projects::Format::TarZstd
        {
            Self::ZstdTar
        } else {
            Self::Plain
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkRef {
    pub id: String,
    pub bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    pub object: String,
    pub encoding: Encoding,
    pub bytes: u64,
    pub sha256: String,
    pub chunks: Vec<ChunkRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub index_version: u32,
    pub manifest: Manifest,
    /// One entry per manifest artifact, in the same order.
    pub artifacts: Vec<Stored>,
}

impl Index {
    /// Parse a decrypted index and check it describes `set_id`'s manifest.
    pub fn parse(plain: &[u8], completion: &super::Completion) -> Result<Self, BackupError> {
        let index: Index = serde_json::from_slice(plain)
            .map_err(|e| BackupError::Invalid(format!("chunk index: {e}")))?;
        let manifest = &index.manifest;
        let matches = index.index_version == INDEX_VERSION
            && manifest.set_id == completion.set_id
            && manifest.schema_version == completion.schema_version
            && manifest.encryption == completion.encryption
            && manifest.artifacts.len() == index.artifacts.len()
            && manifest
                .artifacts
                .iter()
                .zip(&index.artifacts)
                .all(|(a, s)| {
                    a.object == s.object && Encoding::of(a, manifest.schema_version) == s.encoding
                })
            && manifest
                .artifacts
                .iter()
                .zip(&index.artifacts)
                .all(|(a, s)| {
                    let plain = s.encoding != Encoding::Plain
                        || (s.bytes, &s.sha256) == (a.plaintext_bytes, &a.plaintext_sha256);
                    plain
                        && s.chunks.iter().all(|c| {
                            valid_id(&c.id) && c.bytes > 0 && c.bytes as usize <= MAX_CHUNK
                        })
                        && s.chunks.iter().map(|c| c.bytes as u64).sum::<u64>() == s.bytes
                });
        if !matches {
            return Err(BackupError::Invalid(
                "the chunk index does not match its set".into(),
            ));
        }
        Ok(index)
    }

    pub fn chunk_ids(&self) -> impl Iterator<Item = &str> {
        self.artifacts
            .iter()
            .flat_map(|s| s.chunks.iter().map(|c| c.id.as_str()))
    }
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The keys every chunk of one master key shares.
pub struct ChunkKeys {
    identity: age::x25519::Identity,
    mac: Zeroizing<[u8; 32]>,
    key_id: String,
}

impl ChunkKeys {
    pub fn derive(key: &SecretKey) -> Result<Self, BackupError> {
        let identity = super::identity_for_purpose(key, &[0u8; 32], CHUNK_PURPOSE)?;
        let mut mac = Zeroizing::new([0u8; 32]);
        hkdf::Hkdf::<sha2_010::Sha256>::new(None, key.bytes())
            .expand(CHUNK_ID_PURPOSE, mac.as_mut())
            .map_err(|_| BackupError::Crypto("HKDF output length rejected".into()))?;
        Ok(Self {
            identity,
            mac,
            key_id: super::key_id(key),
        })
    }

    /// Where this key's chunks live, relative to the bucket prefix.
    pub fn root(&self) -> String {
        format!("{CHUNKS}{}/", self.key_id)
    }

    pub fn id(&self, plain: &[u8]) -> String {
        let mut mac =
            Hmac::<sha2_010::Sha256>::new_from_slice(self.mac.as_ref()).expect("any key length");
        mac.update(plain);
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, BackupError> {
        let compressed = zstd::stream::encode_all(plain, LEVEL)
            .map_err(|e| BackupError::Crypto(e.to_string()))?;
        encrypt(&self.identity, &compressed)
    }

    /// Decrypt and decompress a chunk, accepting it only if its bytes have
    /// the id it is stored under.
    pub fn open(&self, id: &str, sealed: &[u8]) -> Result<Vec<u8>, BackupError> {
        let damaged = || BackupError::Invalid(format!("chunk {id} is damaged"));
        let decryptor = age::Decryptor::new(sealed).map_err(|_| damaged())?;
        let reader = decryptor
            .decrypt(std::iter::once(&self.identity as &dyn age::Identity))
            .map_err(|_| damaged())?;
        let mut plain = Vec::new();
        zstd::stream::read::Decoder::new(reader)
            .and_then(|d| d.take(MAX_CHUNK as u64 + 1).read_to_end(&mut plain))
            .map_err(|_| damaged())?;
        if plain.len() > MAX_CHUNK || self.id(&plain) != id {
            return Err(damaged());
        }
        Ok(plain)
    }
}

/// Encrypt a small object whole.
pub fn encrypt(identity: &age::x25519::Identity, plain: &[u8]) -> Result<Vec<u8>, BackupError> {
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|e| BackupError::Crypto(e.to_string()))?;
    let mut out = Vec::with_capacity(plain.len() + plain.len() / 4096 + 256);
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    writer
        .write_all(plain)
        .and_then(|_| writer.finish().map(|_| ()))
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    Ok(out)
}

struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> Hashing<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn matches(&self, bytes: u64, sha256: &str) -> bool {
        self.bytes == bytes && hex::encode(self.hasher.clone().finalize()) == sha256
    }

    fn digest(&self) -> String {
        hex::encode(self.hasher.clone().finalize())
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

/// Read a local artifact's stored stream with `f`. The whole artifact is
/// read and checked against its manifest entry before the result is used.
pub fn read_stored<T>(
    set_dir: &Path,
    artifact: &Artifact,
    identity: &age::x25519::Identity,
    encoding: Encoding,
    f: impl FnOnce(&mut dyn Read) -> Result<T, BackupError>,
) -> Result<T, BackupError> {
    let path = set_dir.join(&artifact.object);
    let io = |e: std::io::Error| BackupError::io(path.display(), e);
    let file = std::fs::File::open(&path).map_err(io)?;
    let reader = age::Decryptor::new(BufReader::new(file))
        .and_then(|d| d.decrypt(std::iter::once(identity as &dyn age::Identity)))
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    let mut plain = Hashing::new(reader);
    let out = match encoding {
        Encoding::Plain => {
            let out = f(&mut plain)?;
            std::io::copy(&mut plain, &mut std::io::sink()).map_err(io)?;
            out
        }
        Encoding::ZstdTar => {
            let mut tar = super::projects::decompress(&mut plain).map_err(io)?;
            let out = f(&mut tar)?;
            std::io::copy(&mut tar, &mut std::io::sink()).map_err(io)?;
            drop(tar);
            std::io::copy(&mut plain, &mut std::io::sink()).map_err(io)?;
            out
        }
    };
    if !plain.matches(artifact.plaintext_bytes, &artifact.plaintext_sha256) {
        return Err(BackupError::Invalid(format!(
            "the local copy of {} does not match its manifest",
            artifact.object
        )));
    }
    Ok(out)
}

/// Cut a stored stream into chunks, calling `each` with every chunk's id
/// and bytes in order. Returns the chunk list, length, and SHA-256.
pub fn split(
    keys: &ChunkKeys,
    input: &mut dyn Read,
    mut each: impl FnMut(&str, &[u8]) -> Result<(), BackupError>,
) -> Result<(Vec<ChunkRef>, u64, String), BackupError> {
    let mut hashing = Hashing::new(input);
    let mut chunks = Vec::new();
    for chunk in fastcdc::v2020::StreamCDC::new(&mut hashing, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
        let chunk = chunk.map_err(|e| BackupError::Io(format!("reading for chunks: {e}")))?;
        let id = keys.id(&chunk.data);
        each(&id, &chunk.data)?;
        chunks.push(ChunkRef {
            id,
            bytes: chunk.length as u32,
        });
    }
    Ok((chunks, hashing.bytes, hashing.digest()))
}

struct Joined<I> {
    chunks: I,
    current: std::io::Cursor<Vec<u8>>,
}

impl<I: Iterator<Item = Result<Vec<u8>, BackupError>>> Read for Joined<I> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.current.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            match self.chunks.next() {
                Some(Ok(bytes)) => self.current = std::io::Cursor::new(bytes),
                Some(Err(err)) => return Err(std::io::Error::other(err.to_string())),
                None => return Ok(0),
            }
        }
    }
}

/// Rebuild an artifact from its chunks' bytes, in order, and seal it into
/// a new file at `target` with at most `limit` plaintext bytes. The stored
/// stream must match the index before the result is used.
pub fn rebuild(
    target: &Path,
    identity: &age::x25519::Identity,
    stored: &Stored,
    chunks: impl Iterator<Item = Result<Vec<u8>, BackupError>>,
    limit: u64,
) -> Result<Sealed, BackupError> {
    let mut joined = Hashing::new(Joined {
        chunks,
        current: std::io::Cursor::new(Vec::new()),
    });
    let sealed = match stored.encoding {
        Encoding::Plain => super::seal(target, identity, &mut joined, limit)?,
        Encoding::ZstdTar => {
            let mut archive = super::projects::compress(&mut joined)
                .map_err(|e| BackupError::io(target.display(), e))?;
            super::seal(target, identity, &mut archive, limit)?
        }
    };
    if !joined.matches(stored.bytes, &stored.sha256) {
        let _ = std::fs::remove_file(target);
        return Err(BackupError::Invalid(format!(
            "the rebuilt {} does not match its chunk index",
            stored.object
        )));
    }
    Ok(sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SecretKey {
        use base64::Engine;
        SecretKey::parse(&base64::engine::general_purpose::STANDARD.encode([byte; 32])).unwrap()
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn ids(keys: &ChunkKeys, data: &[u8]) -> Vec<String> {
        split(keys, &mut &data[..], |_, _| Ok(()))
            .unwrap()
            .0
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn an_insertion_changes_only_the_chunks_around_it() {
        let keys = ChunkKeys::derive(&key(1)).unwrap();
        let before = noise(40 << 20, 7);
        let mut after = before.clone();
        after.splice(20 << 20..20 << 20, b"inserted".iter().copied());
        let (a, b) = (ids(&keys, &before), ids(&keys, &after));
        let shared = b.iter().filter(|id| a.contains(id)).count();
        assert!(a.len() >= 5, "{} chunks", a.len());
        assert!(
            shared + 2 >= b.len(),
            "{shared} of {} chunks reused",
            b.len()
        );
    }

    #[test]
    fn chunk_ids_are_keyed_and_chunks_open_only_under_their_id() {
        let (one, two) = (
            ChunkKeys::derive(&key(1)).unwrap(),
            ChunkKeys::derive(&key(2)).unwrap(),
        );
        let data = noise(100_000, 3);
        let id = one.id(&data);
        assert_ne!(id, two.id(&data));
        assert_ne!(id, super::super::sha256_hex(&data));
        assert!(valid_id(&id));
        let sealed = one.seal(&data).unwrap();
        assert_eq!(one.open(&id, &sealed).unwrap(), data);
        assert!(two.open(&id, &sealed).is_err(), "another key");
        assert!(one.open(&one.id(b"other"), &sealed).is_err(), "another id");
        let mut flipped = sealed.clone();
        let at = flipped.len() - 20;
        flipped[at] ^= 1;
        assert!(one.open(&id, &flipped).is_err(), "damaged");
    }

    #[test]
    fn stored_streams_rebuild_into_sealed_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let keys = ChunkKeys::derive(&key(1)).unwrap();
        let identity = super::super::identity(&key(1), &[5; 32]).unwrap();
        let tar = noise(9 << 20, 11);
        let mut archive = Vec::new();
        super::super::projects::compress(&tar[..])
            .unwrap()
            .read_to_end(&mut archive)
            .unwrap();
        for (encoding, plain) in [(Encoding::Plain, tar.clone()), (Encoding::ZstdTar, archive)] {
            let object = format!("{encoding:?}.age");
            let sealed = super::super::seal(
                &dir.path().join(&object),
                &identity,
                &mut &plain[..],
                1 << 30,
            )
            .unwrap();
            let artifact = Artifact {
                kind: if encoding == Encoding::Plain {
                    "sqlite"
                } else {
                    "project"
                }
                .into(),
                object: object.clone(),
                bot_id: None,
                ciphertext_bytes: sealed.ciphertext_bytes,
                ciphertext_sha256: sealed.ciphertext_sha256,
                plaintext_bytes: sealed.plaintext_bytes,
                plaintext_sha256: sealed.plaintext_sha256,
            };
            let mut store = std::collections::HashMap::new();
            let (chunks, bytes, sha256) =
                read_stored(dir.path(), &artifact, &identity, encoding, |input| {
                    split(&keys, input, |id, data| {
                        store.insert(id.to_owned(), keys.seal(data)?);
                        Ok(())
                    })
                })
                .unwrap();
            assert_eq!(
                (bytes, &sha256),
                (tar.len() as u64, &super::super::sha256_hex(&tar))
            );
            let stored = Stored {
                object,
                encoding,
                bytes,
                sha256,
                chunks,
            };
            let target = dir.path().join(format!("rebuilt-{encoding:?}.age"));
            let fetch = stored
                .chunks
                .iter()
                .map(|c| keys.open(&c.id, &store[&c.id]));
            let rebuilt = rebuild(&target, &identity, &stored, fetch, 1 << 30).unwrap();
            let opened = Artifact {
                object: target.file_name().unwrap().to_str().unwrap().into(),
                ciphertext_bytes: rebuilt.ciphertext_bytes,
                ciphertext_sha256: rebuilt.ciphertext_sha256,
                plaintext_bytes: rebuilt.plaintext_bytes,
                plaintext_sha256: rebuilt.plaintext_sha256,
                ..artifact.clone()
            };
            let again = read_stored(dir.path(), &opened, &identity, encoding, |input| {
                let mut out = Vec::new();
                input
                    .read_to_end(&mut out)
                    .map_err(|e| BackupError::Io(e.to_string()))?;
                Ok(out)
            })
            .unwrap();
            assert_eq!(again, tar, "{encoding:?}");
            let mut short = stored.clone();
            short.chunks.pop();
            let fetch = short.chunks.iter().map(|c| keys.open(&c.id, &store[&c.id]));
            let missing = dir.path().join(format!("short-{encoding:?}.age"));
            assert!(rebuild(&missing, &identity, &stored, fetch, 1 << 30).is_err());
            assert!(!missing.exists());
        }
    }
}
