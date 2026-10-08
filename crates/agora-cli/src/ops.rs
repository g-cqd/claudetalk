//! Signing and channel-crypto glue shared by the commands. All key material stays here.

use crate::CliError;
use crate::client::Client;
use agora_crypto::{ChannelKey, Identity, body_aad, random_bytes};
use agora_proto::{MessageView, MessageWire, OpWire, Reply, Req, unb64, unhex_pk};
use agora_store::{Attachment, Pub, SignedMessage, SignedOp, hex, unhex32};

/// Wall clock in ms.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// 16 random bytes, lowercase hex (32 chars).
pub fn nonce() -> String {
    hex(&random_bytes::<16>().expect("OS RNG unavailable"))
}

/// `<prefix>-<24 lowercase hex chars>` from 12 random bytes, e.g. "m-3f9a...".
pub fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        hex(&random_bytes::<12>().expect("OS RNG unavailable"))
    )
}

/// SignedOp::new(kind, subject, arg, now_ms(), &nonce()).sign(id) as an OpWire.
pub fn sign_op(id: &Identity, kind: &str, subject: &str, arg: &str) -> OpWire {
    OpWire::from_op(&SignedOp::new(kind, subject, arg, now_ms(), &nonce()).sign(id))
}

/// Plaintext message: new_id("m"), ts now, encrypted false, epoch 0, signed.
pub fn plain_message(
    id: &Identity,
    channel: &str,
    thread_of: Option<String>,
    body: &[u8],
    attachments: Vec<Attachment>,
) -> MessageWire {
    let msg = SignedMessage {
        id: new_id("m"),
        channel: channel.to_string(),
        thread_of,
        sender: [0; 32],
        ts: now_ms(),
        encrypted: false,
        epoch: 0,
        body: body.to_vec(),
        attachments,
        sig: vec![],
    };
    MessageWire::from_message(&msg.sign(id))
}

/// Encrypted message at `epoch`: body = key.seal(body_aad(channel, epoch, msg_id), body).
pub fn sealed_message(
    id: &Identity,
    channel: &str,
    thread_of: Option<String>,
    epoch: i64,
    key: &ChannelKey,
    body: &[u8],
    attachments: Vec<Attachment>,
) -> Result<MessageWire, CliError> {
    let msg_id = new_id("m");
    let sealed = key.seal(&body_aad(channel, epoch, &msg_id), body)?;
    let msg = SignedMessage {
        id: msg_id,
        channel: channel.to_string(),
        thread_of,
        sender: [0; 32],
        ts: now_ms(),
        encrypted: true,
        epoch,
        body: sealed,
        attachments,
        sig: vec![],
    };
    Ok(MessageWire::from_message(&msg.sign(id)))
}

/// Wrap `key` for each member at (channel, epoch): [(member, id.wrap_key(member, channel, epoch, key))].
pub fn wrap_for(
    id: &Identity,
    channel: &str,
    epoch: i64,
    key: &ChannelKey,
    members: &[Pub],
) -> Result<Vec<(Pub, Vec<u8>)>, CliError> {
    members
        .iter()
        .map(|pk| -> Result<(Pub, Vec<u8>), CliError> {
            Ok((*pk, id.wrap_key(pk, channel, epoch, key)?))
        })
        .collect()
}

/// Decrypt an encrypted message body with the key of its epoch
/// (aad = body_aad(view.msg.channel, view.msg.epoch, view.msg.id)).
pub fn open_body(key: &ChannelKey, view: &MessageView) -> Result<Vec<u8>, CliError> {
    let blob = unb64(&view.msg.body, "body")?;
    let aad = body_aad(&view.msg.channel, view.msg.epoch, &view.msg.id);
    Ok(key.open(&aad, &blob)?)
}

/// True iff the message's signature verifies (MessageWire::to_message + SignedMessage::verify).
pub fn verify_view(view: &MessageView) -> bool {
    view.msg
        .to_message()
        .ok()
        .is_some_and(|m| m.verify().is_ok())
}

/// `s` with every control character replaced by its `escape_default` form, so text from
/// a peer (names, bodies, labels) cannot move the cursor or rewrite the terminal.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else if matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
        ) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Exactly 64 lowercase hex -> Pub, else Usage("invalid public key: <s>").
pub fn parse_pk(s: &str) -> Result<Pub, CliError> {
    unhex32(s).ok_or_else(|| CliError::Usage(format!("invalid public key: {s}")))
}

