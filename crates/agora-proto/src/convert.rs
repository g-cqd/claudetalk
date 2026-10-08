//! Conversions between store types and wire types, base64/hex helpers, and the
//! handshake signature. Every decoder rejects bad input with
//! `ProtoError::Encoding("<field name>")`; nothing here panics on untrusted input.

use crate::*;
use agora_crypto::Identity;
use agora_store::{
    Attachment, CardRow, ChannelInfo, MessageRow, Pub, SignedMessage, SignedNote, SignedOp,
    SignedReaction, SprintSummary, hex, unhex32,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Standard base64 with padding.
pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Strict inverse of `b64`; `field` names the field in the error.
pub fn unb64(s: &str, field: &'static str) -> Result<Vec<u8>, ProtoError> {
    STANDARD.decode(s).map_err(|_| ProtoError::Encoding(field))
}

/// Exactly 64 lowercase hex chars (agora_store::unhex32) else Encoding(field).
pub fn unhex_pk(s: &str, field: &'static str) -> Result<Pub, ProtoError> {
    unhex32(s).ok_or(ProtoError::Encoding(field))
}

/// `agora_crypto::canonical(HELLO_DOMAIN, &[challenge, pubkey])`.
pub fn hello_signing_bytes(challenge: &[u8], pubkey: &Pub) -> Vec<u8> {
    agora_crypto::canonical(HELLO_DOMAIN, &[challenge, pubkey.as_slice()])
}

impl ClientHello {
    /// Version PROTO_VERSION, pubkey = hex(id.public()), sig over hello_signing_bytes.
    pub fn sign(id: &Identity, challenge: &[u8], label: &str) -> Self {
        let pubkey = id.public();
        let sig = id.sign(&hello_signing_bytes(challenge, &pubkey));
        Self {
            version: PROTO_VERSION,
            pubkey: hex(&pubkey),
            sig: b64(&sig),
            label: label.to_string(),
        }
    }

    /// Decode pubkey and sig, verify against `challenge`; returns the proven key.
    /// Wrong version -> Encoding("version"); bad signature -> Crypto(BadSignature).
    pub fn verify(&self, challenge: &[u8]) -> Result<Pub, ProtoError> {
        if self.version != PROTO_VERSION {
            return Err(ProtoError::Encoding("version"));
        }
        let pubkey = unhex_pk(&self.pubkey, "pubkey")?;
        let sig = unb64(&self.sig, "sig")?;
        agora_crypto::verify(&pubkey, &hello_signing_bytes(challenge, &pubkey), &sig)?;
        Ok(pubkey)
    }
}

impl OpWire {
    pub fn from_op(op: &SignedOp) -> Self {
        Self {
            kind: op.kind.clone(),
            subject: op.subject.clone(),
            arg: op.arg.clone(),
            ts: op.ts,
            nonce: op.nonce.clone(),
            actor: hex(&op.actor),
            sig: b64(&op.sig),
        }
    }

    pub fn to_op(&self) -> Result<SignedOp, ProtoError> {
        Ok(SignedOp {
            kind: self.kind.clone(),
            subject: self.subject.clone(),
            arg: self.arg.clone(),
            ts: self.ts,
            nonce: self.nonce.clone(),
            actor: unhex_pk(&self.actor, "actor")?,
            sig: unb64(&self.sig, "sig")?,
        })
    }
}

impl MessageWire {
    pub fn from_message(m: &SignedMessage) -> Self {
        Self {
            id: m.id.clone(),
            channel: m.channel.clone(),
            thread_of: m.thread_of.clone(),
            sender: hex(&m.sender),
            ts: m.ts,
            encrypted: m.encrypted,
            epoch: m.epoch,
            body: b64(&m.body),
            attachments: m
                .attachments
                .iter()
                .map(AttachmentWire::from_attachment)
                .collect(),
            sig: b64(&m.sig),
        }
    }

    pub fn to_message(&self) -> Result<SignedMessage, ProtoError> {
        Ok(SignedMessage {
            id: self.id.clone(),
            channel: self.channel.clone(),
            thread_of: self.thread_of.clone(),
            sender: unhex_pk(&self.sender, "sender")?,
            ts: self.ts,
            encrypted: self.encrypted,
            epoch: self.epoch,
            body: unb64(&self.body, "body")?,
            attachments: self
                .attachments
                .iter()
                .map(AttachmentWire::to_attachment)
                .collect(),
            sig: unb64(&self.sig, "sig")?,
        })
    }
}

impl MessageView {
    pub fn from_row(row: &MessageRow) -> Self {
        Self {
            seq: row.seq,
            msg: MessageWire {
                id: row.id.clone(),
                channel: row.channel.clone(),
                thread_of: row.thread_of.clone(),
                sender: hex(&row.sender),
                ts: row.ts,
                encrypted: row.encrypted,
                epoch: row.epoch,
                body: b64(&row.body),
                attachments: row
                    .attachments
                    .iter()
                    .map(AttachmentWire::from_attachment)
                    .collect(),
                sig: b64(&row.sig),
            },
        }
    }
}

impl AttachmentWire {
    pub fn from_attachment(a: &Attachment) -> Self {
        Self {
            hash: a.hash.clone(),
            name: a.name.clone(),
            mime: a.mime.clone(),
        }
    }

    /// Infallible by signature: `hash` is not checked here; a malformed hash simply
    /// names no blob, and the store never returns unverified bytes for it.
    pub fn to_attachment(&self) -> Attachment {
        Attachment {
            hash: self.hash.clone(),
            name: self.name.clone(),
            mime: self.mime.clone(),
        }
    }
}

impl ReactionWire {
    pub fn from_reaction(r: &SignedReaction) -> Self {
        Self {
            kind: r.kind.clone(),
            target: r.target.clone(),
            by: hex(&r.by),
            emoji: r.emoji.clone(),
            remove: r.remove,
            ts: r.ts,
            sig: b64(&r.sig),
        }
    }

    pub fn to_reaction(&self) -> Result<SignedReaction, ProtoError> {
        Ok(SignedReaction {
            kind: self.kind.clone(),
            target: self.target.clone(),
            by: unhex_pk(&self.by, "by")?,
            emoji: self.emoji.clone(),
            remove: self.remove,
            ts: self.ts,
            sig: unb64(&self.sig, "sig")?,
        })
    }
}

impl NoteWire {
    pub fn from_note(n: &SignedNote) -> Self {
        Self {
            id: n.id.clone(),
            author: hex(&n.author),
            ts: n.ts,
            title: n.title.clone(),
            body: n.body.clone(),
            tags: n.tags.clone(),
            supersedes: n.supersedes.clone(),
            sig: b64(&n.sig),
        }
    }

    pub fn to_note(&self) -> Result<SignedNote, ProtoError> {
        Ok(SignedNote {
            id: self.id.clone(),
            author: unhex_pk(&self.author, "author")?,
            ts: self.ts,
            title: self.title.clone(),
            body: self.body.clone(),
            tags: self.tags.clone(),
            supersedes: self.supersedes.clone(),
            sig: unb64(&self.sig, "sig")?,
        })
    }
}

impl KeyWire {
    pub fn from_pair(pk: &Pub, wrapped: &[u8]) -> Self {
        Self {
            pk: hex(pk),
            wrapped: b64(wrapped),
        }
    }
}

/// Decode a key list (field names "keys.pk" / "keys.wrapped" in errors).
pub fn keys_from_wire(keys: &[KeyWire]) -> Result<Vec<(Pub, Vec<u8>)>, ProtoError> {
    keys.iter()
        .map(|k| {
            Ok((
                unhex_pk(&k.pk, "keys.pk")?,
                unb64(&k.wrapped, "keys.wrapped")?,
            ))
        })
        .collect()
}

pub fn keys_to_wire(keys: &[(Pub, Vec<u8>)]) -> Vec<KeyWire> {
    keys.iter()
        .map(|(pk, wrapped)| KeyWire::from_pair(pk, wrapped))
        .collect()
}

impl ChannelView {
    pub fn from_info(info: &ChannelInfo) -> Self {
        Self {
            id: info.id.clone(),
            kind: info.kind.clone(),
            name: info.name.clone(),
            parent: info.parent.clone(),
            epoch: info.epoch,
            encrypted: info.encrypted,
            rotation_pending: info.rotation_pending,
            my_role: info.my_role.clone(),
        }
    }
}

impl MemberView {
    /// pseudonym = agora_crypto::pseudonym(pk).
    pub fn from_member(pk: &Pub, role: &str) -> Self {
        Self {
            pk: hex(pk),
            pseudonym: agora_crypto::pseudonym(pk),
            role: role.to_string(),
        }
    }
}

impl CardView {
    pub fn from_row(row: &CardRow) -> Self {
        Self {
            id: row.id.clone(),
            title: row.title.clone(),
            lane: row.lane.clone(),
            state: row.state.as_str().to_string(),
            owner: row.owner.clone(),
            updated: row.updated,
        }
    }
}

impl SprintSummaryView {
    pub fn from_summary(s: &SprintSummary) -> Self {
        Self {
            total: s.total,
            done: s.done,
            in_flight: s.in_flight,
            blocked: s.blocked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(seed: u8) -> Identity {
        Identity::from_seed([seed; 32])
    }

    /// The bytes `SignedOp::sign` covers (store-private), rebuilt for verification.
    fn op_bytes(op: &SignedOp) -> Vec<u8> {
        agora_crypto::canonical(
            "op/v1",
            &[
                op.kind.as_bytes(),
                op.subject.as_bytes(),
                op.arg.as_bytes(),
                &op.ts.to_le_bytes(),
                op.nonce.as_bytes(),
                &op.actor,
            ],
        )
    }

    #[test]
    fn b64_round_trips_including_empty() {
        for bytes in [&[][..], &[0u8, 1, 254, 255][..], &[7u8; 33][..]] {
            assert_eq!(unb64(&b64(bytes), "body").expect("valid"), bytes);
        }
    }

    #[test]
    fn unb64_rejects_unpadded_noncanonical_and_bad_alphabet() {
        assert!(matches!(
            unb64("AQ", "body"),
            Err(ProtoError::Encoding("body"))
        ));
        // Trailing bits set: "AR==" is not the canonical encoding of 0x01.
        assert!(matches!(
            unb64("AR==", "sig"),
            Err(ProtoError::Encoding("sig"))
        ));
        assert!(matches!(
            unb64("A!==", "body"),
            Err(ProtoError::Encoding("body"))
        ));
    }

    #[test]
    fn unhex_pk_accepts_lowercase_and_rejects_uppercase_and_short() {
        let pk = hex(&id(1).public());
        assert_eq!(unhex_pk(&pk, "pubkey").expect("valid"), id(1).public());
        assert!(matches!(
            unhex_pk(&"AB".repeat(32), "pubkey"),
            Err(ProtoError::Encoding("pubkey"))
        ));
        assert!(matches!(
            unhex_pk(&"ab".repeat(31), "pubkey"),
            Err(ProtoError::Encoding("pubkey"))
        ));
    }

    #[test]
    fn op_wire_round_trip_keeps_signature_valid() {
        let signer = id(7);
        let op =
            SignedOp::new("card.create", "card-1", "{\"title\":\"x\"}", 1_000, "n-1").sign(&signer);
        let back = OpWire::from_op(&op).to_op().expect("decodes");
        assert_eq!(back.kind, op.kind);
        assert_eq!(back.subject, op.subject);
        assert_eq!(back.arg, op.arg);
        assert_eq!(back.ts, op.ts);
        assert_eq!(back.nonce, op.nonce);
        assert_eq!(back.actor, signer.public());
        assert_eq!(back.sig, op.sig);
        agora_crypto::verify(&back.actor, &op_bytes(&back), &back.sig).expect("still signed");
    }

    #[test]
    fn message_wire_round_trip_is_lossless() {
        let wire = MessageWire {
            id: "m-1".into(),
            channel: "ch-1".into(),
            thread_of: Some("m-0".into()),
            sender: hex(&id(2).public()),
            ts: 42,
            encrypted: true,
            epoch: 3,
            body: b64(&[1, 2, 3, 4]),
            attachments: vec![AttachmentWire {
                hash: "ab".repeat(32),
                name: "a.txt".into(),
                mime: "text/plain".into(),
            }],
            sig: b64(&[9; 64]),
        };
        let msg = wire.to_message().expect("decodes");
        assert_eq!(msg.sender, id(2).public());
        assert_eq!(msg.body, vec![1, 2, 3, 4]);
        assert_eq!(MessageWire::from_message(&msg), wire);
    }

    #[test]
    fn reaction_and_note_wire_round_trip_are_lossless() {
        let reaction = ReactionWire {
            kind: "message".into(),
            target: "m-1".into(),
            by: hex(&id(4).public()),
            emoji: "+1".into(),
            remove: true,
            ts: 5,
            sig: b64(&[3; 64]),
        };
        let r = reaction.to_reaction().expect("decodes");
        assert_eq!(ReactionWire::from_reaction(&r), reaction);

        let note = NoteWire {
            id: "n-1".into(),
            author: hex(&id(5).public()),
            ts: 6,
            title: "t".into(),
            body: "b".into(),
            tags: "x,y".into(),
            supersedes: None,
            sig: b64(&[8; 64]),
        };
        let n = note.to_note().expect("decodes");
        assert_eq!(NoteWire::from_note(&n), note);
    }

    #[test]
    fn client_hello_signs_and_verifies_for_the_right_challenge() {
        let signer = id(9);
        let challenge = [5u8; 32];
        let hello = ClientHello::sign(&signer, &challenge, "folder");
        assert_eq!(hello.version, PROTO_VERSION);
        assert_eq!(hello.verify(&challenge).expect("verifies"), signer.public());
    }

    #[test]
    fn client_hello_rejects_wrong_challenge() {
        let hello = ClientHello::sign(&id(9), &[5u8; 32], "folder");
        assert!(matches!(
            hello.verify(&[6u8; 32]),
            Err(ProtoError::Crypto(agora_crypto::CryptoError::BadSignature))
        ));
    }

    #[test]
    fn client_hello_rejects_wrong_version() {
        let mut hello = ClientHello::sign(&id(9), &[5u8; 32], "folder");
        hello.version = PROTO_VERSION + 1;
        assert!(matches!(
            hello.verify(&[5u8; 32]),
            Err(ProtoError::Encoding("version"))
        ));
    }

    #[test]
    fn keys_from_wire_error_names_the_field() {
        let good_pk = hex(&id(10).public());
        let good_wrapped = b64(&[1u8; 72]);

        let bad_pk = [KeyWire {
            pk: "not-hex".into(),
            wrapped: good_wrapped.clone(),
        }];
        assert!(matches!(
            keys_from_wire(&bad_pk),
            Err(ProtoError::Encoding("keys.pk"))
        ));

        let bad_wrapped = [KeyWire {
            pk: good_pk.clone(),
            wrapped: "!!".into(),
        }];
        assert!(matches!(
            keys_from_wire(&bad_wrapped),
            Err(ProtoError::Encoding("keys.wrapped"))
        ));

        let ok = [KeyWire {
            pk: good_pk,
            wrapped: good_wrapped,
        }];
        let decoded = keys_from_wire(&ok).expect("decodes");
        assert_eq!(keys_to_wire(&decoded), ok);
    }
}
