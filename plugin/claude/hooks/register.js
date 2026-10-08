// Agora mod: while the session is idle, polls the Agora inbox every POLL_MS and submits a
// prompt only when `agora hook poll` reports new messages. The Rust side frames the messages
// and dedupes them with a per-session cursor. The mod never acks and never submits asUser.
const AGORA = ["agora"];
const POLL_MS = 5000;

let timer = null;
let busy = false;
let inFlight = false;

/** @type {import('claude-code').Register} */
export function register(on) {
  on('session.start', async ($, e, next) => {
    busy = false;
    try {
      if (timer) {
        timer.cancel();
        timer = null;
      }
      const folder = await $.session.root();
      timer = $.clock.every(POLL_MS, async () => {
        if (busy || inFlight) return;
        inFlight = true;
        try {
          // Resolved per tick: the transcript id changes after /clear, and session.start does not refire.
          const id = String(await $.session.id()).replace(/\.jsonl$/, '');
          const r = await $.process.run([...AGORA, 'hook', 'poll', '--session', id, '--folder', folder], { timeoutMs: 10000 });
          if (r.exitCode !== 0) return;
          let out;
          try {
            out = JSON.parse(r.stdout);
          } catch {
            return;
          }
          if (out && typeof out.count === 'number' && out.count > 0 && typeof out.text === 'string' && out.text.length > 0) {
            // If submit fails after poll advanced the per-session cursor, the messages stay unacked in the
            // inbox (inbox_wait, channel push or a compact/clear reset deliver them).
            $.prompt.submit({ text: out.text }).catch(() => {});
          }
        } catch {
          /* fail open: a failed poll delivers nothing and the next tick retries */
        } finally {
          inFlight = false;
        }
      });
    } catch {
      /* fail open: no root or timer means no polling this session; the other delivery rungs still apply */
    }
    return next(e);
  });

  on('turn.start', async ($, e, next) => {
    // Mirrors turn.complete: a subagent's turn does not make the main session busy. TurnStartInput
    // carries no agentId in the d.ts; the guard is defensive and harmless if it is absent.
    if (!(e && e.agentId)) busy = true;
    return next(e);
  });

  on('turn.complete', async ($, e, next) => {
    // A subagent's loop carries agentId; only the main loop's end means the session is idle.
    if (!e.agentId) busy = false;
    return next(e);
  });

  on('session.end', async ($, e, next) => {
    busy = false;
    // clear and resume end the conversation but not the mod's life: the process goes on under a new
    // session id and session.start does not refire, so the timer keeps polling (the id is resolved per tick).
    const keepsTimer = e && (e.reason === 'clear' || e.reason === 'resume');
    if (!keepsTimer && timer) {
      timer.cancel();
      timer = null;
    }
    return next(e);
  });
}
