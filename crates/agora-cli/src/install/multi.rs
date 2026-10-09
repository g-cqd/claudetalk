//! `--client a,b,c|all`: runs the per-client installers and merges their capabilities entries
//! (SPEC 4.1, 4.3 last paragraph, 4.4).

use super::claude::{self, Action, Options, Scope};
use super::client::Client;
use super::paths::ClientPaths;
use super::probe_clients::{self, ClientProbe};
use super::{
    PROJECT_NOTE, check_capabilities_target, ensure_home, install_claude, render_actions,
    scope_name, uninstall_claude,
};
use super::{agy, codex, copilot_cli, cursor, opencode, vscode};
use crate::{CliError, Out};
use agora_client::capabilities::{self, CapabilitiesFile, HarnessCapabilities};
use agora_daemon::{Paths, now_ms};
use merge::{apply_probe, forget_entry, record_entry, write_capabilities};
use serde_json::{Value, json};

mod merge;

/// Everything one client module needs for one run.
pub struct Ctx<'a> {
    pub o: &'a Options,
    pub paths: &'a ClientPaths,
    /// `--trust-hooks` (Codex only).
    pub trust_hooks: bool,
}

/// What one client module did: its actions plus the capabilities entry describing THIS run
/// (flags and files; `multi` merges it into the existing entry).
#[derive(Debug, Clone, Default)]
pub struct ClientReport {
    pub actions: Vec<Action>,
    pub caps: HarnessCapabilities,
    pub notes: Vec<String>,
}

impl Ctx<'_> {
    /// `"user"` or `"project:<absolute project dir>"` (the form stored in `HarnessCapabilities::scopes`).
    pub fn scope_label(&self) -> String {
        match self.o.scope {
            claude::Scope::User => "user".to_owned(),
            claude::Scope::Project => format!("project:{}", self.o.project_dir.display()),
        }
    }

    /// The agora executable as UTF-8 (`CliError::Usage` otherwise).
    pub fn exe_str(&self) -> Result<&str, CliError> {
        self.o
            .exe
            .to_str()
            .ok_or_else(|| CliError::Usage("the agora binary path is not valid UTF-8".into()))
    }

    /// The agora home as UTF-8 (`CliError::Usage` otherwise).
    pub fn home_str(&self) -> Result<&str, CliError> {
        self.o
            .agora_home
            .to_str()
            .ok_or_else(|| CliError::Usage("the agora home path is not valid UTF-8".into()))
    }

    /// The MCP server args every client uses: `["--home", "<home>", "mcp"]`.
    pub fn mcp_args(&self) -> Result<Vec<String>, CliError> {
        Ok(vec!["--home".into(), self.home_str()?.into(), "mcp".into()])
    }

    /// `"<exe> --home <home> hook <kind> --dialect <dialect>"` plus ` --event <event>` when `event`
    /// is Some; the exe and home go through `sh_arg`. `kind` is `session-start`, `context` or `stop`.
    pub fn hook_cmd(
        &self,
        kind: &str,
        dialect: &str,
        event: Option<&str>,
    ) -> Result<String, CliError> {
        let mut cmd = format!(
            "{} --home {} hook {kind} --dialect {dialect}",
            sh_arg(self.exe_str()?),
            sh_arg(self.home_str()?)
        );
        if let Some(e) = event {
            cmd.push_str(" --event ");
            cmd.push_str(e);
        }
        Ok(cmd)
    }
}

/// Install every client of `clients` (never empty) in order. A list that is exactly `[Claude]` is
/// NOT handled here (`install::install` keeps today's path). For each client: Claude ->
/// `super::install_claude` and its `Out` embedded as is; others -> the client module's
/// `install(&Ctx)`, then (unless `o.dry_run`) the capabilities entry is merged and
/// `capabilities.json` written (scopes plus this scope, files plus the written paths, flags from the
/// report, probe fields from `probe_clients::probe` when `probe`; the home is created like
/// `install_claude` does). A failing client does NOT stop the others: its entry is
/// `{"client":..,"error":{"code","message"}}`. JSON `{"ok":<no client failed>,"clients":[<per-client
/// out>..],"capabilities_actions":[..]}`; per-client out for non-Claude: `{"client","scope","dry_run",
/// "actions","probe","capabilities","notes"}`. Text: one block per client. `main` prints an `Out` whose
/// JSON has `"ok": false` in full and then exits non-zero, so the function returns `Ok` even when
/// clients failed.
pub fn install(
    o: &Options,
    clients: &[Client],
    probe: bool,
    trust_hooks: bool,
) -> Result<Out, CliError> {
    install_with(o, clients, probe, trust_hooks, &process_paths(o)?)
}

/// Uninstall every client of `clients`: Claude -> `super::uninstall_claude`; others -> the client
/// module's `uninstall(&Ctx, &CapabilitiesFile)` (caps loaded once, before any removal, with every
/// client of this list already taken out, so a shared file is removed when all its users go), then
/// the capabilities entry loses this scope (`scopes` minus it; the entry is removed when none is
/// left; `files` of that scope removed). Same failure rule and output shape as `install`.
pub fn uninstall(o: &Options, clients: &[Client]) -> Result<Out, CliError> {
    uninstall_with(o, clients, &process_paths(o)?)
}

