//! End-to-end encryption for muxel Cloud (docs/cloud-sync-protocol.md §3).
//!
//! Primitives: XChaCha20-Poly1305, X25519, HKDF-SHA256, Ed25519, SHA-256. The
//! server never sees a key or a plaintext; everything it stores comes out of
//! this module as ciphertext.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("decryption failed (wrong key or tampered data)")]
    Decrypt,
    #[error("signature check failed")]
    Signature,
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("the operating system's random number generator failed")]
    Random,
    #[error("that recovery key isn't valid")]
    BadRecoveryKey,
}

pub type Result<T> = std::result::Result<T, CryptoError>;

/// A 32-byte symmetric key, wiped on drop.
pub type Key = Zeroizing<[u8; 32]>;

const NONCE: usize = 24;

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|_| CryptoError::Random)?;
    Ok(b)
}

pub fn random_key() -> Result<Key> {
    Ok(Zeroizing::new(random::<32>()?))
}

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str, what: &'static str) -> Result<Vec<u8>> {
    B64.decode(s.trim())
        .map_err(|_| CryptoError::Malformed(what))
}

fn arr32(v: &[u8], what: &'static str) -> Result<[u8; 32]> {
    v.try_into().map_err(|_| CryptoError::Malformed(what))
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hkdf32(ikm: &[u8], salt: &[u8], info: &str) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(info.as_bytes(), out.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// `nonce(24) ‖ XChaCha20-Poly1305(key, nonce, aad, plaintext)`.
pub fn secretbox(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let nonce = random::<NONCE>()?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)?;
    let mut out = Vec::with_capacity(NONCE + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn secretbox_open(key: &[u8; 32], aad: &[u8], boxed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if boxed.len() < NONCE + 16 {
        return Err(CryptoError::Malformed("ciphertext"));
    }
    let (nonce, ct) = boxed.split_at(NONCE);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

/// Encrypt to an X25519 public key: `epk(32) ‖ secretbox(k, aad, plaintext)`.
pub fn seal(recipient: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let esk = StaticSecret::from(random::<32>()?);
    let epk = PublicKey::from(&esk);
    let shared = esk.diffie_hellman(&PublicKey::from(*recipient));
    let k = hkdf32(
        shared.as_bytes(),
        &[epk.as_bytes().as_slice(), recipient.as_slice()].concat(),
        "muxel-cloud/v1/seal",
    );
    let mut out = epk.as_bytes().to_vec();
    out.extend(secretbox(&k, aad, plaintext)?);
    Ok(out)
}

pub fn seal_open(sk: &StaticSecret, aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if sealed.len() < 32 {
        return Err(CryptoError::Malformed("sealed box"));
    }
    let (epk, rest) = sealed.split_at(32);
    let epk = arr32(epk, "sealed box")?;
    let me = PublicKey::from(sk);
    let shared = sk.diffie_hellman(&PublicKey::from(epk));
    let k = hkdf32(
        shared.as_bytes(),
        &[epk.as_slice(), me.as_bytes().as_slice()].concat(),
        "muxel-cloud/v1/seal",
    );
    secretbox_open(&k, aad, rest)
}

fn key_from(bytes: &[u8], what: &'static str) -> Result<Key> {
    Ok(Zeroizing::new(arr32(bytes, what)?))
}

// ---------------------------------------------------------------------------
// Account keys (§3.1)
// ---------------------------------------------------------------------------

/// A user's two keypairs. Private halves are wiped on drop by the dalek types.
pub struct AccountKeys {
    enc: StaticSecret,
    sign: SigningKey,
}

impl AccountKeys {
    pub fn generate() -> Result<Self> {
        Ok(Self {
            enc: StaticSecret::from(random::<32>()?),
            sign: SigningKey::from_bytes(&random::<32>()?),
        })
    }

    fn from_private(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 64 {
            return Err(CryptoError::Malformed("account private key"));
        }
        Ok(Self {
            enc: StaticSecret::from(arr32(&bytes[..32], "account private key")?),
            sign: SigningKey::from_bytes(&arr32(&bytes[32..], "account private key")?),
        })
    }

    fn private_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut v = Zeroizing::new(Vec::with_capacity(64));
        v.extend_from_slice(self.enc.as_bytes());
        v.extend_from_slice(self.sign.as_bytes());
        v
    }

    pub fn enc_pub(&self) -> [u8; 32] {
        *PublicKey::from(&self.enc).as_bytes()
    }

    pub fn sign_pub(&self) -> [u8; 32] {
        self.sign.verifying_key().to_bytes()
    }

    pub fn enc_secret(&self) -> &StaticSecret {
        &self.enc
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.sign.sign(msg).to_bytes()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.enc_pub(), &self.sign_pub())
    }
}

/// First 10 bytes of `SHA-256("muxel-cloud/v1/fp" ‖ enc_pub ‖ sign_pub)` as five
/// groups of four hex digits.
pub fn fingerprint(enc_pub: &[u8; 32], sign_pub: &[u8; 32]) -> String {
    let h = sha256(&[b"muxel-cloud/v1/fp", enc_pub, sign_pub]);
    h[..10]
        .chunks(2)
        .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn verify(sign_pub: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<()> {
    let vk =
        VerifyingKey::from_bytes(sign_pub).map_err(|_| CryptoError::Malformed("public key"))?;
    let sig = Signature::from_slice(sig).map_err(|_| CryptoError::Malformed("signature"))?;
    vk.verify(msg, &sig).map_err(|_| CryptoError::Signature)
}

/// The Recovery Key: 16 random bytes shown once as `MXL-XXXX-XXXX-…`.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryKey(Zeroizing<[u8; 16]>);

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl RecoveryKey {
    pub fn generate() -> Result<Self> {
        Ok(Self(Zeroizing::new(random::<16>()?)))
    }

    /// Crockford base32 of the 128 bits (26 characters), grouped by four.
    pub fn display(&self) -> String {
        let mut bits: u128 = u128::from_be_bytes(*self.0);
        let mut chars = [0u8; 26];
        // 26 × 5 = 130 bits: the top two of the first symbol are zero.
        for c in chars.iter_mut().rev() {
            *c = CROCKFORD[(bits & 31) as usize];
            bits >>= 5;
        }
        let s = std::str::from_utf8(&chars).expect("ascii");
        let groups: Vec<&str> = s
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("ascii"))
            .collect();
        format!("MXL-{}", groups.join("-"))
    }

    pub fn parse(input: &str) -> Result<Self> {
        let s = input.trim().to_ascii_uppercase();
        let s = s.strip_prefix("MXL").unwrap_or(&s);
        let mut bits: u128 = 0;
        let mut n = 0;
        for ch in s.chars() {
            let ch = match ch {
                '-' | ' ' | '\t' | '\n' => continue,
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            let v = CROCKFORD
                .iter()
                .position(|&c| c as char == ch)
                .ok_or(CryptoError::BadRecoveryKey)?;
            if n == 0 && v > 7 {
                return Err(CryptoError::BadRecoveryKey);
            }
            bits = (bits << 5) | v as u128;
            n += 1;
        }
        if n != 26 {
            return Err(CryptoError::BadRecoveryKey);
        }
        Ok(Self(Zeroizing::new(bits.to_be_bytes())))
    }

    fn derive(&self, salt: &[u8]) -> Key {
        hkdf32(self.0.as_slice(), salt, "muxel-cloud/v1/recovery")
    }
}

/// What the server stores for an account (`/account/keys`), all base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBundle {
    pub enc_pub: String,
    pub sign_pub: String,
    pub sealed_private: String,
    pub auk_recovery: String,
    pub recovery_salt: String,
}

/// A brand-new account: keys, the AUK to keep in the keychain, the Recovery
/// Key to show once, and the bundle to upload.
pub struct NewAccount {
    pub keys: AccountKeys,
    pub auk: Key,
    pub recovery: RecoveryKey,
    pub bundle: AccountBundle,
}

pub fn create_account() -> Result<NewAccount> {
    let keys = AccountKeys::generate()?;
    let auk = random_key()?;
    let recovery = RecoveryKey::generate()?;
    let salt = random::<16>()?;
    let bundle = AccountBundle {
        enc_pub: b64(&keys.enc_pub()),
        sign_pub: b64(&keys.sign_pub()),
        sealed_private: b64(&secretbox(
            &auk,
            b"muxel-cloud/v1/account-private",
            &keys.private_bytes(),
        )?),
        auk_recovery: b64(&secretbox(
            &recovery.derive(&salt),
            b"muxel-cloud/v1/auk",
            auk.as_slice(),
        )?),
        recovery_salt: b64(&salt),
    };
    Ok(NewAccount {
        keys,
        auk,
        recovery,
        bundle,
    })
}

/// A new Recovery Key for an existing AUK (`/account/keys/rotate-recovery`).
pub fn rotate_recovery(auk: &[u8; 32]) -> Result<(RecoveryKey, String, String)> {
    let recovery = RecoveryKey::generate()?;
    let salt = random::<16>()?;
    let wrapped = secretbox(&recovery.derive(&salt), b"muxel-cloud/v1/auk", auk)?;
    Ok((recovery, b64(&wrapped), b64(&salt)))
}

/// Recover the AUK from the Recovery Key.
pub fn auk_from_recovery(bundle: &AccountBundle, rk: &RecoveryKey) -> Result<Key> {
    let salt = unb64(&bundle.recovery_salt, "recovery salt")?;
    let wrapped = unb64(&bundle.auk_recovery, "wrapped AUK")?;
    let auk = secretbox_open(&rk.derive(&salt), b"muxel-cloud/v1/auk", &wrapped)
        .map_err(|_| CryptoError::BadRecoveryKey)?;
    key_from(&auk, "AUK")
}

/// Unseal the account's private keys with the AUK, checking they match the
/// published public halves.
pub fn unlock(bundle: &AccountBundle, auk: &[u8; 32]) -> Result<AccountKeys> {
    let sealed = unb64(&bundle.sealed_private, "sealed private key")?;
    let private = secretbox_open(auk, b"muxel-cloud/v1/account-private", &sealed)?;
    let keys = AccountKeys::from_private(&private)?;
    if b64(&keys.enc_pub()) != bundle.enc_pub || b64(&keys.sign_pub()) != bundle.sign_pub {
        return Err(CryptoError::Malformed(
            "account keys (public halves differ)",
        ));
    }
    Ok(keys)
}

/// Device approval: an existing device wraps the AUK to a new device's key.
pub fn seal_auk_for_device(device_pub: &[u8; 32], auk: &[u8; 32]) -> Result<String> {
    Ok(b64(&seal(device_pub, b"muxel-cloud/v1/auk-approval", auk)?))
}

pub fn open_auk_from_approval(device_sk: &StaticSecret, sealed_b64: &str) -> Result<Key> {
    let sealed = unb64(sealed_b64, "sealed AUK")?;
    let auk = seal_open(device_sk, b"muxel-cloud/v1/auk-approval", &sealed)?;
    key_from(&auk, "AUK")
}

// ---------------------------------------------------------------------------
// Scope key grants (§3.2)
// ---------------------------------------------------------------------------

fn grant_aad(scope: Uuid, generation: u32) -> Vec<u8> {
    format!("muxel-cloud/v1/grant|{scope}|{generation}").into_bytes()
}

fn grant_sig_msg(scope: Uuid, generation: u32, member: Uuid, wrapped: &[u8]) -> Vec<u8> {
    let mut m = format!("muxel-cloud/v1/grant-sig|{scope}|{generation}|{member}|").into_bytes();
    m.extend_from_slice(&sha256(&[wrapped]));
    m
}

/// A grant as sent to / received from the server (base64 fields).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireGrant {
    pub wrapped_key: String,
    pub signature: String,
}

pub fn make_grant(
    granter: &AccountKeys,
    member: Uuid,
    member_enc_pub: &[u8; 32],
    scope: Uuid,
    generation: u32,
    sk: &[u8; 32],
) -> Result<WireGrant> {
    let wrapped = seal(member_enc_pub, &grant_aad(scope, generation), sk)?;
    let sig = granter.sign(&grant_sig_msg(scope, generation, member, &wrapped));
    Ok(WireGrant {
        wrapped_key: b64(&wrapped),
        signature: b64(&sig),
    })
}

/// Open a grant addressed to `me`, after checking `granter_sign_pub` signed it.
pub fn open_grant(
    me: &AccountKeys,
    my_user: Uuid,
    scope: Uuid,
    generation: u32,
    grant: &WireGrant,
    granter_sign_pub: &[u8; 32],
) -> Result<Key> {
    let wrapped = unb64(&grant.wrapped_key, "grant")?;
    let sig = unb64(&grant.signature, "grant signature")?;
    verify(
        granter_sign_pub,
        &grant_sig_msg(scope, generation, my_user, &wrapped),
        &sig,
    )?;
    let sk = seal_open(me.enc_secret(), &grant_aad(scope, generation), &wrapped)?;
    key_from(&sk, "scope key")
}

// ---------------------------------------------------------------------------
// Item envelopes (§3.3)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub user: Uuid,
    pub device: Uuid,
}

/// The JSON envelope the server stores (as opaque bytes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub generation: u32,
    pub wk: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wa: Option<String>,
    pub ct: String,
    pub by: Author,
    pub sig: String,
}

