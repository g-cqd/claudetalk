// Live ClaudeTalk dashboard. Receives a `snapshot` SSE event every 500 ms and
// re-renders. Selected chat is preserved across renders; auto-scroll messages
// to the bottom only when the user is already near the bottom.
//
// Phase 3 extensions:
//   - viewer selector → ?viewer=X plumbed through SSE + /api/messages + /api/calls
//   - full chat view paginates older messages via /api/messages?since_id=0
//   - tool log has live filters (pseudonym/tool/kind/error_only)
//   - per-pseudonym stable color via HSL(hash(pseudonym) % 360, 70%, 60%)

const $ = (id) => document.getElementById(id);
const fmt = (n) => new Intl.NumberFormat().format(n);

const state = {
  snapshot: null,
  selection: { kind: "none", id: null },
  lastSnapshotAt: 0,
  viewer: "",
  eventSource: null,
  // Phase 3.1: messages currently shown in chat view, plus the oldest id we've
  // loaded so we can paginate older history on demand.
  chatBuffer: { chatId: null, messages: [], hasMore: true },
  // Phase 3.3: tool log filters.
  callFilters: { pseudonym: "", tool: "", kind: "", error_only: false },
};

function setStatus(live) {
  const dot = $("status-dot");
  dot.className = "dot " + (live ? "dot-live" : "dot-disconnected");
  $("status-text").textContent = live ? "live" : "disconnected — retrying";
}