/// The client path table for this process: the user home and project from `o`, the environment
/// (`CODEX_HOME`, `COPILOT_HOME`, `XDG_CONFIG_HOME`, `AGORA_VSCODE_USER_DIR`) read here. A relative
/// value of one of them is a usage error, returned before any client is touched.
fn process_paths(o: &Options) -> Result<ClientPaths, CliError> {
    ClientPaths::from_env(&o.user_home, &o.project_dir, &|k| std::env::var_os(k))
}

/// `install` with the client paths passed in, so tests need no process environment.
fn install_with(
    o: &Options,
    clients: &[Client],
    probe: bool,
    trust_hooks: bool,
    paths: &ClientPaths,
) -> Result<Out, CliError> {
    let now = now_ms();
    if !o.dry_run {
        // Before any client writes: the home must exist, and a symlinked capabilities.json must
        // fail before anything is written.
        ensure_home(&o.agora_home)?;
        check_capabilities_target(&Paths::new(&o.agora_home))?;
    }
    let mut items = Vec::with_capacity(clients.len());
    let mut done = Vec::new();
    for &client in clients {
        if client == Client::Claude {
            items.push(match install_claude(o, probe) {
                Ok(out) => claude_item(o, out),
                Err(e) => Item::failed(o, client, &e),
            });
            continue;
        }
        let ctx = Ctx {
            o,
            paths,
            trust_hooks,
        };
        match install_module(client, &ctx) {
            Ok(mut report) => {
                if o.scope == Scope::Project && !o.dry_run {
                    report.notes.push(PROJECT_NOTE.to_owned());
                }
                let found = probe.then(|| probe_clients::probe(client));
                if let Some(p) = &found {
                    apply_probe(client, &mut report.caps, p, now);
                }
                report.caps.notes = report.notes.clone();
                items.push(client_item(
                    o,
                    client,
                    &report,
                    found.as_ref(),
                    Some(&report.caps),
                ));
                done.push(Done {
                    client,
                    scope: ctx.scope_label(),
                    report,
                    probed: found.is_some(),
                });
            }
            Err(e) => items.push(Item::failed(o, client, &e)),
        }
    }
    let mut ok = items.iter().all(|i| !i.failed);
    let mut caps_actions = Vec::new();
    if !o.dry_run && !done.is_empty() {
        let detail = format!("recorded: {}", names(&done));
        let (actions, written) = write_capabilities(o, now, detail, |file| {
            for d in &done {
                record_entry(file, d);
            }
        });
        caps_actions = actions;
        ok &= written;
    }
    Ok(finish(o, items, caps_actions, ok))
}

/// `uninstall` with the client paths passed in, so tests need no process environment.
fn uninstall_with(o: &Options, clients: &[Client], paths: &ClientPaths) -> Result<Out, CliError> {
    let now = now_ms();
    // The shared-file decisions of each module see the state before this run, with every client of
    // this list already gone: a file shared only by clients of this list is removed.
    let mut caps_for_client = capabilities::load(&Paths::new(&o.agora_home)).unwrap_or_default();
    for &client in clients {
        caps_for_client.set_harness(client.harness(), None);
    }
    let mut items = Vec::with_capacity(clients.len());
    let mut done = Vec::new();
    for &client in clients {
        if client == Client::Claude {
            items.push(match uninstall_claude(o) {
                Ok(out) => claude_item(o, out),
                Err(e) => Item::failed(o, client, &e),
            });
            continue;
        }
        let ctx = Ctx {
            o,
            paths,
            trust_hooks: false,
        };
        match uninstall_module(client, &ctx, &caps_for_client) {
            Ok(mut report) => {
                // A module that reports no files still leaves this scope: its files are the ones
                // the scope's targets name, so they leave the entry with it.
                if report.caps.files.is_empty() {
                    report.caps.files = scope_file_strings(paths, client, o.scope);
                }
                items.push(client_item(o, client, &report, None, None));
                done.push(Done {
                    client,
                    scope: ctx.scope_label(),
                    report,
                    probed: false,
                });
            }
            Err(e) => items.push(Item::failed(o, client, &e)),
        }
    }
    let mut ok = items.iter().all(|i| !i.failed);
    let mut caps_actions = Vec::new();
    if !o.dry_run && !done.is_empty() {
        let detail = format!("removed: {}", names(&done));
        let (actions, written) = write_capabilities(o, now, detail, |file| {
            for d in &done {
                forget_entry(file, d);
            }
        });
        caps_actions = actions;
        ok &= written;
    }
    Ok(finish(o, items, caps_actions, ok))
}

/// A non-Claude client that succeeded in this run, kept until `capabilities.json` is written.
struct Done {
    client: Client,
    /// `Ctx::scope_label` of the run.
    scope: String,
    report: ClientReport,
    /// `--probe` ran for this client.
    probed: bool,
}

