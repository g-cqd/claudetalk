//! init, whoami, stop, channel *, role grant, revoke (see SPEC "CLI").

use crate::client::Client;
use crate::identity::current_folder;
use crate::ops::{channel_key, parse_pk, sanitize, sign_op, wrap_for};
use crate::{ChannelCmd, CliError, Ctx, InitArgs, Out, RevokeArgs, RoleCmd};
use agora_crypto::ChannelKey;
use agora_proto::{ChannelView, MemberView, ProtoError, Reply, Req, b64, keys_to_wire, unhex_pk};
use agora_store::{Pub, hex, keys_digest};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long `stop` waits for the daemon to release its lock after Shutdown.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval between probes while `stop` waits.
const STOP_POLL: Duration = Duration::from_millis(25);

pub async fn init(ctx: &Ctx, a: InitArgs) -> Result<Out, CliError> {
    if a.owner && a.label.is_some() {
        return Err(CliError::Usage(
            "--label cannot be combined with --owner (the owner's label is \"owner\")".into(),
        ));
    }
    let mut client = Client::connect(ctx).await?;
    if a.owner {
        expect_unit(client.call(Req::BootstrapOwner).await?)?;
    } else {
        let label = a.label.unwrap_or_else(|| ctx.label.clone());
        expect_unit(client.call(Req::Register { label }).await?)?;
    }
    whoami_of(&mut client).await
}

pub async fn whoami(ctx: &Ctx) -> Result<Out, CliError> {
    let mut client = Client::connect(ctx).await?;
    whoami_of(&mut client).await
}

async fn whoami_of(client: &mut Client) -> Result<Out, CliError> {
    let Reply::Whoami {
        pubkey,
        pseudonym,
        role,
        registered,
        revoked,
        owner_exists,
    } = client.call(Req::Whoami).await?
    else {
        return Err(CliError::UnexpectedReply);
    };
    let folder = current_folder()?.to_string_lossy().into_owned();
    let text = format!(
        "{} {pubkey}\nrole: {}\nregistered: {}\nrevoked: {}\nowner exists: {}\nfolder: {}",
        sanitize(&pseudonym),
        role.as_deref().unwrap_or("none"),
        yes_no(registered),
        yes_no(revoked),
        yes_no(owner_exists),
        sanitize(&folder),
    );
    Ok(Out {
        json: json!({
            "pubkey": pubkey,
            "pseudonym": pseudonym,
            "role": role,
            "registered": registered,
            "revoked": revoked,
            "owner_exists": owner_exists,
            "folder": folder,
        }),
        text,
    })
}

