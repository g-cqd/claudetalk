import {
  type AskRow,
  type ChatRow,
  type InstanceRow,
  db,
  listAnsweredAsksFrom,
  listChatsFor,
  type MessageRow,
  type Unread,
  unreadSummary,
} from "./db.ts";
import { discoverableGroupsFor } from "./notifications.ts";
import { displayBoth, displayName } from "./nickname.ts";
import { summariseReactions, summariseReactionsBatch } from "./reactions.ts";

export function fmtAgo(ts: number): string {
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}

/** Render the instance line. With `viewer`, prefix the pseudonym with any
 *  personal nickname the viewer has set. */
export function fmtInstance(i: InstanceRow, viewer?: string): string {
  const label = viewer ? displayBoth(viewer, i.pseudonym, null) : i.pseudonym;
  return `- ${label}  (last seen ${fmtAgo(i.last_seen)})  path=${i.path}`;
}

export function fmtAsk(a: AskRow, viewer?: string): string {
  const status = a.answered_at === null ? "PENDING" : "ANSWERED";
  const from = viewer ? displayBoth(viewer, a.from_pseudonym, null) : a.from_pseudonym;
  const to = viewer ? displayBoth(viewer, a.to_pseudonym, null) : a.to_pseudonym;
  return [
    `ask_id=${a.id}  ${status}`,
    `  from: ${from}  ->  to: ${to}`,
    `  asked: ${fmtAgo(a.created_at)}`,
    `  question: ${a.body}`,
    a.answer_body ? `  answer: ${a.answer_body}` : "",
  ]
    .filter(Boolean)
    .join("\n");
}

export function fmtChat(c: ChatRow): string {
  const t = c.title ? ` "${c.title}"` : "";
  return `${c.kind === "group" ? "group" : "direct"} chat${t} id=${c.id}`;
}

export function fmtMessage(m: MessageRow, viewer?: string): string {
  const author = viewer ? displayName(viewer, m.from_pseudonym, m.chat_id) : m.from_pseudonym;
  // Display seq as the human-visible "[N]" id. parent_id is a UUID; resolve
  // to its seq via lookup so threading stays readable. Falls back to UUID
  // if the parent was deleted (shouldn't happen with FK cascade, but defensive).
  let idTag = `[${m.seq}]`;
  if (m.parent_id !== null && m.parent_id !== undefined) {
    const parent = lookupMessageSeq(m.parent_id);
    idTag = `[${m.seq} ↪ ${parent ?? m.parent_id}]`;
  }
  const reactions = summariseReactions(m.id);
  return `${idTag} ${author} (${fmtAgo(m.created_at)}): ${m.body}${reactions}`;
}

function lookupMessageSeq(uuid: string): number | null {
  const row = db().query<{ seq: number }, [string]>(
    "SELECT seq FROM messages WHERE id = ?",
  ).get(uuid);
  return row?.seq ?? null;
}

/** Phase v0.5.2 perf: render a list of messages in two SQL queries total
 *  (parent-seq lookup + reactions) instead of 2N. Used by chat / groupchat
 *  / read tool handlers when rendering the recent slice. */
export function fmtMessageList(messages: MessageRow[], viewer?: string): string[] {
  if (messages.length === 0) return [];
  const parentIds = [...new Set(messages.map((m) => m.parent_id).filter((p): p is string => !!p))];
  const parentSeqByUuid = new Map<string, number>();
  if (parentIds.length > 0) {
    const placeholders = parentIds.map(() => "?").join(",");
    const rows = db().query<{ id: string; seq: number }, string[]>(
      `SELECT id, seq FROM messages WHERE id IN (${placeholders})`,
    ).all(...parentIds);
    for (const r of rows) parentSeqByUuid.set(r.id, r.seq);
  }
  const reactionsByUuid = summariseReactionsBatch(messages.map((m) => m.id));
  return messages.map((m) => {
    const author = viewer ? displayName(viewer, m.from_pseudonym, m.chat_id) : m.from_pseudonym;
    let idTag = `[${m.seq}]`;
    if (m.parent_id !== null && m.parent_id !== undefined) {
      const parentSeq = parentSeqByUuid.get(m.parent_id);
      idTag = `[${m.seq} ↪ ${parentSeq ?? m.parent_id}]`;
    }
    const reactions = reactionsByUuid.get(m.id) ?? "";
    return `${idTag} ${author} (${fmtAgo(m.created_at)}): ${m.body}${reactions}`;
  });
}

