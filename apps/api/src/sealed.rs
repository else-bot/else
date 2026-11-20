//! Secrets sealed at rest with age, to a key derived from the installation's
//! `SECRET_KEY`, a fresh random salt, and a purpose string. The purpose binds
//! a ciphertext to what it belongs to, so changing a purpose's bytes makes
//! every stored secret under it unreadable.

use std::io::{Read, Write};

use zeroize::Zeroizing;

use crate::credentials::SecretKey;

/// A secret could not be sealed or opened. Deliberately carries no detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a secret could not be encrypted or decrypted")
    }
}

impl std::error::Error for Error {}

pub struct Sealed {
    pub salt: [u8; 32],
    pub cipher: Vec<u8>,
}

fn identity(
    key: &SecretKey,
    salt: &[u8; 32],
    purpose: &str,
) -> Result<age::x25519::Identity, Error> {
    crate::backup::identity_for_purpose(key, salt, purpose).map_err(|_| Error)
}

pub fn seal(key: &SecretKey, purpose: &str, plain: &[u8]) -> Result<Sealed, Error> {
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).map_err(|_| Error)?;
    let recipient = identity(key, &salt, purpose)?.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|_| Error)?;
    let mut cipher = Vec::new();
    let mut writer = encryptor.wrap_output(&mut cipher).map_err(|_| Error)?;
    writer
        .write_all(plain)
        .and_then(|()| writer.finish().map(drop))
        .map_err(|_| Error)?;
    Ok(Sealed { salt, cipher })
}

pub fn open(
    key: &SecretKey,
    purpose: &str,
    salt: &[u8],
    cipher: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let salt: [u8; 32] = salt.try_into().map_err(|_| Error)?;
    let identity = identity(key, &salt, purpose)?;
    let decryptor = age::Decryptor::new(cipher).map_err(|_| Error)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| Error)?;
    let mut plain = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plain).map_err(|_| Error)?;
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_only_under_the_purpose_it_was_sealed_for() {
        let key = SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
        let sealed = seal(&key, "else.bot/test/v1", b"hunter2").unwrap();
        let plain = open(&key, "else.bot/test/v1", &sealed.salt, &sealed.cipher).unwrap();
        assert_eq!(plain.as_slice(), b"hunter2");
        assert!(open(&key, "else.bot/test/v2", &sealed.salt, &sealed.cipher).is_err());
    }
}