pub async fn stop(ctx: &Ctx) -> Result<Out, CliError> {
    let mut client = match Client::connect_no_spawn(&ctx.paths, &ctx.identity, &ctx.label).await {
        Ok(client) => client,
        Err(CliError::Daemon(_)) => return Ok(stopped(false)),
        Err(e) => return Err(e),
    };
    expect_unit(client.call(Req::Shutdown).await?)?;
    drop(client);
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        // The daemon holds its lock until it has exited, so a lock we can take means it is gone.
        if lock_released(&ctx.paths.lock)? {
            return Ok(stopped(true));
        }
        if Instant::now() >= deadline {
            return Err(CliError::Daemon("still running 5 s after shutdown".into()));
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

/// True when no process holds the daemon's lock (or the lock file does not exist).
/// The probe lock is released when the file handle drops at the end of this function.
fn lock_released(path: &Path) -> Result<bool, CliError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

pub async fn channel(ctx: &Ctx, c: ChannelCmd) -> Result<Out, CliError> {
    match c {
        ChannelCmd::Create {
            id,
            kind,
            name,
            parent,
            members,
            encrypt,
        } => create(ctx, id, kind, name, parent, members, encrypt).await,
        ChannelCmd::Add { channel, pk, admin } => add(ctx, channel, pk, admin).await,
        ChannelCmd::Remove { channel, pk } => remove(ctx, channel, pk).await,
        ChannelCmd::Promote { channel, pk } => set_role(ctx, channel, pk, "admin").await,
        ChannelCmd::Demote { channel, pk } => set_role(ctx, channel, pk, "member").await,
        ChannelCmd::Rotate { channel } => rotate(ctx, channel).await,
        ChannelCmd::List => list(ctx).await,
        ChannelCmd::Info { channel } => info(ctx, channel).await,
    }
}

async fn create(
    ctx: &Ctx,
    id: String,
    kind: String,
    name: String,
    parent: Option<String>,
    members: Vec<String>,
    encrypt: bool,
) -> Result<Out, CliError> {
    let members = members
        .iter()
        .map(|m| parse_member(m.as_str()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut arg = Map::new();
    arg.insert("kind".into(), json!(kind));
    arg.insert("name".into(), json!(name));
    if let Some(parent) = parent {
        arg.insert("parent".into(), json!(parent));
    }
    arg.insert(
        "members".into(),
        json!(
            members
                .iter()
                .map(|(pk, role)| json!({"pk": hex(pk), "role": role}))
                .collect::<Vec<Value>>()
        ),
    );
    // The creator is always a member, so the key is wrapped for it too.
    let keys = if encrypt {
        let key = ChannelKey::generate()?;
        let set = with_self(
            members.iter().map(|(pk, _)| *pk).collect(),
            ctx.identity.public(),
        );
        let keys = wrap_for(&ctx.identity, &id, 0, &key, &set)?;
        arg.insert("keys".into(), json!(keys_digest(0, &keys)));
        keys
    } else {
        Vec::new()
    };
    let mut client = Client::connect(ctx).await?;
    let op = sign_op(
        &ctx.identity,
        "channel.create",
        &id,
        &Value::Object(arg).to_string(),
    );
    expect_unit(
        client
            .call(Req::ChannelCreate {
                op,
                keys: keys_to_wire(&keys),
            })
            .await?,
    )?;
    Ok(Out {
        json: json!({"channel": id, "encrypted": encrypt}),
        text: format!("created channel {id}"),
    })
}

async fn add(ctx: &Ctx, channel: String, pk: String, admin: bool) -> Result<Out, CliError> {
    let target = parse_pk(&pk)?;
    let role = if admin { "admin" } else { "member" };
    let mut client = Client::connect(ctx).await?;
    let (info, _) = channel_info(&mut client, &channel).await?;
    let mut arg = Map::new();
    arg.insert("pk".into(), json!(hex(&target)));
    arg.insert("role".into(), json!(role));
    let wrapped = if info.encrypted {
        let (epoch, key) = channel_key(&mut client, &ctx.identity, &channel, None).await?;
        let mut keys = wrap_for(&ctx.identity, &channel, epoch, &key, &[target])?;
        arg.insert("keys".into(), json!(keys_digest(epoch, &keys)));
        let (_, wrapped) = keys.pop().ok_or(CliError::UnexpectedReply)?;
        Some(b64(&wrapped))
    } else {
        None
    };
    let op = sign_op(
        &ctx.identity,
        "channel.add",
        &channel,
        &Value::Object(arg).to_string(),
    );
    expect_unit(client.call(Req::ChannelAdd { op, wrapped }).await?)?;
    Ok(Out {
        json: json!({"channel": channel, "pk": hex(&target), "role": role}),
        text: format!("added {pk} to {channel} as {role}"),
    })
}

/// How `channel remove` binds key material for its target.
#[derive(Debug, PartialEq, Eq)]
enum RemoveKeys {
    /// Plain channel: no key material.
    Plain,
    /// Another member leaves an encrypted channel: a new key at epoch+1 for the remaining members.
    Rotate,
    /// We leave an encrypted channel: no key material; the remaining members must rotate.
    SelfLeave,
}

/// Decides the key handling for removing `target` from a channel. `me` is our own public key.
fn remove_keys(encrypted: bool, target: &Pub, me: &Pub) -> RemoveKeys {
    match (encrypted, target == me) {
        (false, _) => RemoveKeys::Plain,
        (true, false) => RemoveKeys::Rotate,
        (true, true) => RemoveKeys::SelfLeave,
    }
}

async fn remove(ctx: &Ctx, channel: String, pk: String) -> Result<Out, CliError> {
    let target = parse_pk(&pk)?;
    let me = ctx.identity.public();
    let mut client = Client::connect(ctx).await?;
    let (info, members) = channel_info(&mut client, &channel).await?;
    let mode = remove_keys(info.encrypted, &target, &me);
    let mut arg = Map::new();
    arg.insert("pk".into(), json!(hex(&target)));
    let keys = match mode {
        RemoveKeys::Rotate => {
            let remaining: Vec<Pub> = member_pks(&members)?
                .into_iter()
                .filter(|m| *m != target)
                .collect();
            let next = next_epoch(info.epoch)?;
            let key = ChannelKey::generate()?;
            let keys = wrap_for(&ctx.identity, &channel, next, &key, &remaining)?;
            arg.insert("keys".into(), json!(keys_digest(next, &keys)));
            keys
        }
        RemoveKeys::Plain | RemoveKeys::SelfLeave => Vec::new(),
    };
    let op = sign_op(
        &ctx.identity,
        "channel.remove",
        &channel,
        &Value::Object(arg).to_string(),
    );
    expect_unit(
        client
            .call(Req::ChannelRemove {
                op,
                keys: keys_to_wire(&keys),
            })
            .await?,
    )?;
    let rotation_required = mode == RemoveKeys::SelfLeave;
    let mut body = json!({"channel": channel, "pk": hex(&target), "removed": true});
    let text = if rotation_required {
        body["rotation_required"] = json!(true);
        format!(
            "removed {pk} from {channel}\nremaining members must run `agora channel rotate {channel}`"
        )
    } else {
        format!("removed {pk} from {channel}")
    };
    Ok(Out { json: body, text })
}

async fn set_role(
    ctx: &Ctx,
    channel: String,
    pk: String,
    role: &'static str,
) -> Result<Out, CliError> {
    let target = parse_pk(&pk)?;
    let mut client = Client::connect(ctx).await?;
    let arg = json!({"pk": hex(&target), "role": role}).to_string();
    let op = sign_op(&ctx.identity, "channel.role", &channel, &arg);
    expect_unit(client.call(Req::ChannelRole { op }).await?)?;
    Ok(Out {
        json: json!({"channel": channel, "pk": hex(&target), "role": role}),
        text: format!("{pk} is {role} in {channel}"),
    })
}

async fn rotate(ctx: &Ctx, channel: String) -> Result<Out, CliError> {
    let mut client = Client::connect(ctx).await?;
    let (info, members) = channel_info(&mut client, &channel).await?;
    if !info.encrypted {
        return Err(CliError::Usage("channel is not encrypted".into()));
    }
    let next = next_epoch(info.epoch)?;
    let key = ChannelKey::generate()?;
    let keys = wrap_for(&ctx.identity, &channel, next, &key, &member_pks(&members)?)?;
    let arg = json!({"keys": keys_digest(next, &keys)}).to_string();
    let op = sign_op(&ctx.identity, "channel.rotate", &channel, &arg);
    let epoch = match client
        .call(Req::ChannelRotate {
            op,
            keys: keys_to_wire(&keys),
        })
        .await?
    {
        Reply::Epoch { epoch } => epoch,
        _ => return Err(CliError::UnexpectedReply),
    };
    Ok(Out {
        json: json!({"epoch": epoch}),
        text: format!("{channel} rotated to epoch {epoch}"),
    })
}

async fn list(ctx: &Ctx) -> Result<Out, CliError> {
    let mut client = Client::connect(ctx).await?;
    let Reply::Channels { channels } = client.call(Req::ChannelList).await? else {
        return Err(CliError::UnexpectedReply);
    };
    let text = channels
        .iter()
        .map(channel_line)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Out {
        json: json!({"channels": to_json(&channels)?}),
        text,
    })
}

async fn info(ctx: &Ctx, channel: String) -> Result<Out, CliError> {
    let mut client = Client::connect(ctx).await?;
    let (view, members) = channel_info(&mut client, &channel).await?;
    let mut lines = vec![channel_line(&view)];
    lines.extend(members.iter().map(|m| {
        format!(
            "  {} {} {}",
            sanitize(&m.role),
            sanitize(&m.pseudonym),
            m.pk
        )
    }));
    Ok(Out {
        json: json!({"channel": to_json(&view)?, "members": to_json(&members)?}),
        text: lines.join("\n"),
    })
}

pub async fn role(ctx: &Ctx, c: RoleCmd) -> Result<Out, CliError> {
    match c {
        RoleCmd::Grant { pk, role } => {
            let target = parse_pk(&pk)?;
            let mut client = Client::connect(ctx).await?;
            let op = sign_op(&ctx.identity, "role.grant", &hex(&target), &role);
            expect_unit(client.call(Req::RoleGrant { op }).await?)?;
            Ok(Out {
                json: json!({"pk": pk, "role": role}),
                text: format!("granted {role} to {pk}"),
            })
        }
    }
}

pub async fn revoke(ctx: &Ctx, a: RevokeArgs) -> Result<Out, CliError> {
    let target = parse_pk(&a.pk)?;
    let mut client = Client::connect(ctx).await?;
    let op = sign_op(&ctx.identity, "identity.revoke", &hex(&target), "");
    expect_unit(client.call(Req::Revoke { op }).await?)?;
    Ok(Out {
        json: json!({"pk": a.pk, "revoked": true}),
        text: format!("revoked {}", a.pk),
    })
}

/// `PK` (role member) or `PK:admin`; anything else after the key is a usage error.
fn parse_member(s: &str) -> Result<(Pub, &'static str), CliError> {
    match s.split_once(':') {
        None => Ok((parse_pk(s)?, "member")),
        Some((pk, "admin")) => Ok((parse_pk(pk)?, "admin")),
        Some(_) => Err(CliError::Usage(format!(
            "member must be PK or PK:admin: {s}"
        ))),
    }
}

/// `members` plus `me`, sorted and without duplicates: the set a new key is wrapped for.
fn with_self(members: Vec<Pub>, me: Pub) -> Vec<Pub> {
    members
        .into_iter()
        .chain(std::iter::once(me))
        .collect::<BTreeSet<Pub>>()
        .into_iter()
        .collect()
}

fn next_epoch(epoch: i64) -> Result<i64, CliError> {
    epoch
        .checked_add(1)
        .ok_or_else(|| CliError::Usage("channel key epoch overflow".into()))
}

/// Ask the daemon for a channel's view and members (Req::ChannelInfo).
async fn channel_info(
    client: &mut Client,
    channel: &str,
) -> Result<(ChannelView, Vec<MemberView>), CliError> {
    match client
        .call(Req::ChannelInfo {
            channel: channel.to_string(),
        })
        .await?
    {
        Reply::Channel { channel, members } => Ok((channel, members)),
        _ => Err(CliError::UnexpectedReply),
    }
}

fn member_pks(members: &[MemberView]) -> Result<Vec<Pub>, CliError> {
    members
        .iter()
        .map(|m| unhex_pk(&m.pk, "pubkey").map_err(CliError::from))
        .collect()
}

fn channel_line(c: &ChannelView) -> String {
    let encryption = if c.encrypted { "encrypted" } else { "plain" };
    let pending = if c.rotation_pending {
        " rotation-pending"
    } else {
        ""
    };
    let role = c
        .my_role
        .as_deref()
        .map(|r| format!(" my-role={r}"))
        .unwrap_or_default();
    format!(
        "{} {} epoch {} {encryption}{pending}{role}",
        sanitize(&c.id),
        sanitize(&c.kind),
        c.epoch
    )
}

fn to_json<T: Serialize>(v: &T) -> Result<Value, CliError> {
    Ok(serde_json::to_value(v).map_err(ProtoError::from)?)
}

fn expect_unit(reply: Reply) -> Result<(), CliError> {
    match reply {
        Reply::Unit => Ok(()),
        _ => Err(CliError::UnexpectedReply),
    }
}

fn stopped(stopped: bool) -> Out {
    Out {
        json: json!({"stopped": stopped}),
        text: if stopped {
            "stopped".into()
        } else {
            "not running".into()
        },
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk_hex(seed: u8) -> String {
        hex(&[seed; 32])
    }

    #[test]
    fn bare_pk_is_a_member() {
        let (pk, role) = parse_member(&pk_hex(1)).expect("parses");
        assert_eq!(pk, [1u8; 32]);
        assert_eq!(role, "member");
    }

    #[test]
    fn admin_suffix_makes_an_admin() {
        let (pk, role) = parse_member(&format!("{}:admin", pk_hex(2))).expect("parses");
        assert_eq!(pk, [2u8; 32]);
        assert_eq!(role, "admin");
    }

    #[test]
    fn other_suffixes_and_bad_keys_are_usage_errors() {
        let bad = [
            format!("{}:member", pk_hex(1)),
            format!("{}:", pk_hex(1)),
            format!("{}:admin:x", pk_hex(1)),
            pk_hex(0xab).to_uppercase(),
            "not-a-key".to_string(),
            format!("{}:admin", "a".repeat(63)),
        ];
        for s in bad {
            let err = parse_member(&s).expect_err("must be rejected");
            assert!(matches!(err, CliError::Usage(_)), "{s}: got {err:?}");
        }
    }

    #[test]
    fn wrap_set_adds_self_once_and_sorts() {
        let (me, other) = ([1u8; 32], [2u8; 32]);
        assert_eq!(with_self(vec![other, me, other], me), vec![me, other]);
        assert_eq!(with_self(vec![], me), vec![me]);
    }

    #[test]
    fn next_epoch_is_plus_one_and_refuses_overflow() {
        assert_eq!(next_epoch(4).expect("next"), 5);
        assert!(next_epoch(i64::MAX).is_err());
    }

    #[test]
    fn remove_keys_plain_channel_carries_no_keys() {
        let (me, other) = ([1u8; 32], [2u8; 32]);
        assert_eq!(remove_keys(false, &other, &me), RemoveKeys::Plain);
        assert_eq!(remove_keys(false, &me, &me), RemoveKeys::Plain);
    }

    #[test]
    fn remove_keys_encrypted_removal_of_another_member_rotates() {
        let (me, other) = ([1u8; 32], [2u8; 32]);
        assert_eq!(remove_keys(true, &other, &me), RemoveKeys::Rotate);
    }

    #[test]
    fn remove_keys_encrypted_self_leave_sends_no_keys_and_requires_rotation() {
        let me = [1u8; 32];
        assert_eq!(remove_keys(true, &me, &me), RemoveKeys::SelfLeave);
    }

    #[test]
    fn lock_is_released_when_missing_unheld_or_after_the_holder_drops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agora.lock");
        assert!(lock_released(&path).expect("missing file"));
        let holder = File::create(&path).expect("create");
        holder.try_lock().expect("hold");
        assert!(!lock_released(&path).expect("held"));
        drop(holder);
        assert!(lock_released(&path).expect("released"));
    }
}
