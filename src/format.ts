import type { AskRow, ChatRow, InstanceRow, MessageRow, Unread } from "./db.ts";
import { displayBoth, displayName } from "./nickname.ts";

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
  return `[${m.id}] ${author} (${fmtAgo(m.created_at)}): ${m.body}`;
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
        `- ${fmtChat(c.chat)}  unread=${c.unreadCount}  last_read_id=${c.lastReadId}`,
      );
      if (c.latest) lines.push(`    latest: ${fmtMessage(c.latest, viewer)}`);
    }
  }
  return lines.join("\n");
}