function directPeer(chatId: string, me: string): string {
  return chatId.replace(/^direct:/, "").split("|").find((p) => p !== me) ?? "?";
}

/** Render the full inbox: pending asks → your chats (always listed, caught-up
 *  or not) → recent answers → discoverable groups. */
export function renderInbox(me: string, answeredSinceId: number): string {
  const u = unreadSummary(me);
  const answered = listAnsweredAsksFrom(me, answeredSinceId);
  const discoverable = discoverableGroupsFor(me, 24 * 60 * 60_000, 10);
  const myChats = listChatsFor(me);
  const unreadById = new Map(u.unreadChats.map((c) => [c.chat.id, c]));

  const parts: string[] = [`Inbox for ${me}:`];

  parts.push(
    u.pendingAsks.length > 0
      ? `Pending asks for you (${u.pendingAsks.length}):`
      : "No pending asks.",
  );
  for (const a of u.pendingAsks) parts.push(fmtAsk(a, me), "");

  parts.push("");
  if (myChats.length === 0) {
    parts.push("No chats yet. Use 'chat' or 'groupchat' to start one.");
  } else {
    parts.push(`Your chats (${myChats.length}):`);
    for (const { chat } of myChats) {
      const unread = unreadById.get(chat.id);
      const label = chat.kind === "group"
        ? fmtChat(chat)
        : `direct with ${directPeer(chat.id, me)}  (id=${chat.id})`;
      if (unread && unread.unreadCount > 0) {
        parts.push(`  - ${label}  unread=${unread.unreadCount}  last_read_seq=${unread.lastReadSeq}`);
        if (unread.latest) parts.push(`      latest: ${fmtMessage(unread.latest, me)}`);
      } else {
        parts.push(`  - ${label}  (all caught up)`);
      }
    }
  }

  if (answered.length > 0) {
    parts.push("", `Answers to asks you sent (${answered.length}):`);
    for (const a of answered) parts.push(fmtAsk(a, me));
  }

  if (discoverable.length > 0) {
    parts.push("", `Discoverable group chats (${discoverable.length}, you're not a member):`);
    for (const g of discoverable) {
      const title = g.chat.title ? ` "${g.chat.title}"` : "";
      const slug = g.chat.id.replace(/^group:/, "");
      const ago = Math.floor((Date.now() - g.latest_at) / 1000);
      parts.push(
        `  - ${slug}${title}  members=${g.member_count}  last_post: ${g.latest_from} (${ago}s ago)`,
        `      → join via: mcp__claudetalk__groupchat slug=${slug}`,
      );
    }
  }
  return parts.join("\n");
}

export function fmtUnread(u: Unread, viewer?: string): string {
  const lines: string[] = [];
  if (u.pendingAsks.length === 0 && u.unreadChats.length === 0) {
    return "Inbox empty. No pending asks, no unread chat messages.";
  }
  if (u.pendingAsks.length > 0) {
    lines.push(`Pending asks for you (${u.pendingAsks.length}):`);
    for (const a of u.pendingAsks) lines.push(fmtAsk(a, viewer), "");
  }
  if (u.unreadChats.length > 0) {
    lines.push(`Unread chats (${u.unreadChats.length}):`);
    for (const c of u.unreadChats) {
      lines.push(
        `- ${fmtChat(c.chat)}  unread=${c.unreadCount}  last_read_seq=${c.lastReadSeq}`,
      );
      if (c.latest) lines.push(`    latest: ${fmtMessage(c.latest, viewer)}`);
    }
  }
  return lines.join("\n");
}
