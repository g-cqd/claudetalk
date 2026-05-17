/**
 * Nicknames: validation, resolution, SQLite helpers, and the MCP tool
 * registrations live here so the whole feature is in one file.
 *
 * Tables (defined in src/db.ts migrate()):
 *   - personal_nicknames(viewer, target → nickname)
 *   - group_nickname_votes(chat_id, target, voter → nickname, voted_at)
 *
 * Display resolution order:
 *   1. personal nickname (viewer → target)
 *   2. group nickname active in chatId (≥2 voters incl. target agree)
 *   3. target pseudonym (fallback)
 */
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { Identity } from "./pseudonym.ts";
import {
  _now,
  db,
  getChat,
  listChatMembers,
  listChatsFor,
} from "./db.ts";

// ---------------- validation ----------------

const NICKNAME_RE = /^[A-Za-z][A-Za-z0-9_-]{0,29}$/;
/** Pseudonyms look like `<Adjective><Animal>-<3hex>`. We refuse nicknames
 *  matching that pattern so a user can never disguise one peer as another. */
const PSEUDONYM_RE = /^[A-Z][a-z]+[A-Z][a-z]+-[0-9a-f]{3}$/;

export class NicknameError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NicknameError";
  }
}

export function validateNickname(name: string): string {
  if (typeof name !== "string") throw new NicknameError("nickname must be a string");
  const trimmed = name.trim();
  if (trimmed.length === 0) throw new NicknameError("nickname cannot be empty");
  if (!NICKNAME_RE.test(trimmed)) {
    throw new NicknameError(
      "nickname must be 1-30 chars, alphanumeric or '_-', starting with a letter",
    );
  }
  if (PSEUDONYM_RE.test(trimmed)) {
    throw new NicknameError(
      "nickname cannot look like a pseudonym (Adjective+Animal+3hex)",
    );
  }
  return trimmed;
}

// ---------------- store helpers ----------------

interface PersonalNicknameRow {
  viewer: string;
  target: string;
  nickname: string;
  set_at: number;
}

interface GroupNicknameVoteRow {
  chat_id: string;
  target: string;
  voter: string;
  nickname: string;
  voted_at: number;
}

export function setPersonalNickname(viewer: string, target: string, nickname: string): void {
  db().run(
    `INSERT INTO personal_nicknames (viewer, target, nickname, set_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(viewer, target) DO UPDATE SET nickname = excluded.nickname, set_at = excluded.set_at`,
    [viewer, target, nickname, _now()],
  );
}

function clearPersonalNickname(viewer: string, target: string): boolean {
  const res = db().run(
    "DELETE FROM personal_nicknames WHERE viewer = ? AND target = ?",
    [viewer, target],
  );
  return res.changes > 0;
}

function getPersonalNickname(viewer: string, target: string): string | null {
  const row = db()
    .query<{ nickname: string }, [string, string]>(
      "SELECT nickname FROM personal_nicknames WHERE viewer = ? AND target = ?",
    )
    .get(viewer, target);
  return row?.nickname ?? null;
}

function listPersonalNicknamesFor(viewer: string): PersonalNicknameRow[] {
  return db()
    .query<PersonalNicknameRow, [string]>(
      `SELECT viewer, target, nickname, set_at
       FROM personal_nicknames WHERE viewer = ? ORDER BY target ASC`,
    )
    .all(viewer);
}

export function castGroupNicknameVote(
  chatId: string,
  target: string,
  voter: string,
  nickname: string,
): void {
  db().run(
    `INSERT INTO group_nickname_votes (chat_id, target, voter, nickname, voted_at)
     VALUES (?, ?, ?, ?, ?)
     ON CONFLICT(chat_id, target, voter) DO UPDATE
       SET nickname = excluded.nickname, voted_at = excluded.voted_at`,
    [chatId, target, voter, nickname, _now()],
  );
}

/** Active group nickname for (chat, target): the nickname with ≥2 voters AND
 *  the target among them. Ties broken by most-recent vote. Returns null if no
 *  candidate qualifies. */
