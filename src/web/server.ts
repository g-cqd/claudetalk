/**
 * Read-only live dashboard for ClaudeTalk. `Bun.serve()` binds to 127.0.0.1
 * by default; SSE (text/event-stream) pushes a fresh snapshot to every
 * connected browser tab every `pollMs` ticks.
 */
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { snapshot } from "./snapshot.ts";

type BunServer = ReturnType<typeof Bun.serve>;

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

function sseStream(pollMs: number, signal: AbortSignal): ReadableStream<Uint8Array> {
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
      send("snapshot", snapshot());
      const timer = setInterval(() => {
        if (closed) return;
        try {
          send("snapshot", snapshot());
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

export function serveDashboard(opts: ServeOptions = {}): DashboardServer {
  const port = opts.port ?? 4242;
  const hostname = opts.hostname ?? "127.0.0.1";
  const pollMs = opts.pollMs ?? 500;

  const server = Bun.serve({
    port,
    hostname,
    async fetch(req) {
      const url = new URL(req.url);
      const path = url.pathname;

      if (path === "/" || path === "/index.html") return staticFile("index.html");
      if (path === "/style.css") return staticFile("style.css");
      if (path === "/client.js") return staticFile("client.js");

      if (path === "/api/snapshot") return jsonResponse(snapshot());

      if (path === "/api/stream") {
        return new Response(sseStream(pollMs, req.signal), {
          headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache, no-transform",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
          },
        });
      }

      if (path === "/healthz") return jsonResponse({ ok: true });

      return new Response("Not Found", { status: 404 });
    },
    error(err) {
      console.error("[claudetalk.web]", err);
      return new Response("Internal error", { status: 500 });
    },
  });

  const url = `http://${hostname}:${server.port}/`;
  return {
    server,
    url,
    stop: async () => {
      await server.stop(true);
    },
  };
}

if (import.meta.main) {
  const port = Number(process.env.CLAUDETALK_WEB_PORT ?? 4242);
  const d = serveDashboard({ port });
  // eslint-disable-next-line no-console
  console.log(`ClaudeTalk dashboard: ${d.url}`);
  const stop = async () => {
    await d.stop();
    process.exit(0);
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
}

