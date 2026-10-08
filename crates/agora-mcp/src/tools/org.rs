//! Tool handlers: channel_create, channel_add, channel_remove, channel_rotate, channel_promote, channel_demote, role_grant, revoke.

use super::args::*;
use super::{ToolCx, ToolError, ToolOutput, ToolResult};
use agora_client::Out;
use agora_client::api::org::{
    self, ChannelAddParams, ChannelCreateParams, ChannelRemoveParams, ChannelRoleParams,
    ChannelRotateParams, RevokeParams, RoleGrantParams,
};

/// Refuse an empty required argument before the daemon sees it.
fn non_empty(field: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::usage(format!("{field} must not be empty")));
    }
    Ok(())
}

/// The tool result of a successful api call.
fn done(out: Out) -> ToolResult {
    Ok(ToolOutput {
        structured: out.json,
        text: out.text,
    })
}

/// Create a channel via `api::org::channel_create` (`members` are "PK" or "PK:admin"; `kind` default group; `encrypt` default false).
pub async fn channel_create(cx: &ToolCx<'_>, a: ChannelCreateArgs) -> ToolResult {
    non_empty("channel id", &a.id)?;
    non_empty("channel kind", &a.kind)?;
    done(
        org::channel_create(
            &cx.peer.ctx,
            ChannelCreateParams {
                id: a.id,
                kind: a.kind,
                name: a.name,
                parent: a.parent,
                members: a.members.unwrap_or_default(),
                encrypt: a.encrypt,
            },
        )
        .await?,
    )
}

/// Add a member via `api::org::channel_add`.
pub async fn channel_add(cx: &ToolCx<'_>, a: ChannelAddArgs) -> ToolResult {
    non_empty("channel", &a.channel)?;
    non_empty("pk", &a.pk)?;
    done(
        org::channel_add(
            &cx.peer.ctx,
            ChannelAddParams {
                channel: a.channel,
                pk: a.pk,
                admin: a.admin,
            },
        )
        .await?,
    )
}

/// Remove a member via `api::org::channel_remove` (destructive).
pub async fn channel_remove(cx: &ToolCx<'_>, a: ChannelRemoveArgs) -> ToolResult {
    non_empty("channel", &a.channel)?;
    non_empty("pk", &a.pk)?;
    done(
        org::channel_remove(
            &cx.peer.ctx,
            ChannelRemoveParams {
                channel: a.channel,
                pk: a.pk,
            },
        )
        .await?,
    )
}

/// New key epoch of an encrypted channel via `api::org::channel_rotate`.
pub async fn channel_rotate(cx: &ToolCx<'_>, a: ChannelRotateArgs) -> ToolResult {
    non_empty("channel", &a.channel)?;
    done(org::channel_rotate(&cx.peer.ctx, ChannelRotateParams { channel: a.channel }).await?)
}

/// Make a member an admin via `api::org::channel_role` with admin = true.
pub async fn channel_promote(cx: &ToolCx<'_>, a: ChannelPromoteArgs) -> ToolResult {
    non_empty("channel", &a.channel)?;
    non_empty("pk", &a.pk)?;
    done(
        org::channel_role(
            &cx.peer.ctx,
            ChannelRoleParams {
                channel: a.channel,
                pk: a.pk,
                admin: true,
            },
        )
        .await?,
    )
}

/// Make an admin a plain member via `api::org::channel_role` with admin = false (destructive).
pub async fn channel_demote(cx: &ToolCx<'_>, a: ChannelDemoteArgs) -> ToolResult {
    non_empty("channel", &a.channel)?;
    non_empty("pk", &a.pk)?;
    done(
        org::channel_role(
            &cx.peer.ctx,
            ChannelRoleParams {
                channel: a.channel,
                pk: a.pk,
                admin: false,
            },
        )
        .await?,
    )
}

/// Grant a role to a public key via `api::org::role_grant` (destructive: can lower privileges).
pub async fn role_grant(cx: &ToolCx<'_>, a: RoleGrantArgs) -> ToolResult {
    non_empty("pk", &a.pk)?;
    non_empty("role", &a.role)?;
    done(
        org::role_grant(
            &cx.peer.ctx,
            RoleGrantParams {
                pk: a.pk,
                role: a.role,
            },
        )
        .await?,
    )
}

/// Revoke an identity via `api::org::revoke` (destructive).
pub async fn revoke(cx: &ToolCx<'_>, a: RevokeArgs) -> ToolResult {
    non_empty("pk", &a.pk)?;
    done(org::revoke(&cx.peer.ctx, RevokeParams { pk: a.pk }).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_required_argument_is_a_usage_error() {
        let err = non_empty("channel", "").expect_err("empty must be refused");
        assert_eq!(err.code, "usage");
        assert!(non_empty("channel", "general").is_ok());
    }
}
