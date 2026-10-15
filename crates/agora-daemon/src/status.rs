//! Presence status: StatusSet, StatusClear, Statuses (see SPEC "Handlers: status").

use crate::dispatch::{daemon_error, op_from_wire, unrouted};
use crate::{Conn, Shared, now_ms};
use agora_proto::{ErrorBody, Reply, Req, StatusView};
use agora_store::{StatusRow, hex};
use std::collections::BTreeSet;

/// Handles StatusSet, StatusClear, Statuses; anything else -> `Err(dispatch::unrouted())`.
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    match req {
        Req::StatusSet { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = now_ms();
            sh.writer
                .exec(move |s| {
                    s.set_status(now, &op)?;
                    Ok(((), Vec::new()))
                })
                .await
                .map_err(daemon_error)?;
            Ok(Reply::Unit)
        }
        Req::StatusClear { op } => {
            let op = op_from_wire(conn, &op)?;
            let now = now_ms();
            sh.writer
                .exec(move |s| {
                    s.clear_status(now, &op)?;
                    Ok(((), Vec::new()))
                })
                .await
                .map_err(daemon_error)?;
            Ok(Reply::Unit)
        }
        Req::Statuses => {
            let me = conn.pubkey;
            let rows = sh
                .reader
                .exec(move |s| s.statuses(&me))
                .await
                .map_err(daemon_error)?;
            let online: BTreeSet<String> = sh
                .presence
                .online()
                .into_iter()
                .map(|pk| hex(&pk))
                .collect();
            let statuses = rows
                .iter()
                .map(|r| view(r, online.contains(&r.pk)))
                .collect();
            Ok(Reply::Statuses { statuses })
        }
        _ => Err(unrouted()),
    }
}

/// One store row plus the daemon's presence flag.
fn view(r: &StatusRow, online: bool) -> StatusView {
    StatusView {
        pk: r.pk.clone(),
        pseudonym: r.pseudonym.clone(),
        state: r.state.clone(),
        harness: r.harness.clone(),
        folder: r.folder.clone(),
        matter: r.matter.clone(),
        ts: r.ts,
        online,
    }
}
