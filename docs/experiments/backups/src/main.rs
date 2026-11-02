use bech32::ToBase32;
use hkdf::Hkdf;
use sha2::Sha256;
use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BACKUP_LABEL: &[u8] = b"else.bot/backups/v1";

fn identity(master: &[u8], salt: &[u8], label: &[u8]) -> Result<age::x25519::Identity> {
    if master.len() != 32 || salt.len() != 32 {
        return Err("expected synthetic 32-byte master and salt".into());
    }
    let mut seed = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), master)
        .expand(label, &mut seed)
        .map_err(|_| "HKDF output length rejected")?;
    let encoded = bech32::encode("age-secret-key-", seed.to_base32(), bech32::Variant::Bech32)?;
    encoded.to_uppercase().parse().map_err(Into::into)
}

fn stream_file(
    mode: &str,
    key: &age::x25519::Identity,
    source: &Path,
    target: &Path,
) -> Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)?;
    let result = (|| -> Result<()> {
        match mode {
            "encrypt" => {
                let recipient = key.to_public();
                let encryptor = age::Encryptor::with_recipients(std::iter::once(
                    &recipient as &dyn age::Recipient,
                ))?;
                let mut writer = encryptor.wrap_output(&mut output)?;
                io::copy(&mut input, &mut writer)?;
                writer.finish()?;
            }
            "decrypt" => {
                let decryptor = age::Decryptor::new(input)?;
                let mut reader = decryptor.decrypt(std::iter::once(key as &dyn age::Identity))?;
                io::copy(&mut reader, &mut output)?;
            }
            _ => return Err("expected encrypt or decrypt".into()),
        }
        output.sync_all()?;
        Ok(())
    })();
    drop(output);
    if result.is_err() {
        fs::remove_file(target)?;
    }
    result
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().collect();
    if args.len() != 6 {
        return Err(
            "usage: fixture encrypt|decrypt|key-check synthetic-master salt input output".into(),
        );
    }
    let master = fs::read(&args[2])?;
    let salt = fs::read(&args[3])?;
    let key = identity(&master, &salt, BACKUP_LABEL)?;
    if args[1] == "key-check" {
        assert_eq!(
            key.to_public(),
            identity(&master, &salt, BACKUP_LABEL)?.to_public()
        );
        assert_ne!(
            key.to_public(),
            identity(&master, &salt, b"else.bot/secrets/v1")?.to_public()
        );
        let mut other_salt = salt.clone();
        other_salt[0] ^= 1;
        assert_ne!(
            key.to_public(),
            identity(&master, &other_salt, BACKUP_LABEL)?.to_public()
        );
        println!("HKDF determinism, purpose separation, and salt separation passed");
        return Ok(());
    }
    stream_file(
        args[1].to_str().ok_or("invalid mode")?,
        &key,
        Path::new(&args[4]),
        Path::new(&args[5]),
    )
}
