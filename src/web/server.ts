/**
 * Read-only live dashboard for ClaudeTalk. `Bun.serve()` binds to 127.0.0.1
 * by default; SSE (text/event-stream) pushes a fresh snapshot to every
 * connected browser tab every `pollMs` ticks.
 */
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { checkpointWal, getChat, getDashboardVersion, listMessages } from "../db.ts";
import { listToolCalls } from "../audit-log.ts";
import { displayName } from "../nickname.ts";
import { snapshot } from "./snapshot.ts";

type BunServer = ReturnType<typeof Bun.serve>;

interface WsData {
  viewer: string | null;
}

const HERE = dirname(fileURLToPath(import.meta.url));
const STATIC_DIR = HERE;

export interface ServeOptions {
  port?: number;
  hostname?: string;
  pollMs?: number;
}

export interface DashboardServer {
  server: BunServer;
  url: string;
  stop: () => Promise<void>;
}

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "application/javascript; charset=utf-8",
  ".svg": "image/svg+xml",
};

function mimeFor(path: string): string {
  const ext = path.slice(path.lastIndexOf("."));
  return MIME[ext] ?? "application/octet-stream";
}

async function staticFile(relPath: string): Promise<Response> {
  const file = Bun.file(join(STATIC_DIR, relPath));
  if (!(await file.exists())) return new Response("Not Found", { status: 404 });
  return new Response(file, {
    headers: { "Content-Type": mimeFor(relPath), "Cache-Control": "no-cache" },
  });
}

function jsonResponse(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": "no-cache" },
  });
}