/// One client's entry of the output: its JSON, its text block and whether it failed.
struct Item {
    json: Value,
    block: Vec<String>,
    failed: bool,
}

impl Item {
    /// `{"client", "error": {"code", "message"}}`, the block shows the message.
    fn failed(o: &Options, client: Client, e: &CliError) -> Item {
        Item {
            json: json!({
                "client": client.as_str(),
                "error": {"code": e.code(), "message": e.to_string()},
            }),
            block: vec![header(o, client), format!("error: {e}")],
            failed: true,
        }
    }
}

/// The text header of one client's block.
fn header(o: &Options, client: Client) -> String {
    format!("== {} ({}) ==", client.as_str(), scope_name(o.scope))
}

/// Claude's own `Out` embedded as is; its text becomes the block body.
fn claude_item(o: &Options, out: Out) -> Item {
    let mut block = vec![header(o, Client::Claude)];
    block.extend(out.text.lines().map(str::to_owned));
    Item {
        json: out.json,
        block,
        failed: false,
    }
}

/// The output entry of a successful non-Claude client.
fn client_item(
    o: &Options,
    client: Client,
    report: &ClientReport,
    probe: Option<&ClientProbe>,
    caps: Option<&HarnessCapabilities>,
) -> Item {
    let mut block = vec![header(o, client)];
    block.extend(render_actions(&report.actions));
    block.extend(report.notes.iter().map(|n| format!("note: {n}")));
    if let Some(p) = probe {
        block.extend(p.notes.iter().map(|n| format!("note: {n}")));
    }
    Item {
        json: json!({
            "client": client.as_str(),
            "scope": scope_name(o.scope),
            "dry_run": o.dry_run,
            "actions": report.actions,
            "probe": probe,
            "capabilities": caps,
            "notes": report.notes,
        }),
        block,
        failed: false,
    }
}

/// The whole output: `{"ok", "clients", "capabilities_actions"}`; the text is one block per client,
/// then the capabilities actions when there are any.
fn finish(o: &Options, items: Vec<Item>, caps_actions: Vec<Action>, ok: bool) -> Out {
    let (json_items, mut blocks): (Vec<Value>, Vec<Vec<String>>) =
        items.into_iter().map(|i| (i.json, i.block)).unzip();
    if !caps_actions.is_empty() {
        let mut block = vec![format!("== capabilities.json ({}) ==", scope_name(o.scope))];
        block.extend(render_actions(&caps_actions));
        blocks.push(block);
    }
    let text = blocks
        .iter()
        .map(|b| b.join("\n"))
        .collect::<Vec<_>>()
        .join("\n\n");
    Out {
        json: json!({
            "ok": ok,
            "clients": json_items,
            "capabilities_actions": caps_actions,
        }),
        text,
    }
}

/// The client module of a non-Claude client, for install.
fn install_module(client: Client, c: &Ctx) -> Result<ClientReport, CliError> {
    match client {
        Client::Codex => codex::install(c),
        Client::CopilotCli => copilot_cli::install(c),
        Client::CopilotVsCode => vscode::install(c),
        Client::Agy => agy::install(c),
        Client::OpenCode => opencode::install(c),
        Client::Cursor => cursor::install(c),
        Client::Claude => Err(CliError::Usage("claude has no client module".into())),
    }
}

/// The client module of a non-Claude client, for uninstall.
fn uninstall_module(
    client: Client,
    c: &Ctx,
    caps: &CapabilitiesFile,
) -> Result<ClientReport, CliError> {
    match client {
        Client::Codex => codex::uninstall(c, caps),
        Client::CopilotCli => copilot_cli::uninstall(c, caps),
        Client::CopilotVsCode => vscode::uninstall(c, caps),
        Client::Agy => agy::uninstall(c, caps),
        Client::OpenCode => opencode::uninstall(c, caps),
        Client::Cursor => cursor::uninstall(c, caps),
        Client::Claude => Err(CliError::Usage("claude has no client module".into())),
    }
}

/// The files one client uses in one scope (MCP config, hooks file, instructions file, the ones the
/// scope has), as the strings `HarnessCapabilities::files` holds. Empty for Claude.
fn scope_file_strings(paths: &ClientPaths, client: Client, scope: Scope) -> Vec<String> {
    let Some(t) = paths.targets(client, scope) else {
        return Vec::new();
    };
    [Some(t.mcp), t.hooks, t.instructions]
        .into_iter()
        .flatten()
        .map(|p| p.display().to_string())
        .collect()
}

/// `s` unchanged when it is non-empty and every byte is an ASCII letter or digit or one of
/// `/._-+:@%=,`; otherwise `claude::shell_quote(s)`. Plain paths stay readable in the hook commands.
fn sh_arg(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%=,".contains(&b));
    if plain {
        s.to_owned()
    } else {
        claude::shell_quote(s)
    }
}

/// The client names of `done`, comma separated.
fn names(done: &[Done]) -> String {
    done.iter()
        .map(|d| d.client.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