/// Where an item lives; bound into every AAD and signature so the server
/// can't move a ciphertext elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemAddr<'a> {
    pub scope: Uuid,
    pub collection: &'a str,
    pub item: Uuid,
}

impl ItemAddr<'_> {
    fn aad_c(&self) -> Vec<u8> {
        format!(
            "muxel-cloud/v1/item|{}|{}|{}",
            self.scope, self.collection, self.item
        )
        .into_bytes()
    }

    fn aad_k(&self, generation: u32) -> Vec<u8> {
        let mut a = self.aad_c();
        a.extend_from_slice(format!("|key|{generation}").as_bytes());
        a
    }

    fn sig_msg(&self, generation: u32, wk: &[u8], wa: &[u8], ct: &[u8]) -> Vec<u8> {
        let h = sha256(&[wk, wa, ct]);
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "muxel-cloud/v1/item-sig|{}|{}|{}|{}|{}",
            self.scope, self.collection, self.item, generation, hex
        )
        .into_bytes()
    }
}

/// Encrypt and sign an item. `assignee_pub` additionally wraps the data key to
/// a task's assignee (§3.3 `wa`).
pub fn seal_item(
    addr: ItemAddr,
    generation: u32,
    scope_key: &[u8; 32],
    plaintext: &[u8],
    author: &AccountKeys,
    by: Author,
    assignee_pub: Option<&[u8; 32]>,
) -> Result<Envelope> {
    let dek = random_key()?;
    let wk = secretbox(scope_key, &addr.aad_k(generation), dek.as_slice())?;
    let wa = match assignee_pub {
        Some(p) => Some(seal(p, &addr.aad_k(generation), dek.as_slice())?),
        None => None,
    };
    let ct = secretbox(&dek, &addr.aad_c(), plaintext)?;
    let sig = author.sign(&addr.sig_msg(generation, &wk, wa.as_deref().unwrap_or(&[]), &ct));
    Ok(Envelope {
        v: 1,
        generation,
        wk: b64(&wk),
        wa: wa.map(|w| b64(&w)),
        ct: b64(&ct),
        by,
        sig: b64(&sig),
    })
}

