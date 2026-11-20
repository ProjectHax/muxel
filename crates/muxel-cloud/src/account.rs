//! Signing in and unlocking (protocol §2–3): the browser device login, first
//! key setup on a new account, and unlocking on another device with the
//! Recovery Key. Everything here blocks; call it off the UI thread.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::{ApiError, Client, Transport};
use crate::crypto::{self, AccountBundle, CryptoError, Key, RecoveryKey};
use crate::engine::{CycleError, Session};

/// Keychain entry names (under the profile's keychain service).
pub const TOKEN_SECRET: &str = "token";
pub const AUK_SECRET: &str = "auk";

/// What a device remembers about its account between runs. Holds no secrets:
/// the bundle's private half is sealed under the AUK, which lives in the
/// keychain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountFile {
    pub user: Uuid,
    pub device: Uuid,
    pub email: String,
    pub bundle: AccountBundle,
    /// A Recovery Key was shown but the user hasn't said they saved it.
    #[serde(default)]
    pub recovery_unconfirmed: bool,
}

/// The outcome of [`setup`].
pub enum Setup {
    /// Unlocked. `created` is the Recovery Key of an account set up just now,
    /// to show the user exactly once.
    Ready {
        account: AccountFile,
        auk: Key,
        created: Option<RecoveryKey>,
    },
    /// The account has keys but this device can't open them yet: ask for the
    /// Recovery Key (or, later, approval from a signed-in device).
    NeedsRecovery { account: AccountFile },
}

/// After sign-in (or on start-up with a stored token): fetch or create the
/// account keys and try the AUK this device already has.
pub fn setup<T: Transport>(client: &Client<T>, auk: Option<Key>) -> Result<Setup, CycleError> {
    let me = client.me()?;
    let file = |bundle: AccountBundle| AccountFile {
        user: me.user.id,
        device: me.device.id,
        email: me.user.email.clone(),
        bundle,
        recovery_unconfirmed: false,
    };
    let bundle = match client.account_keys()? {
        Some(bundle) => bundle,
        None => {
            let new = crypto::create_account()?;
            match client.put_account_keys(&new.bundle) {
                Ok(()) => {
                    return Ok(Setup::Ready {
                        account: AccountFile {
                            recovery_unconfirmed: true,
                            ..file(new.bundle)
                        },
                        auk: new.auk,
                        created: Some(new.recovery),
                    });
                }
                // Another device of this account uploaded first: use theirs.
                Err(ApiError::Conflict(_)) => client
                    .account_keys()?
                    .ok_or(ApiError::Decode("account keys vanished".into()))?,
                Err(e) => return Err(e.into()),
            }
        }
    };
    match auk {
        Some(auk) if crypto::unlock(&bundle, &auk).is_ok() => Ok(Setup::Ready {
            account: file(bundle),
            auk,
            created: None,
        }),
        _ => Ok(Setup::NeedsRecovery {
            account: file(bundle),
        }),
    }
}

/// The AUK from a typed Recovery Key, checked against the account's keys.
pub fn recover(account: &AccountFile, typed: &str) -> Result<Key, CryptoError> {
    let rk = RecoveryKey::parse(typed)?;
    let auk = crypto::auk_from_recovery(&account.bundle, &rk)?;
    crypto::unlock(&account.bundle, &auk)?;
    Ok(auk)
}

/// Replace the account's Recovery Key (the old one stops working). Returns
/// the new key to show, and the bundle as the server now has it.
pub fn new_recovery_key<T: Transport>(
    client: &Client<T>,
    account: &AccountFile,
    auk: &Key,
) -> Result<(RecoveryKey, AccountBundle), CycleError> {
    crypto::unlock(&account.bundle, auk)?;
    let (rk, wrapped, salt) = crypto::rotate_recovery(auk)?;
    client.rotate_recovery(&wrapped, &salt)?;
    let bundle = AccountBundle {
        auk_recovery: wrapped,
        recovery_salt: salt,
        ..account.bundle.clone()
    };
    Ok((rk, bundle))
}

/// The unlocked identity for a sync cycle.
pub fn session(account: &AccountFile, auk: &Key) -> Result<Session, CryptoError> {
    Ok(Session {
        user: account.user,
        device: account.device,
        keys: crypto::unlock(&account.bundle, auk)?,
    })
}

/// The AUK as stored in the keychain.
pub fn encode_auk(auk: &Key) -> String {
    crypto::b64(auk.as_slice())
}

pub fn decode_auk(text: &str) -> Option<Key> {
    let bytes = crypto::unb64(text, "AUK").ok()?;
    let arr: [u8; 32] = bytes.as_slice().try_into().ok()?;
    Some(zeroize::Zeroizing::new(arr))
}