function sseStream(
  pollMs: number,
  signal: AbortSignal,
  viewer: string | null,
): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      let closed = false;
      const send = (event: string, data: unknown) => {
        if (closed) return;
        try {
          controller.enqueue(
            enc.encode(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`),
          );
        } catch {
          closed = true;
        }
      };
      send("hello", { ok: true });
      send("snapshot", snapshot({ viewer }));
      const timer = setInterval(() => {
        if (closed) return;
        try {
          send("snapshot", snapshot({ viewer }));
        } catch {
          closed = true;
          clearInterval(timer);
        }
      }, pollMs);
      const onAbort = () => {
        closed = true;
        clearInterval(timer);
        try {
          controller.close();
        } catch {}
      };
      signal.addEventListener("abort", onAbort, { once: true });
    },
  });
}

/** Optional `viewer` query param. Returned verbatim from URL, no validation;
 *  if an unknown pseudonym is passed, displayName just falls through to it. */
function viewerFrom(url: URL): string | null {
  const v = url.searchParams.get("viewer");
  return v && v.length > 0 ? v : null;
}

export function serveDashboard(opts: ServeOptions = {}): DashboardServer {
  const port = opts.port ?? 4242;
  const hostname = opts.hostname ?? "127.0.0.1";
  const pollMs = opts.pollMs ?? 500;
  // Phase 3.5: shared ticker drains the dashboard_version row and only
  // builds a new snapshot when it bumped. Cheap (1 row by PK).
  const VERSION_POLL_MS = 150;

  // viewer (or null = unbound) -> Set of open WebSocket handles. Set size
  // is the authoritative subscriber count. Previously this was a number
  // counter that could drift if `open` fired without a matching `close`
  // (transport error, ws.send threw before establishment, etc.) —
  // resulting in a ticker that never stopped because count > 0 with no
  // real subscribers. (Perf audit M8.)
  const subscribers = new Map<string | null, Set<unknown>>();
  let lastVersion = -1;
  let versionTimer: ReturnType<typeof setInterval> | null = null;

  const topicFor = (v: string | null) => `snap:${v ?? ""}`;

  function startVersionTickerIfNeeded(srv: BunServer): void {
    if (versionTimer !== null) return;
    lastVersion = getDashboardVersion();
    versionTimer = setInterval(() => {
      if (subscribers.size === 0) return;
      const v = getDashboardVersion();
      if (v === lastVersion) return;
      lastVersion = v;
      for (const viewer of subscribers.keys()) {
        const payload = JSON.stringify({
          type: "snapshot",
          version: v,
          data: snapshot({ viewer }),
        });
        srv.publish(topicFor(viewer), payload);
      }
    }, VERSION_POLL_MS);
    if (typeof versionTimer === "object" && "unref" in versionTimer) {
      (versionTimer as { unref: () => void }).unref();
    }
  }

  function stopVersionTicker(): void {
    if (versionTimer !== null) {
      clearInterval(versionTimer);
      versionTimer = null;
    }
  }

  function addSubscriber(viewer: string | null, ws: unknown): void {
    const set = subscribers.get(viewer) ?? new Set<unknown>();
    set.add(ws);
    subscribers.set(viewer, set);
  }

  function removeSubscriber(viewer: string | null, ws: unknown): void {
    const set = subscribers.get(viewer);
    if (!set) return;
    set.delete(ws);
    if (set.size === 0) subscribers.delete(viewer);
  }

  const server = Bun.serve<WsData>({
    port,
    hostname,
    async fetch(req, srv) {
      const url = new URL(req.url);
      const path = url.pathname;

      if (path === "/ws") {
        const data: WsData = { viewer: viewerFrom(url) };
        if (srv.upgrade(req, { data })) return undefined;
        return new Response("upgrade failed", { status: 426 });
      }

      if (path === "/" || path === "/index.html") return staticFile("index.html");
      if (path === "/style.css") return staticFile("style.css");
      if (path === "/client.js") return staticFile("client.js");

      if (path === "/api/snapshot") {
        return jsonResponse(snapshot({ viewer: viewerFrom(url) }));
      }

      if (path === "/api/stream") {
        return new Response(sseStream(pollMs, req.signal, viewerFrom(url)), {
          headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache, no-transform",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
          },
        });
      }

      // Phase 3.1 — paginated chat messages.
      // GET /api/messages?chat_id=...&since_seq=N&limit=M&viewer=X
      // Returns { chat_id, messages: [{...,display_from_name}], has_more }
      if (path === "/api/messages") {
        const chatId = url.searchParams.get("chat_id");
        if (!chatId) return jsonResponse({ error: "chat_id required" }, 400);
        const chat = getChat(chatId);
        if (!chat) return jsonResponse({ error: "unknown chat" }, 404);
        const sinceSeq = Number(url.searchParams.get("since_seq") ?? 0);
        const limit = Math.max(1, Math.min(Number(url.searchParams.get("limit") ?? 100), 500));
        const viewer = viewerFrom(url);
        const rows = listMessages(chatId, sinceSeq, limit);
        const messages = rows.map((m) => ({
          ...m,
          display_from_name:
            viewer === null ? m.from_pseudonym : displayName(viewer, m.from_pseudonym, chatId),
        }));
        return jsonResponse({
          chat_id: chatId,
          messages,
          has_more: messages.length === limit,
        });
      }

      // Phase 3.3 — filtered tool call log.
      // GET /api/calls?pseudonym=X&tool=Y&kind=Z&since_id=N&limit=M&error_only=1
      if (path === "/api/calls") {
        const limit = Math.max(1, Math.min(Number(url.searchParams.get("limit") ?? 100), 500));
        const sinceId = url.searchParams.has("since_id")
          ? Number(url.searchParams.get("since_id"))
          : undefined;
        let rows = listToolCalls({
          pseudonym: url.searchParams.get("pseudonym") ?? undefined,
          tool: url.searchParams.get("tool") ?? undefined,
          kind: url.searchParams.get("kind") ?? undefined,
          sinceId,
          limit,
        });
        if (url.searchParams.get("error_only") === "1") {
          rows = rows.filter((r) => r.is_error === 1);
        }
        const viewer = viewerFrom(url);
        const calls = rows.map((c) => ({
          ...c,
          display_pseudonym_name:
            viewer === null ? c.pseudonym : displayName(viewer, c.pseudonym, null),
        }));
        return jsonResponse({ calls });
      }

      if (path === "/healthz") return jsonResponse({ ok: true });

      return new Response("Not Found", { status: 404 });
    },
    error(err) {
      console.error("[claudetalk.web]", err);
      return new Response("Internal error", { status: 500 });
    },
    websocket: {
      open(ws) {
        const v = ws.data.viewer;
        const topic = topicFor(v);
        ws.subscribe(topic);
        addSubscriber(v, ws);
        startVersionTickerIfNeeded(server);
        ws.send(
          JSON.stringify({
            type: "snapshot",
            version: getDashboardVersion(),
            data: snapshot({ viewer: v }),
          }),
        );
      },
      message(ws, raw) {
        // Only one supported message: { type: "ping" } → respond { type: "pong" }.
        try {
          const parsed = typeof raw === "string" ? JSON.parse(raw) : null;
          if (parsed?.type === "ping") ws.send(JSON.stringify({ type: "pong" }));
        } catch {
          // ignore malformed
        }
      },
      close(ws) {
        removeSubscriber(ws.data.viewer, ws);
        if (subscribers.size === 0) stopVersionTicker();
      },
    },
  });

  const url = `http://${hostname}:${server.port}/`;
  return {
    server,
    url,
    stop: async () => {
      stopVersionTicker();
      await server.stop(true);
    },
  };
}

if (import.meta.main) {
  const port = Number(process.env.CLAUDETALK_WEB_PORT ?? 4242);
  const d = serveDashboard({ port });
  // eslint-disable-next-line no-console
  console.log(`ClaudeTalk dashboard: ${d.url}`);
  // Phase 5.1: drain the WAL every 5 min so the dashboard process holds
  // the periodic checkpoint duty; MCP servers stay focused on tool latency.
  const checkpoint = setInterval(checkpointWal, 5 * 60 * 1000);
  const stop = async () => {
    clearInterval(checkpoint);
    await d.stop();
    process.exit(0);
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
}