/// Ask the daemon for our wrapped key (Req::ChannelKey) and unwrap it with
/// unwrap_key(wrapper, channel, epoch, wrapped). Returns (epoch, key).
pub async fn channel_key(
    client: &mut Client,
    id: &Identity,
    channel: &str,
    epoch: Option<i64>,
) -> Result<(i64, ChannelKey), CliError> {
    let reply = client
        .call(Req::ChannelKey {
            channel: channel.to_string(),
            epoch,
        })
        .await?;
    let Reply::Key {
        epoch: got,
        wrapper,
        wrapped,
    } = reply
    else {
        return Err(CliError::UnexpectedReply);
    };
    // A specific epoch was asked for: any other epoch is not the key we wanted.
    if epoch.is_some_and(|want| want != got) {
        return Err(CliError::UnexpectedReply);
    }
    let epoch = got;
    let wrapper = unhex_pk(&wrapper, "wrapper")?;
    let wrapped = unb64(&wrapped, "wrapped")?;
    let key = id.unwrap_key(&wrapper, channel, epoch, &wrapped)?;
    Ok((epoch, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_proto::b64;

    fn id(seed: u8) -> Identity {
        Identity::from_seed([seed; 32])
    }

    fn view(msg: MessageWire) -> MessageView {
        MessageView { seq: 1, msg }
    }

    fn is_lower_hex(s: &str) -> bool {
        s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    }

    #[test]
    fn nonce_is_32_lowercase_hex_chars_and_fresh() {
        let (a, b) = (nonce(), nonce());
        assert_eq!(a.len(), 32);
        assert!(is_lower_hex(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn new_id_is_prefix_dash_24_lowercase_hex_chars() {
        let s = new_id("m");
        let (prefix, hex_part) = s.split_once('-').expect("has a dash");
        assert_eq!(prefix, "m");
        assert_eq!(hex_part.len(), 24);
        assert!(is_lower_hex(hex_part));
        assert_ne!(new_id("m"), new_id("m"));
    }

    #[test]
    fn sign_op_round_trips_with_matching_fields_and_valid_signature() {
        let signer = id(3);
        let wire = sign_op(&signer, "card.create", "card-1", "{\"title\":\"x\"}");
        let op = wire.to_op().expect("decodes");
        assert_eq!(op.kind, "card.create");
        assert_eq!(op.subject, "card-1");
        assert_eq!(op.arg, "{\"title\":\"x\"}");
        assert_eq!(op.actor, signer.public());
        assert_eq!(op.nonce.len(), 32);
        assert!(op.ts.abs_diff(now_ms()) < 60_000);
        // The bytes the store's SignedOp covers, rebuilt here for verification.
        let bytes = agora_crypto::canonical(
            "op/v1",
            &[
                op.kind.as_bytes(),
                op.subject.as_bytes(),
                op.arg.as_bytes(),
                &op.ts.to_le_bytes(),
                op.nonce.as_bytes(),
                &op.actor,
            ],
        );
        agora_crypto::verify(&op.actor, &bytes, &op.sig).expect("signature verifies");
    }

    #[test]
    fn plain_message_verifies_and_detects_tampering() {
        let signer = id(4);
        let wire = plain_message(&signer, "c1", None, b"hello", vec![]);
        assert!(!wire.encrypted);
        assert_eq!(wire.epoch, 0);
        assert_eq!(wire.sender, hex(&signer.public()));
        assert!(verify_view(&view(wire.clone())));

        let mut tampered = wire;
        tampered.body = b64(b"HELLO");
        assert!(!verify_view(&view(tampered)));
    }

    #[test]
    fn sealed_message_decrypts_with_its_key_and_not_another() {
        let signer = id(5);
        let key = ChannelKey::generate().expect("rng");
        let wire = sealed_message(&signer, "c1", None, 2, &key, b"secret", vec![]).expect("seals");
        assert!(wire.encrypted);
        assert_eq!(wire.epoch, 2);
        assert!(verify_view(&view(wire.clone())));

        let v = view(wire);
        assert_eq!(open_body(&key, &v).expect("opens"), b"secret");
        let other = ChannelKey::generate().expect("rng");
        assert!(open_body(&other, &v).is_err());
    }

    #[test]
    fn wrap_for_output_unwraps_for_each_member() {
        let alice = id(6);
        let (bob, carol) = (id(7), id(8));
        let key = ChannelKey::generate().expect("rng");
        let wrapped =
            wrap_for(&alice, "c1", 4, &key, &[bob.public(), carol.public()]).expect("wraps");
        assert_eq!(wrapped.len(), 2);
        for (member, (pk, w)) in [&bob, &carol].into_iter().zip(wrapped.iter()) {
            assert_eq!(*pk, member.public());
            let unwrapped = member
                .unwrap_key(&alice.public(), "c1", 4, w)
                .expect("unwraps");
            assert!(unwrapped == key);
        }
    }

    #[test]
    fn sanitize_escapes_newlines_and_escape_sequences() {
        assert_eq!(sanitize("a\nb"), "a\\nb");
        assert_eq!(sanitize("\x1b[31mred"), "\\u{1b}[31mred");
        assert_eq!(sanitize("plain é"), "plain é");
    }

    #[test]
    fn sanitize_escapes_bidi_and_invisible_characters() {
        assert!(sanitize("a\u{202E}b").contains("\\u{202e}"));
        assert!(sanitize("x\u{200B}y").contains("\\u{200b}"));
    }
}