function activeGroupNickname(chatId: string, target: string): string | null {
  const votes = db()
    .query<GroupNicknameVoteRow, [string, string]>(
      `SELECT chat_id, target, voter, nickname, voted_at
       FROM group_nickname_votes WHERE chat_id = ? AND target = ?
       ORDER BY voted_at DESC`,
    )
    .all(chatId, target);
  if (votes.length === 0) return null;
  const tally = new Map<string, { count: number; targetVoted: boolean; latest: number }>();
  for (const v of votes) {
    const t = tally.get(v.nickname) ?? { count: 0, targetVoted: false, latest: 0 };
    t.count++;
    if (v.voter === target) t.targetVoted = true;
    if (v.voted_at > t.latest) t.latest = v.voted_at;
    tally.set(v.nickname, t);
  }
  let best: { name: string; latest: number } | null = null;
  for (const [name, t] of tally) {
    if (t.count >= 2 && t.targetVoted) {
      if (best === null || t.latest > best.latest) best = { name, latest: t.latest };
    }
  }
  return best?.name ?? null;
}

// ---------------- display resolution ----------------

/** Resolve the name to display for `target` from `viewer`'s perspective in
 *  the given chat context. */
export function displayName(
  viewer: string,
  target: string,
  chatId: string | null = null,
): string {
  if (viewer === target) return target; // never alias yourself in your own view
  const personal = getPersonalNickname(viewer, target);
  if (personal !== null) return personal;
  if (chatId !== null) {
    const group = activeGroupNickname(chatId, target);
    if (group !== null) return group;
  }
  return target;
}

/** Render `nickname (pseudonym)` if they differ, else just the pseudonym. */
export function displayBoth(
  viewer: string,
  target: string,
  chatId: string | null = null,
): string {
  const d = displayName(viewer, target, chatId);
  return d === target ? target : `${d} (${target})`;
}

// ---------------- MCP tool registrations ----------------

function text(s: string) {
  return { content: [{ type: "text" as const, text: s }] };
}
function error(s: string) {
  return { content: [{ type: "text" as const, text: s }], isError: true };
}

