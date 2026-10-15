//! init, whoami, stop, channel *, role grant, revoke: clap args -> `agora_client::api::org` -> Out.

use crate::{ChannelCmd, CliError, Ctx, InitArgs, Out, RevokeArgs, RoleCmd, StatusCmd};
use agora_client::api::org::{
    self as api, ChannelAddParams, ChannelCreateParams, ChannelInfoParams, ChannelRemoveParams,
    ChannelRoleParams, ChannelRotateParams, InitParams, RevokeParams, RoleGrantParams,
};
use agora_client::api::status::{self as status_api, StatusClearParams, StatusSetParams};
use agora_client::identity::current_folder;
use agora_client::ops::sanitize;
use serde_json::json;

pub async fn init(ctx: &Ctx, a: InitArgs) -> Result<Out, CliError> {
    let out = api::init(
        ctx,
        InitParams {
            owner: a.owner,
            label: a.label,
        },
    )
    .await?;
    with_folder(out)
}

pub async fn whoami(ctx: &Ctx) -> Result<Out, CliError> {
    with_folder(api::whoami(ctx).await?)
}

/// The CLI shows this folder's path in `whoami` and `init`: the JSON key `folder` and a last
/// text line `folder: <path>`. The api verbs do not read the process folder state.
fn with_folder(mut out: Out) -> Result<Out, CliError> {
    let folder = current_folder()?.to_string_lossy().into_owned();
    out.text = format!("{}\nfolder: {}", out.text, sanitize(&folder));
    out.json["folder"] = json!(folder);
    Ok(out)
}

pub async fn stop(ctx: &Ctx) -> Result<Out, CliError> {
    api::stop(ctx).await
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
        } => {
            api::channel_create(
                ctx,
                ChannelCreateParams {
                    id,
                    kind,
                    name,
                    parent,
                    members,
                    encrypt,
                },
            )
            .await
        }
        ChannelCmd::Add { channel, pk, admin } => {
            api::channel_add(ctx, ChannelAddParams { channel, pk, admin }).await
        }
        ChannelCmd::Remove { channel, pk } => {
            api::channel_remove(ctx, ChannelRemoveParams { channel, pk }).await
        }
        ChannelCmd::Promote { channel, pk } => {
            api::channel_role(
                ctx,
                ChannelRoleParams {
                    channel,
                    pk,
                    admin: true,
                },
            )
            .await
        }
        ChannelCmd::Demote { channel, pk } => {
            api::channel_role(
                ctx,
                ChannelRoleParams {
                    channel,
                    pk,
                    admin: false,
                },
            )
            .await
        }
        ChannelCmd::Rotate { channel } => {
            api::channel_rotate(ctx, ChannelRotateParams { channel }).await
        }
        ChannelCmd::List => api::channel_list(ctx).await,
        ChannelCmd::Info { channel } => api::channel_info(ctx, ChannelInfoParams { channel }).await,
    }
}

pub async fn role(ctx: &Ctx, c: RoleCmd) -> Result<Out, CliError> {
    match c {
        RoleCmd::Grant { pk, role } => api::role_grant(ctx, RoleGrantParams { pk, role }).await,
    }
}

pub async fn revoke(ctx: &Ctx, a: RevokeArgs) -> Result<Out, CliError> {
    api::revoke(ctx, RevokeParams { pk: a.pk }).await
}

/// `agora status set|clear|list`. The CLI fills `harness` with "cli" and
/// `folder` with the current folder when the caller does not set them, so the
/// status names where the command ran.
pub async fn status(ctx: &Ctx, c: StatusCmd) -> Result<Out, CliError> {
    match c {
        StatusCmd::Set {
            state,
            harness,
            folder,
            matter,
        } => {
            let folder = match folder {
                Some(f) => f,
                None => current_folder()?.to_string_lossy().into_owned(),
            };
            status_api::status_set(
                ctx,
                StatusSetParams {
                    state,
                    harness: Some(harness.unwrap_or_else(|| "cli".into())),
                    folder: Some(folder),
                    matter,
                },
            )
            .await
        }
        StatusCmd::Clear { harness } => {
            status_api::status_clear(
                ctx,
                StatusClearParams {
                    harness: Some(harness.unwrap_or_else(|| "cli".into())),
                },
            )
            .await
        }
        StatusCmd::List => status_api::statuses(ctx).await,
    }
}
