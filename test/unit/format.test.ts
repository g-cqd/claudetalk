import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  fmtAgo,
  fmtAsk,
  fmtChat,
  fmtInstance,
  fmtMessage,
  fmtUnread,
} from "../../src/format.ts";
import type {
  AskRow,
  ChatRow,
  InstanceRow,
  MessageRow,
  Unread,
} from "../../src/db.ts";

const NOW = 1_777_000_000_000;
const origDateNow = Date.now;

beforeEach(() => {
  Date.now = () => NOW;
});
afterEach(() => {
  Date.now = origDateNow;
});

describe("fmtAgo", () => {
  test.each([
    [0, "0s ago"],
    [5_000, "5s ago"],
    [60_000, "1m ago"],
    [60 * 60_000, "1h ago"],
    [24 * 60 * 60_000, "1d ago"],
    [3 * 24 * 60 * 60_000, "3d ago"],
  ])("ago(%dms) → %s", (ms, label) => {
    expect(fmtAgo(NOW - ms)).toBe(label);
  });

  test("clamps future timestamps to 0s ago", () => {
    expect(fmtAgo(NOW + 1_000)).toBe("0s ago");
  });
});

const instance: InstanceRow = {
  pseudonym: "SwiftFox-a3f",
  path: "/tmp/swift",
  first_seen: NOW - 60_000,
  last_seen: NOW - 5_000,
  pid: 42,
};

test("fmtInstance prints the pseudonym, age and path", () => {
  const out = fmtInstance(instance);
  expect(out).toContain("SwiftFox-a3f");
  expect(out).toContain("5s ago");
  expect(out).toContain("path=/tmp/swift");
});

test("fmtChat distinguishes direct vs group and includes a title", () => {
  const direct: ChatRow = { id: "direct:A|B", kind: "direct", title: null, created_at: NOW };
  const group: ChatRow = { id: "group:design", kind: "group", title: "Design", created_at: NOW };
  expect(fmtChat(direct)).toContain("direct chat");
  expect(fmtChat(group)).toContain("group chat");
  expect(fmtChat(group)).toContain('"Design"');
});

test("fmtMessage prefixes seq, includes author and body", () => {
  const m: MessageRow = {
    id: "550e8400-e29b-41d4-a716-446655440000",
    seq: 7,
    chat_id: "group:x",
    from_pseudonym: "SwiftFox-a3f",
    body: "hello",
    created_at: NOW - 1_000,
    parent_id: null,
  };
  expect(fmtMessage(m)).toBe("[7] SwiftFox-a3f (1s ago): hello");
});

test("fmtAsk includes status, ids, and (when present) the answer body", () => {
  const pending: AskRow = {
    id: 11,
    from_pseudonym: "A",
    to_pseudonym: "B",
    body: "ping?",
    created_at: NOW,
    answered_at: null,
    answer_body: null,
  };
  expect(fmtAsk(pending)).toContain("PENDING");
  expect(fmtAsk(pending)).toContain("ask_id=11");
  expect(fmtAsk(pending)).not.toContain("answer:");

  const answered: AskRow = { ...pending, id: 12, answered_at: NOW, answer_body: "pong" };
  expect(fmtAsk(answered)).toContain("ANSWERED");
  expect(fmtAsk(answered)).toContain("answer: pong");
});

test("fmtUnread says 'empty' when there's nothing pending", () => {
  expect(fmtUnread({ pendingAsks: [], unreadChats: [] })).toContain("empty");
});

test("fmtUnread summarises pending asks AND unread chats", () => {
  const u: Unread = {
    pendingAsks: [
      {
        id: 1,
        from_pseudonym: "Alice",
        to_pseudonym: "Me",
        body: "Q",
        created_at: NOW,
        answered_at: null,
        answer_body: null,
      },
    ],
    unreadChats: [
      {
        chat: { id: "direct:Me|Alice", kind: "direct", title: null, created_at: NOW },
        unreadCount: 3,
        latest: {
          id: "660e8400-e29b-41d4-a716-446655440099",
          seq: 99,
          chat_id: "direct:Me|Alice",
          from_pseudonym: "Alice",
          body: "yo",
          created_at: NOW - 1000,
          parent_id: null,
        },
        lastReadSeq: 96,
      },
    ],
  };
  const out = fmtUnread(u);
  expect(out).toContain("Pending asks");
  expect(out).toContain("Unread chats");
  expect(out).toContain("unread=3");
  expect(out).toContain("last_read_seq=96");
});