/// How the reader can get at the data key.
pub enum ItemKey<'a> {
    /// The scope key for the envelope's generation.
    Scope(&'a [u8; 32]),
    /// The reader is the task's assignee and holds no scope key.
    Assignee(&'a AccountKeys),
}

/// Verify the author's signature, then decrypt.
pub fn open_item(
    addr: ItemAddr,
    env: &Envelope,
    key: ItemKey,
    author_sign_pub: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>> {
    if env.v != 1 {
        return Err(CryptoError::Malformed("envelope version"));
    }
    let wk = unb64(&env.wk, "wk")?;
    let wa = match &env.wa {
        Some(w) => unb64(w, "wa")?,
        None => Vec::new(),
    };
    let ct = unb64(&env.ct, "ct")?;
    let sig = unb64(&env.sig, "sig")?;
    verify(
        author_sign_pub,
        &addr.sig_msg(env.generation, &wk, &wa, &ct),
        &sig,
    )?;
    let dek = match key {
        ItemKey::Scope(sk) => secretbox_open(sk, &addr.aad_k(env.generation), &wk)?,
        ItemKey::Assignee(me) => {
            if wa.is_empty() {
                return Err(CryptoError::Decrypt);
            }
            seal_open(me.enc_secret(), &addr.aad_k(env.generation), &wa)?
        }
    };
    let dek = key_from(&dek, "data key")?;
    secretbox_open(&dek, &addr.aad_c(), &ct)
}

// ---------------------------------------------------------------------------
// Activity envelopes (§3.4)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEnvelope {
    pub v: u32,
    pub generation: u32,
    pub ct: String,
    pub by: Author,
    pub sig: String,
}

fn activity_aad(team: Uuid, by: Author) -> Vec<u8> {
    format!("muxel-cloud/v1/activity|{team}|{}|{}", by.user, by.device).into_bytes()
}

pub fn seal_activity(
    team: Uuid,
    generation: u32,
    key: &[u8; 32],
    plaintext: &[u8],
    author: &AccountKeys,
    by: Author,
) -> Result<ActivityEnvelope> {
    let aad = activity_aad(team, by);
    let ct = secretbox(key, &aad, plaintext)?;
    let mut msg = aad.clone();
    msg.extend_from_slice(&sha256(&[&ct]));
    Ok(ActivityEnvelope {
        v: 1,
        generation,
        ct: b64(&ct),
        by,
        sig: b64(&author.sign(&msg)),
    })
}

pub fn open_activity(
    team: Uuid,
    env: &ActivityEnvelope,
    key: &[u8; 32],
    author_sign_pub: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>> {
    let aad = activity_aad(team, env.by);
    let ct = unb64(&env.ct, "ct")?;
    let mut msg = aad.clone();
    msg.extend_from_slice(&sha256(&[&ct]));
    verify(author_sign_pub, &msg, &unb64(&env.sig, "sig")?)?;
    secretbox_open(key, &aad, &ct)
}

pub fn pub32(b64s: &str, what: &'static str) -> Result<[u8; 32]> {
    arr32(&unb64(b64s, what)?, what)
}

#[cfg(test)]
mod tests {
    use super::{
        AccountKeys, Author, CryptoError, ItemAddr, ItemKey, RecoveryKey, auk_from_recovery,
        create_account, make_grant, open_activity, open_auk_from_approval, open_grant, open_item,
        random_key, seal_activity, seal_auk_for_device, seal_item, seal_open, secretbox,
        secretbox_open, unlock,
    };
    use uuid::Uuid;
    use x25519_dalek::{PublicKey, StaticSecret};

    fn addr(collection: &str) -> ItemAddr<'_> {
        ItemAddr {
            scope: Uuid::from_u128(10),
            collection,
            item: Uuid::from_u128(11),
        }
    }

    fn by() -> Author {
        Author {
            user: Uuid::from_u128(1),
            device: Uuid::from_u128(2),
        }
    }

    #[test]
    fn secretbox_binds_aad() {
        let k = random_key().unwrap();
        let b = secretbox(&k, b"a", b"hello").unwrap();
        assert_eq!(&*secretbox_open(&k, b"a", &b).unwrap(), b"hello");
        assert_eq!(secretbox_open(&k, b"b", &b), Err(CryptoError::Decrypt));
    }

    #[test]
    fn seal_roundtrip() {
        let sk = StaticSecret::from(super::random::<32>().unwrap());
        let pk = *PublicKey::from(&sk).as_bytes();
        let s = super::seal(&pk, b"x", b"secret").unwrap();
        assert_eq!(&*seal_open(&sk, b"x", &s).unwrap(), b"secret");
        let other = StaticSecret::from(super::random::<32>().unwrap());
        assert!(seal_open(&other, b"x", &s).is_err());
    }

    #[test]
    fn recovery_key_display_parse_and_forgiving_input() {
        let rk = RecoveryKey::generate().unwrap();
        let shown = rk.display();
        assert!(shown.starts_with("MXL-"));
        assert_eq!(shown.len(), 4 + 26 + 6);
        assert!(RecoveryKey::parse(&shown).unwrap() == rk);
        let sloppy = shown.to_lowercase().replace('-', " ");
        assert!(RecoveryKey::parse(&sloppy).unwrap() == rk);
        assert!(RecoveryKey::parse("MXL-1234").is_err());
        assert!(
            RecoveryKey::parse("MXL-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZ").is_err(),
            "overflows 128 bits"
        );
        assert!(
            RecoveryKey::parse("MXL-UUUU-0000-0000-0000-0000-0000-00").is_err(),
            "U is not Crockford"
        );
    }

    #[test]
    fn account_lifecycle_recovery_and_device_approval() {
        let acct = create_account().unwrap();
        // Unlock with the AUK from the keychain.
        let k = unlock(&acct.bundle, &acct.auk).unwrap();
        assert_eq!(k.enc_pub(), acct.keys.enc_pub());
        // New device with the Recovery Key.
        let typed = RecoveryKey::parse(&acct.recovery.display()).unwrap();
        let auk = auk_from_recovery(&acct.bundle, &typed).unwrap();
        assert_eq!(*auk, *acct.auk);
        let wrong = RecoveryKey::generate().unwrap();
        assert_eq!(
            auk_from_recovery(&acct.bundle, &wrong).err(),
            Some(CryptoError::BadRecoveryKey)
        );
        // New device approved by an existing one.
        let dev = StaticSecret::from(super::random::<32>().unwrap());
        let sealed = seal_auk_for_device(PublicKey::from(&dev).as_bytes(), &acct.auk).unwrap();
        assert_eq!(*open_auk_from_approval(&dev, &sealed).unwrap(), *acct.auk);
        assert_eq!(acct.keys.fingerprint().split(' ').count(), 5);
    }

    #[test]
    fn grants_are_signed_and_addressed() {
        let owner = AccountKeys::generate().unwrap();
        let member = AccountKeys::generate().unwrap();
        let member_id = Uuid::from_u128(5);
        let scope = Uuid::from_u128(6);
        let sk = random_key().unwrap();
        let g = make_grant(&owner, member_id, &member.enc_pub(), scope, 2, &sk).unwrap();
        let got = open_grant(&member, member_id, scope, 2, &g, &owner.sign_pub()).unwrap();
        assert_eq!(*got, *sk);
        // Wrong generation, wrong member id, or a forged granter all fail.
        assert!(open_grant(&member, member_id, scope, 3, &g, &owner.sign_pub()).is_err());
        assert!(open_grant(&member, Uuid::from_u128(9), scope, 2, &g, &owner.sign_pub()).is_err());
        let mallory = AccountKeys::generate().unwrap();
        assert_eq!(
            open_grant(&member, member_id, scope, 2, &g, &mallory.sign_pub()).err(),
            Some(CryptoError::Signature)
        );
    }

    #[test]
    fn items_verify_bind_location_and_support_assignees() {
        let author = AccountKeys::generate().unwrap();
        let assignee = AccountKeys::generate().unwrap();
        let sk = random_key().unwrap();
        let env = seal_item(
            addr("tasks"),
            1,
            &sk,
            b"{\"t\":1}",
            &author,
            by(),
            Some(&assignee.enc_pub()),
        )
        .unwrap();
        let pt = open_item(addr("tasks"), &env, ItemKey::Scope(&sk), &author.sign_pub()).unwrap();
        assert_eq!(&*pt, b"{\"t\":1}");
        let pt = open_item(
            addr("tasks"),
            &env,
            ItemKey::Assignee(&assignee),
            &author.sign_pub(),
        )
        .unwrap();
        assert_eq!(&*pt, b"{\"t\":1}");
        // Moving the ciphertext to another collection breaks the signature.
        assert_eq!(
            open_item(addr("loops"), &env, ItemKey::Scope(&sk), &author.sign_pub()).err(),
            Some(CryptoError::Signature)
        );
        // A different author can't claim it.
        let other = AccountKeys::generate().unwrap();
        assert!(open_item(addr("tasks"), &env, ItemKey::Scope(&sk), &other.sign_pub()).is_err());
        // Tampering with the ciphertext is caught.
        let mut bad = env.clone();
        bad.ct = super::b64(b"garbage-garbage-garbage-garbage-garbage-garbage");
        assert!(open_item(addr("tasks"), &bad, ItemKey::Scope(&sk), &author.sign_pub()).is_err());
        // Without `wa`, an assignee can't read it.
        let plain = seal_item(addr("tasks"), 1, &sk, b"x", &author, by(), None).unwrap();
        assert!(
            open_item(
                addr("tasks"),
                &plain,
                ItemKey::Assignee(&assignee),
                &author.sign_pub()
            )
            .is_err()
        );
        // Envelope survives JSON.
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(serde_json::from_str::<super::Envelope>(&json).unwrap(), env);
    }

    #[test]
    fn activity_roundtrip() {
        let author = AccountKeys::generate().unwrap();
        let k = random_key().unwrap();
        let team = Uuid::from_u128(3);
        let env = seal_activity(team, 1, &k, b"hi", &author, by()).unwrap();
        assert_eq!(
            &*open_activity(team, &env, &k, &author.sign_pub()).unwrap(),
            b"hi"
        );
        assert!(open_activity(Uuid::from_u128(4), &env, &k, &author.sign_pub()).is_err());
    }
}
