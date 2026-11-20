//! Secure storage of remote-host passwords in the OS keychain (Secret Service on
//! Linux, Keychain on macOS, Credential Manager on Windows) via the `keyring`
//! crate. Passwords are never written to muxel's config or workspace files —
//! only a reference (the host id) is, and the secret is fetched on demand.
//!
//! The keychain is one per OS user, not per muxel setup, so a muxel started with
//! `--user-data-dir DIR` keeps its entries under a service of its own, named from
//! DIR, and never sees or changes the default setup's (or another DIR's). Without
//! a user data dir the service is plain `muxel`, as it always was.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::OnceLock;
use uuid::Uuid;

const SERVICE: &str = "muxel";

/// The keychain service for this process's setup.
fn service() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let dir = std::env::var_os(muxel_store::USER_DATA_DIR_ENV).filter(|d| !d.is_empty());
        if let Some(dir) = &dir {
            // It must exist to have a canonical path, the same on every run; muxel
            // keeps its settings there anyway.
            let _ = std::fs::create_dir_all(dir);
        }
        service_for(dir.as_deref().map(Path::new))
    })
}

/// `muxel` without a user data dir; with one, `muxel-` and a hash of its
/// canonical path, so every spelling of one directory shares its entries.
fn service_for(user_data_dir: Option<&Path>) -> String {
    let Some(dir) = user_data_dir else {
        return SERVICE.to_string();
    };
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    format!(
        "{SERVICE}-{:016x}",
        fnv1a(dir.as_os_str().as_encoded_bytes())
    )
}

/// 64-bit FNV-1a: stable across Rust releases, unlike `DefaultHasher`, so the
/// name a key was saved under never changes.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn entry(host_id: Uuid) -> Result<keyring::Entry> {
    keyring::Entry::new(service(), &format!("remote:{host_id}")).context("open keychain entry")
}

/// Store (or replace) the SSH password for a remote host.
pub fn set_remote_password(host_id: Uuid, password: &str) -> Result<()> {
    entry(host_id)?
        .set_password(password)
        .context("save password to keychain")
}

/// Fetch the stored SSH password for a remote host, if any.
pub fn get_remote_password(host_id: Uuid) -> Option<String> {
    entry(host_id).ok()?.get_password().ok()
}

/// Whether a password is stored for this host.
pub fn has_remote_password(host_id: Uuid) -> bool {
    get_remote_password(host_id).is_some()
}

/// Remove the stored password for a remote host (best-effort; a missing entry is
/// not an error).
pub fn delete_remote_password(host_id: Uuid) -> Result<()> {
    match entry(host_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete password from keychain"),
    }
}

// --- Shared login identities ---------------------------------------------------
// Identities keep their password in a distinct keychain namespace (`identity:{id}`)
// so a host's inline secret and an identity's shared secret never collide.

fn identity_entry(identity_id: Uuid) -> Result<keyring::Entry> {
    keyring::Entry::new(service(), &format!("identity:{identity_id}"))
        .context("open keychain entry")
}

/// Store (or replace) the SSH password for a login identity.
pub fn set_identity_password(identity_id: Uuid, password: &str) -> Result<()> {
    identity_entry(identity_id)?
        .set_password(password)
        .context("save password to keychain")
}

/// Fetch the stored SSH password for a login identity, if any.
pub fn get_identity_password(identity_id: Uuid) -> Option<String> {
    identity_entry(identity_id).ok()?.get_password().ok()
}

/// Whether a password is stored for this identity.
pub fn has_identity_password(identity_id: Uuid) -> bool {
    get_identity_password(identity_id).is_some()
}

/// Remove the stored password for a login identity (best-effort).
pub fn delete_identity_password(identity_id: Uuid) -> Result<()> {
    match identity_entry(identity_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete password from keychain"),
    }
}

// --- Speech-to-text provider API key -------------------------------------------
// A single shared key (there's one provider at a time), namespaced `stt:provider`.

fn stt_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(service(), "stt:provider").context("open keychain entry")
}

/// Store (or replace) the speech-to-text provider API key.
pub fn set_stt_api_key(key: &str) -> Result<()> {
    stt_entry()?
        .set_password(key)
        .context("save API key to keychain")
}

/// Fetch the stored speech-to-text provider API key, if any.
pub fn get_stt_api_key() -> Option<String> {
    stt_entry().ok()?.get_password().ok()
}

/// Whether a provider API key is stored.
pub fn has_stt_api_key() -> bool {
    get_stt_api_key().is_some_and(|k| !k.is_empty())
}

/// Remove the stored provider API key (best-effort).
pub fn delete_stt_api_key() -> Result<()> {
    match stt_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete API key from keychain"),
    }
}

// --- muxel Cloud ---------------------------------------------------------------
// `cloud:token` is the device token; `cloud:auk` is the base64 Account Unlock
// Key that decrypts the account's private keys. Neither is ever written to a
// file. Under `--user-data-dir` they live in that profile's own service.

fn cloud_entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service(), &format!("cloud:{name}")).context("open keychain entry")
}

pub fn set_cloud_secret(name: &str, value: &str) -> Result<()> {
    cloud_entry(name)?
        .set_password(value)
        .context("save to keychain")
}

/// A Cloud secret. Tells "not there" (`Ok(None)`) from "the
/// keychain couldn't be read" (`Err`, e.g. a locked login keyring), which
/// must never be mistaken for being signed out.
pub fn read_cloud_secret(name: &str) -> Result<Option<String>> {
    match cloud_entry(name)?.get_password() {
        Ok(v) if v.is_empty() => Ok(None),
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("read from keychain"),
    }
}

/// Check the keychain can store and return a secret, before creating
/// anything that depends on it.
pub fn probe_cloud_keychain() -> Result<()> {
    let value = uuid::Uuid::new_v4().to_string();
    set_cloud_secret("probe", &value)?;
    let back = read_cloud_secret("probe")?;
    let _ = delete_cloud_secret("probe");
    anyhow::ensure!(
        back.as_deref() == Some(value.as_str()),
        "the keychain didn't return what was saved"
    );
    Ok(())
}

pub fn delete_cloud_secret(name: &str) -> Result<()> {
    match cloud_entry(name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("delete from keychain"),
    }
}

#[cfg(test)]
mod tests {
    use super::{SERVICE, fnv1a, service_for};

    #[test]
    fn no_user_data_dir_keeps_the_service_existing_keys_use() {
        assert_eq!(service_for(None), "muxel");
    }

    #[test]
    fn each_user_data_dir_gets_a_service_of_its_own() {
        let root = std::env::temp_dir().join(format!("muxel-secrets-{}", uuid::Uuid::new_v4()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let service = service_for(Some(&a));
        assert_ne!(
            service, SERVICE,
            "a user data dir shares the default keychain"
        );
        assert!(service.starts_with("muxel-"), "{service}");
        assert_ne!(service, service_for(Some(&b)));
        // Other spellings of the same directory share its entries.
        assert_eq!(service, service_for(Some(&b.join("..").join("a"))));
        #[cfg(windows)]
        {
            let upper = std::path::PathBuf::from(a.to_string_lossy().to_uppercase());
            assert_eq!(service, service_for(Some(&upper)));
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_hash_never_changes() {
        // Another hash would orphan every key saved under a user data dir.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
