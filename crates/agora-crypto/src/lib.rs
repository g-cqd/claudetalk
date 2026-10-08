//! Identity, signatures and channel encryption.
//!
//! * Identity: Ed25519. The pseudonym is the first 8 bytes of blake3(pubkey), hex.
//! * Signing: over a domain-separated, length-prefixed encoding (no ambiguity
//!   between field boundaries).
//! * Encryption: one random 32-byte key per channel (XChaCha20-Poly1305),
//!   wrapped to each member with an X25519 shared secret derived from the
//!   members' Ed25519 identities.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("invalid public key")]
    BadKey,
    #[error("invalid signature")]
    BadSignature,
    #[error("decryption failed")]
    Decrypt,
    #[error("malformed ciphertext")]
    Malformed,
    #[error("random source failed")]
    Random,
}

pub const SIG_LEN: usize = 64;
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

fn random<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| CryptoError::Random)?;
    Ok(b)
}

/// N bytes from the OS random source.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    random::<N>()
}

/// Length-prefixed, domain-separated encoding for signing.
pub fn canonical(domain: &str, fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"agora/");
    out.extend_from_slice(domain.as_bytes());
    out.push(0);
    for f in fields {
        out.extend_from_slice(&(f.len() as u64).to_le_bytes());
        out.extend_from_slice(f);
    }
    out
}

pub fn pseudonym(public: &[u8; 32]) -> String {
    let h = blake3::hash(public);
    h.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self::from_seed(random::<32>()?))
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    pub fn public(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn pseudonym(&self) -> String {
        pseudonym(&self.public())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; SIG_LEN] {
        self.key.sign(msg).to_bytes()
    }

    fn x25519(&self) -> x25519_dalek::StaticSecret {
        x25519_dalek::StaticSecret::from(self.key.to_scalar_bytes())
    }

    fn shared_with(&self, peer: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
        let vk = VerifyingKey::from_bytes(peer).map_err(|_| CryptoError::BadKey)?;
        let peer_x = x25519_dalek::PublicKey::from(vk.to_montgomery().to_bytes());
        let shared = self.x25519().diffie_hellman(&peer_x);
        // Bind both identities so the wrap key is direction-independent.
        let (a, b) = {
            let mine = self.public();
            if mine <= *peer {
                (mine, *peer)
            } else {
                (*peer, mine)
            }
        };
        let mut h = blake3::Hasher::new_derive_key("agora/channel-key-wrap/v1");
        h.update(shared.as_bytes());
        h.update(&a);
        h.update(&b);
        Ok(*h.finalize().as_bytes())
    }

    /// Wrap a channel key for `recipient` at key `epoch`. Output: nonce || ciphertext.
    /// The AAD is always [`wrap_aad`]`(channel, epoch)`, so a wrapped key cannot be
    /// re-bound to another channel or epoch by the caller.
    pub fn wrap_key(
        &self,
        recipient: &[u8; 32],
        channel: &str,
        epoch: i64,
        key: &ChannelKey,
    ) -> Result<Vec<u8>, CryptoError> {
        aead_seal(
            &self.shared_with(recipient)?,
            &wrap_aad(channel, epoch),
            &key.0,
        )
    }

    /// Inverse of [`Identity::wrap_key`]; `sender` is the identity that wrapped the key.
    pub fn unwrap_key(
        &self,
        sender: &[u8; 32],
        channel: &str,
        epoch: i64,
        wrapped: &[u8],
    ) -> Result<ChannelKey, CryptoError> {
        let plain = aead_open(
            &self.shared_with(sender)?,
            &wrap_aad(channel, epoch),
            wrapped,
        )?;
        let key: [u8; KEY_LEN] = plain
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Malformed)?;
        Ok(ChannelKey(key))
    }
}

/// AAD binding a wrapped channel key to its channel and epoch:
/// `canonical("wrap/v1", [channel, epoch.to_le_bytes()])`.
pub fn wrap_aad(channel: &str, epoch: i64) -> Vec<u8> {
    canonical("wrap/v1", &[channel.as_bytes(), &epoch.to_le_bytes()])
}

/// AAD for an encrypted message body:
/// `canonical("body/v1", [channel, epoch.to_le_bytes(), message_id])`.
pub fn body_aad(channel: &str, epoch: i64, message_id: &str) -> Vec<u8> {
    canonical(
        "body/v1",
        &[
            channel.as_bytes(),
            &epoch.to_le_bytes(),
            message_id.as_bytes(),
        ],
    )
}

/// AAD for an encrypted attachment blob: `canonical("att/v1", [channel, epoch.to_le_bytes()])`.
pub fn attachment_aad(channel: &str, epoch: i64) -> Vec<u8> {
    canonical("att/v1", &[channel.as_bytes(), &epoch.to_le_bytes()])
}

pub fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<(), CryptoError> {
    let vk = VerifyingKey::from_bytes(public).map_err(|_| CryptoError::BadKey)?;
    let sig: [u8; SIG_LEN] = sig.try_into().map_err(|_| CryptoError::BadSignature)?;
    vk.verify(msg, &Signature::from_bytes(&sig))
        .map_err(|_| CryptoError::BadSignature)
}

fn aead_seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let nonce = random::<NONCE_LEN>()?;
    let ct = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .map_err(|_| CryptoError::Decrypt)?;
    let mut out = nonce.to_vec();
    out.extend(ct);
    Ok(out)
}