function relTime(ts) {
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

function escapeHtml(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

// Phase 3.4: stable color per pseudonym. Simple djb2 hash → HSL hue.
function colorFor(pseudonym) {
  let h = 5381;
  for (let i = 0; i < pseudonym.length; i++) h = ((h << 5) + h + pseudonym.charCodeAt(i)) >>> 0;
  const hue = h % 360;
  return `hsl(${hue} 70% 60%)`;
}

function authorTag(pseudonym, displayName) {
  const label = displayName && displayName !== pseudonym ? displayName : pseudonym;
  const title = displayName && displayName !== pseudonym ? ` (${pseudonym})` : "";
  return `<span class="author-tag" style="color:${colorFor(pseudonym)}" title="${escapeHtml(pseudonym)}">${escapeHtml(label)}</span>${title ? `<span class="muted">${escapeHtml(title)}</span>` : ""}`;
}

function renderInstances() {
  const ul = $("instances");
  const items = state.snapshot?.instances ?? [];
  $("count-instances").textContent = fmt(items.length);
  ul.innerHTML = "";
  if (items.length === 0) {
    ul.innerHTML = `<li class="muted" style="cursor:default;">no active instances</li>`;
    return;
  }
  for (const i of items) {
    const li = document.createElement("li");
    li.style.cursor = "default";
    li.innerHTML = `
      <div class="li-row">
        ${authorTag(i.pseudonym, i.display_name)}
        <span class="meta">${relTime(i.last_seen)}</span>
      </div>
      <div class="meta">${escapeHtml(i.path)}</div>`;
    ul.appendChild(li);
  }
}

function renderChats() {
  const ul = $("chats");
  const chats = state.snapshot?.chats ?? [];
  $("count-chats").textContent = fmt(chats.length);
  ul.innerHTML = "";
  if (chats.length === 0) {
    ul.innerHTML = `<li class="muted" style="cursor:default;">no chats yet</li>`;
    return;
  }
  for (const c of chats) {
    const li = document.createElement("li");
    const isSelected = state.selection.kind === "chat" && state.selection.id === c.chat.id;
    if (isSelected) li.classList.add("active");
    const totalUnread = Object.values(c.unread_per_member ?? {}).reduce(
      (a, b) => a + (b || 0),
      0,
    );
    const titleHtml = c.chat.title
      ? `${escapeHtml(c.chat.title)} <span class="meta">(${c.chat.kind})</span>`
      : `<span class="meta">${escapeHtml(c.chat.kind)}</span> ${escapeHtml(c.chat.id)}`;
    li.innerHTML = `
      <div class="li-row">
        <span class="name">${titleHtml}</span>
        ${totalUnread > 0 ? `<span class="badge">${totalUnread}</span>` : ""}
      </div>
      <div class="meta">${c.members.map((m) => escapeHtml(m.display_name)).join(" · ")}</div>`;
    li.addEventListener("click", () => {
      // Phase 3.1: when a new chat is selected, reset the buffer.
      state.selection = { kind: "chat", id: c.chat.id };
      state.chatBuffer = { chatId: c.chat.id, messages: c.recent_messages.slice(), hasMore: c.recent_messages.length >= 50 };
      render();
    });
    ul.appendChild(li);
  }
}

function renderAsksSummary() {
  const ul = $("asks-summary");
  const asks = state.snapshot?.asks ?? [];
  const pending = asks.filter((a) => a.answered_at === null).length;
  const answered = asks.length - pending;
  $("count-asks").textContent = fmt(asks.length);
  ul.innerHTML = "";
  const li = document.createElement("li");
  li.classList.add("action");
  if (state.selection.kind === "asks") li.classList.add("active");
  li.innerHTML = `
    <div class="li-row">
      <span class="name">View all asks</span>
      <span class="meta">${pending} pending · ${answered} answered</span>
    </div>`;
  li.addEventListener("click", () => {
    state.selection = { kind: "asks" };
    render();
  });
  ul.appendChild(li);
}

function renderCallsSummary() {
  const ul = $("calls-summary");
  const calls = state.snapshot?.recent_calls ?? [];
  const errors = calls.filter((c) => c.is_error).length;
  $("count-calls").textContent = fmt(calls.length);
  ul.innerHTML = "";
  const li = document.createElement("li");
  li.classList.add("action");
  if (state.selection.kind === "calls") li.classList.add("active");
  li.innerHTML = `
    <div class="li-row">
      <span class="name">View tool log</span>
      <span class="meta">${calls.length} recent${errors ? ` · ${errors} err` : ""}</span>
    </div>`;
  li.addEventListener("click", () => {
    state.selection = { kind: "calls" };
    render();
  });
  ul.appendChild(li);
}

function renderViewerSelect() {
  const sel = $("viewer-select");
  const instances = state.snapshot?.instances ?? [];
  // Preserve current selection across re-renders.
  const current = sel.value;
  sel.innerHTML = '<option value="">— operator (no nicknames) —</option>';
  for (const i of instances) {
    const opt = document.createElement("option");
    opt.value = i.pseudonym;
    opt.textContent = i.display_name === i.pseudonym ? i.pseudonym : `${i.display_name} (${i.pseudonym})`;
    sel.appendChild(opt);
  }
  if (current && instances.some((i) => i.pseudonym === current)) sel.value = current;
}

function renderChatView() {
  const ol = $("messages");
  const buf = state.chatBuffer;
  if (!buf.chatId) return;
  const chat = state.snapshot?.chats.find((c) => c.chat.id === buf.chatId);
  if (chat) {
    $("chat-title").textContent = chat.chat.title ?? chat.chat.id;
    $("chat-meta").textContent = `${chat.chat.kind} · ${chat.members.length} member(s) · ${buf.messages.length} loaded`;
  }
  $("load-older").classList.toggle("hidden", !buf.hasMore || buf.messages.length === 0);
  const nearBottom = ol.scrollTop + ol.clientHeight >= ol.scrollHeight - 32;
  ol.innerHTML = buf.messages
    .map((m) => {
      const replyMark = m.parent_id ? `<span class="msg-id">↪ #${m.parent_id}</span>` : "";
      return `
        <li>
          <div class="msg-head">
            ${authorTag(m.from_pseudonym, m.display_from_name)}
            <span class="msg-id">[#${m.id}]</span>
            ${replyMark}
            <span class="msg-ts">${relTime(m.created_at)}</span>
          </div>
          <div class="msg-body">${escapeHtml(m.body)}</div>
        </li>`;
    })
    .join("");
  if (nearBottom) ol.scrollTop = ol.scrollHeight;
}

async function loadOlderMessages() {
  const buf = state.chatBuffer;
  if (!buf.chatId || buf.messages.length === 0) return;
  const oldestId = buf.messages[0].id;
  // We want messages strictly OLDER than oldestId. The API only goes the
  // other direction (since_id ascending), so fetch with since_id=0 limit=large
  // and slice client-side. With recent_messages capped at 50 and typical chat
  // sizes under 200, this stays small.
  const params = new URLSearchParams({
    chat_id: buf.chatId,
    since_id: "0",
    limit: "500",
  });
  if (state.viewer) params.set("viewer", state.viewer);
  try {
    const res = await fetch(`/api/messages?${params}`);
    if (!res.ok) return;
    const body = await res.json();
    const allUpToOldest = body.messages.filter((m) => m.id < oldestId);
    buf.messages = allUpToOldest.concat(buf.messages);
    buf.hasMore = allUpToOldest.length === 500 - 1; // heuristic
    renderChatView();
  } catch (e) {
    console.error(e);
  }
}

function renderAskView() {
  const asks = state.snapshot?.asks ?? [];
  const ol = $("ask-list");
  ol.innerHTML = asks
    .map((a) => {
      const status = a.answered_at === null ? "pending" : "answered";
      return `
        <li>
          <div class="ask-head">
            <span class="ask-status ${status}">${status.toUpperCase()}</span>
            <span class="ask-flow">${authorTag(a.from_pseudonym, a.display_from_name)} → ${authorTag(a.to_pseudonym, a.display_to_name)}</span>
            <span class="muted">ask_id=${a.id} · ${relTime(a.created_at)}</span>
          </div>
          <div class="ask-q">${escapeHtml(a.body)}</div>
          ${a.answer_body ? `<div class="ask-a">${escapeHtml(a.answer_body)}</div>` : ""}
        </li>`;
    })
    .join("");
}

function renderCallsView() {
  const calls = (state.snapshot?.recent_calls ?? []).slice().reverse();
  const f = state.callFilters;
  const filtered = calls.filter((c) => {
    if (f.pseudonym && !c.pseudonym.toLowerCase().includes(f.pseudonym.toLowerCase())) return false;
    if (f.tool && !c.tool.toLowerCase().includes(f.tool.toLowerCase())) return false;
    if (f.kind && c.kind !== f.kind) return false;
    if (f.error_only && !c.is_error) return false;
    return true;
  });
  const ol = $("calls-list");
  ol.innerHTML = filtered
    .map((c) => {
      const cls = c.is_error ? "err" : "ok";
      const body = c.error
        ? `error: ${escapeHtml(c.error)}`
        : c.result_summary
          ? escapeHtml(c.result_summary)
          : "(empty)";
      const args = c.args_json ? `<div class="muted">args: ${escapeHtml(c.args_json)}</div>` : "";
      return `
        <li class="${cls}">
          <div class="call-head">
            <span class="call-tool">${escapeHtml(c.tool)}</span>
            <span class="call-by">by ${authorTag(c.pseudonym, c.display_pseudonym_name)}</span>
            <span class="muted">${relTime(c.started_at)} · ${c.duration_ms}ms${c.is_error ? " · ERROR" : ""}</span>
          </div>
          ${args}
          <div class="call-body">${body}</div>
        </li>`;
    })
    .join("");
}

function renderSelection() {
  const placeholder = $("placeholder");
  const chatView = $("chat-view");
  const askView = $("ask-view");
  const callsView = $("calls-view");
  placeholder.classList.add("hidden");
  chatView.classList.add("hidden");
  askView.classList.add("hidden");
  callsView.classList.add("hidden");
  if (state.selection.kind === "none") {
    placeholder.classList.remove("hidden");
    return;
  }
  if (state.selection.kind === "chat") {
    chatView.classList.remove("hidden");
    const chat = state.snapshot?.chats.find((c) => c.chat.id === state.selection.id);
    if (chat) {
      // Update buffer with the newest messages from the snapshot.
      const buf = state.chatBuffer;
      if (buf.chatId !== state.selection.id) {
        buf.chatId = state.selection.id;
        buf.messages = chat.recent_messages.slice();
        buf.hasMore = chat.recent_messages.length >= 50;
      } else {
        // Merge new messages from snapshot that arrived after our newest id.
        const newestId = buf.messages.length > 0 ? buf.messages[buf.messages.length - 1].id : 0;
        const fresh = chat.recent_messages.filter((m) => m.id > newestId);
        if (fresh.length > 0) buf.messages.push(...fresh);
      }
    }
    renderChatView();
    return;
  }
  if (state.selection.kind === "asks") {
    askView.classList.remove("hidden");
    renderAskView();
    return;
  }
  if (state.selection.kind === "calls") {
    callsView.classList.remove("hidden");
    renderCallsView();
  }
}

function render() {
  renderViewerSelect();
  renderInstances();
  renderChats();
  renderAsksSummary();
  renderCallsSummary();
  renderSelection();
  if (state.lastSnapshotAt > 0) {
    $("status-when").textContent = `updated ${relTime(state.lastSnapshotAt)}`;
  }
}

function connect() {
  if (state.eventSource) state.eventSource.close();
  const params = new URLSearchParams();
  if (state.viewer) params.set("viewer", state.viewer);
  const url = `/api/stream${params.toString() ? `?${params}` : ""}`;
  const es = new EventSource(url);
  es.addEventListener("open", () => setStatus(true));
  es.addEventListener("error", () => setStatus(false));
  es.addEventListener("snapshot", (ev) => {
    try {
      state.snapshot = JSON.parse(ev.data);
      state.lastSnapshotAt = Date.now();
      setStatus(true);
      render();
    } catch (e) {
      console.error("bad snapshot", e);
    }
  });
  state.eventSource = es;
}

// Wire up controls.
$("viewer-select").addEventListener("change", (e) => {
  state.viewer = e.target.value;
  // Reconnect SSE with the new viewer parameter.
  connect();
});
$("load-older").addEventListener("click", loadOlderMessages);
for (const [id, key] of [
  ["filter-pseudonym", "pseudonym"],
  ["filter-tool", "tool"],
  ["filter-kind", "kind"],
]) {
  $(id).addEventListener("input", (e) => {
    state.callFilters[key] = e.target.value;
    renderCallsView();
  });
}
$("filter-error").addEventListener("change", (e) => {
  state.callFilters.error_only = e.target.checked;
  renderCallsView();
});

connect();
setInterval(render, 1000);