export function registerNicknameTools(server: McpServer, me: Identity): void {
  server.registerTool(
    "nickname_set",
    {
      title: "Set or clear a personal nickname for another instance",
      description:
        "Unilaterally label another instance with a memorable name only YOU see " +
        "(other instances are unaffected). Pass an empty 'nickname' or call " +
        "'nickname_clear' to remove. To agree on a shared nickname inside a chat, " +
        "use 'nickname_in_chat'.",
      inputSchema: {
        target: z.string().min(1).describe("Pseudonym to label (e.g. 'AmberCrow-5ad')."),
        nickname: z
          .string()
          .describe(
            "1-30 chars, alphanumeric or '_-', starts with a letter. Pass empty to remove.",
          ),
      },
    },
    async ({ target, nickname }) => {
      if (target === me.pseudonym) return error("You cannot nickname yourself.");
      if (nickname.trim().length === 0) {
        const cleared = clearPersonalNickname(me.pseudonym, target);
        return text(
          cleared
            ? `Cleared your personal nickname for ${target}.`
            : `No personal nickname was set for ${target}.`,
        );
      }
      let valid: string;
      try {
        valid = validateNickname(nickname);
      } catch (e) {
        return e instanceof NicknameError ? error(e.message) : ((): never => { throw e; })();
      }
      setPersonalNickname(me.pseudonym, target, valid);
      return text(
        `Set your personal nickname for ${target} to '${valid}'. ` +
          `You'll now see '${valid}' wherever ${target} appears in your tool results.`,
      );
    },
  );

  server.registerTool(
    "nickname_clear",
    {
      title: "Clear your personal nickname for another instance",
      description: "Remove a personal nickname you previously set with 'nickname_set'.",
      inputSchema: {
        target: z.string().min(1).describe("Pseudonym whose nickname you want to drop."),
      },
    },
    async ({ target }) => {
      const cleared = clearPersonalNickname(me.pseudonym, target);
      return text(
        cleared
          ? `Cleared your personal nickname for ${target}.`
          : `No personal nickname was set for ${target}.`,
      );
    },
  );

  server.registerTool(
    "nickname_in_chat",
    {
      title: "Cast a group-nickname vote inside a specific chat",
      description:
        "Vote for what the chat should call 'target'. Re-voting replaces your previous vote " +
        "for that target in this chat. A group nickname becomes ACTIVE when ≥2 voters " +
        "(including the target themselves) agree on the same nickname. Typical flow: a " +
        "member proposes by casting a vote; the target ratifies by casting the same vote; " +
        "now everyone in the chat sees the nickname.",
      inputSchema: {
        chat_id: z
          .string()
          .min(1)
          .describe("Chat id (e.g. 'group:design' or 'direct:A|B')."),
        target: z.string().min(1).describe("Pseudonym to label."),
        nickname: z.string().describe("Nickname to vote for. 1-30 chars, alphanum or '_-'."),
      },
    },
    async ({ chat_id, target, nickname }) => {
      const chat = getChat(chat_id);
      if (!chat) return error(`Unknown chat_id '${chat_id}'.`);
      const members = listChatMembers(chat_id).map((m) => m.pseudonym);
      if (!members.includes(me.pseudonym)) {
        return error(
          `You (${me.pseudonym}) are not a member of ${chat_id}. Join first via 'chat' / 'groupchat'.`,
        );
      }
      if (!members.includes(target)) {
        return error(
          `Target ${target} is not a member of ${chat_id}; only members can be nicknamed in-chat.`,
        );
      }
      let valid: string;
      try {
        valid = validateNickname(nickname);
      } catch (e) {
        return e instanceof NicknameError ? error(e.message) : ((): never => { throw e; })();
      }
      castGroupNicknameVote(chat_id, target, me.pseudonym, valid);
      const active = activeGroupNickname(chat_id, target);
      const isActive = active === valid;
      const lines = [
        `Vote recorded in ${chat_id}: ${target} → '${valid}' (by ${me.pseudonym}).`,
      ];
      if (isActive) {
        lines.push(
          `ACTIVE: at least 2 chat members (including ${target}) agree. ` +
            `Everyone in this chat now sees ${target} as '${valid}'.`,
        );
      } else if (me.pseudonym === target) {
        lines.push(
          `You're ratifying as the target — once one other member votes the same, it activates.`,
        );
      } else {
        lines.push(
          `Pending: needs ${target} to also vote '${valid}' (or another member to second it after ${target} has voted).`,
        );
      }
      return text(lines.join("\n"));
    },
  );

  server.registerTool(
    "nicknames_list",
    {
      title: "Show every nickname that affects what you see",
      description:
        "Lists your personal nicknames (only YOU see them) and every active group nickname " +
        "across chats you're a member of.",
      inputSchema: {},
    },
    async () => {
      const personal = listPersonalNicknamesFor(me.pseudonym);
      const myChats = listChatsFor(me.pseudonym);
      const groupActive: Array<{ chatId: string; target: string; name: string }> = [];
      for (const { chat } of myChats) {
        for (const m of listChatMembers(chat.id)) {
          const a = activeGroupNickname(chat.id, m.pseudonym);
          if (a !== null) groupActive.push({ chatId: chat.id, target: m.pseudonym, name: a });
        }
      }
      const lines: string[] = [`Nicknames for ${me.pseudonym}:`];
      if (personal.length === 0) {
        lines.push("  Personal: (none)");
      } else {
        lines.push(`  Personal (${personal.length}):`);
        for (const p of personal) {
          lines.push(
            `    ${displayBoth(me.pseudonym, p.target, null)} → '${p.nickname}'  (set ${new Date(p.set_at).toISOString()})`,
          );
        }
      }
      if (groupActive.length === 0) {
        lines.push("  Group: (none active)");
      } else {
        lines.push(`  Group (${groupActive.length}):`);
        for (const g of groupActive) {
          lines.push(`    [${g.chatId}]  ${g.target} → '${g.name}'`);
        }
      }
      return text(lines.join("\n"));
    },
  );
}