fn aead_open(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < NONCE_LEN + 16 {
        return Err(CryptoError::Malformed);
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| CryptoError::Decrypt)
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChannelKey([u8; KEY_LEN]);

impl ChannelKey {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self(random::<KEY_LEN>()?))
    }

    /// `aad` should bind channel and message id so ciphertext can't be replayed elsewhere.
    pub fn seal(&self, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
        aead_seal(&self.0, aad, plain)
    }

    pub fn open(&self, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        aead_open(&self.0, aad, blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let id = Identity::generate().unwrap();
        let m = canonical("msg", &[b"chan", b"hello"]);
        let sig = id.sign(&m);
        assert!(verify(&id.public(), &m, &sig).is_ok());
        let tampered = canonical("msg", &[b"chan", b"hellO"]);
        assert_eq!(
            verify(&id.public(), &tampered, &sig),
            Err(CryptoError::BadSignature)
        );
    }

    #[test]
    fn canonical_encoding_has_no_boundary_ambiguity() {
        assert_ne!(
            canonical("m", &[b"ab", b"c"]),
            canonical("m", &[b"a", b"bc"])
        );
        assert_ne!(canonical("m", &[b"x"]), canonical("n", &[b"x"]));
    }

    #[test]
    fn other_identity_cannot_forge() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let sig = a.sign(b"x");
        assert!(verify(&b.public(), b"x", &sig).is_err());
    }

    #[test]
    fn channel_key_wraps_between_members() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let eve = Identity::generate().unwrap();
        let key = ChannelKey::generate().unwrap();
        let wrapped = alice.wrap_key(&bob.public(), "c1", 0, &key).unwrap();
        assert!(bob.unwrap_key(&alice.public(), "c1", 0, &wrapped).unwrap() == key);
        assert!(eve.unwrap_key(&alice.public(), "c1", 0, &wrapped).is_err());
    }

    #[test]
    fn wrapped_key_with_wrong_epoch_fails_to_unwrap() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let key = ChannelKey::generate().unwrap();
        let wrapped = alice.wrap_key(&bob.public(), "c1", 3, &key).unwrap();
        assert_eq!(
            bob.unwrap_key(&alice.public(), "c1", 2, &wrapped).err(),
            Some(CryptoError::Decrypt)
        );
        assert_eq!(
            bob.unwrap_key(&alice.public(), "c1", 4, &wrapped).err(),
            Some(CryptoError::Decrypt)
        );
    }

    #[test]
    fn wrapped_key_with_wrong_channel_fails_to_unwrap() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let key = ChannelKey::generate().unwrap();
        let wrapped = alice.wrap_key(&bob.public(), "c1", 0, &key).unwrap();
        assert_eq!(
            bob.unwrap_key(&alice.public(), "c2", 0, &wrapped).err(),
            Some(CryptoError::Decrypt)
        );
    }

    #[test]
    fn unwrap_rejects_wrapped_plaintext_that_is_not_a_key() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let shared = alice.shared_with(&bob.public()).unwrap();
        let wrapped = aead_seal(&shared, &wrap_aad("c1", 0), b"too short").unwrap();
        assert_eq!(
            bob.unwrap_key(&alice.public(), "c1", 0, &wrapped).err(),
            Some(CryptoError::Malformed)
        );
    }

    #[test]
    fn aad_functions_differ_for_different_inputs() {
        assert_ne!(wrap_aad("c1", 0), wrap_aad("c1", 1));
        assert_ne!(wrap_aad("c1", 0), wrap_aad("c2", 0));
        assert_ne!(body_aad("c1", 0, "m1"), body_aad("c1", 0, "m2"));
        assert_ne!(body_aad("c1", 0, "m1"), body_aad("c1", 1, "m1"));
        assert_ne!(body_aad("c1", 0, "m1"), body_aad("c2", 0, "m1"));
        assert_ne!(attachment_aad("c1", 0), attachment_aad("c1", 1));
        assert_ne!(attachment_aad("c1", 0), attachment_aad("c2", 0));
    }

    #[test]
    fn aad_functions_are_domain_separated() {
        assert_ne!(wrap_aad("c1", 0), attachment_aad("c1", 0));
        assert_ne!(body_aad("c1", 0, ""), attachment_aad("c1", 0));
        assert_ne!(body_aad("c1", 0, ""), wrap_aad("c1", 0));
    }

    #[test]
    fn seal_binds_aad_and_detects_tampering() {
        let key = ChannelKey::generate().unwrap();
        let mut ct = key.seal(b"c1|m1", b"secret").unwrap();
        assert_eq!(key.open(b"c1|m1", &ct).unwrap(), b"secret");
        assert!(key.open(b"c1|m2", &ct).is_err());
        let last = ct.len() - 1;
        ct[last] ^= 1;
        assert!(key.open(b"c1|m1", &ct).is_err());
        assert_eq!(key.open(b"", &[0; 3]), Err(CryptoError::Malformed));
    }

    #[test]
    fn pseudonym_is_stable() {
        let id = Identity::from_seed([7; 32]);
        assert_eq!(id.pseudonym(), pseudonym(&id.public()));
        assert_eq!(id.pseudonym().len(), 16);
    }
}
