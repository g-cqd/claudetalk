// Live ClaudeTalk dashboard. Receives a `snapshot` SSE event every 500 ms and
// re-renders. Selected chat is preserved across renders; auto-scroll messages
// to the bottom only when the user is already near the bottom.

const $ = (id) => document.getElementById(id);
const fmt = (n) => new Intl.NumberFormat().format(n);

const state = {
  snapshot: null,
  selection: { kind: "none", id: null }, // {kind:'chat',id} | {kind:'asks'} | {kind:'none'}
  lastSnapshotAt: 0,
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
        <span class="name">${escapeHtml(i.pseudonym)}</span>
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
      : `<span class="meta">${escapeHtml(c.chat.kind)}</span> ${escapeHtml(
          c.chat.id,
        )}`;
    li.innerHTML = `
      <div class="li-row">
        <span class="name">${titleHtml}</span>
        ${totalUnread > 0 ? `<span class="badge">${totalUnread}</span>` : ""}
      </div>
      <div class="meta">${c.members.map(escapeHtml).join(" · ")}</div>`;
    li.addEventListener("click", () => {
      state.selection = { kind: "chat", id: c.chat.id };
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
    const chat = state.snapshot?.chats.find((c) => c.chat.id === state.selection.id);
    if (!chat) {
      placeholder.classList.remove("hidden");
      return;
    }
    chatView.classList.remove("hidden");
    $("chat-title").textContent = chat.chat.title ?? chat.chat.id;
    $("chat-meta").textContent = `${chat.chat.kind} · ${chat.members.length} member(s) · ${chat.recent_messages.length} recent`;
    const ol = $("messages");
    const nearBottom = ol.scrollTop + ol.clientHeight >= ol.scrollHeight - 32;
    ol.innerHTML = chat.recent_messages
      .map(
        (m) => `
        <li>
          <div class="msg-head">
            <span class="msg-author">${escapeHtml(m.from_pseudonym)}</span>
            <span class="msg-id">[#${m.id}]</span>
            <span class="msg-ts">${relTime(m.created_at)}</span>
          </div>
          <div class="msg-body">${escapeHtml(m.body)}</div>
        </li>`,
      )
      .join("");
    if (nearBottom) ol.scrollTop = ol.scrollHeight;
    return;
  }
  if (state.selection.kind === "asks") {
    askView.classList.remove("hidden");
    const asks = state.snapshot?.asks ?? [];
    const ol = $("ask-list");
    ol.innerHTML = asks
      .map((a) => {
        const status = a.answered_at === null ? "pending" : "answered";
        return `
          <li>
            <div class="ask-head">
              <span class="ask-status ${status}">${status.toUpperCase()}</span>
              <span class="ask-flow">${escapeHtml(a.from_pseudonym)} → ${escapeHtml(
                a.to_pseudonym,
              )}</span>
              <span class="muted">ask_id=${a.id} · ${relTime(a.created_at)}</span>
            </div>
            <div class="ask-q">${escapeHtml(a.body)}</div>
            ${a.answer_body ? `<div class="ask-a">${escapeHtml(a.answer_body)}</div>` : ""}
          </li>`;
      })
      .join("");
    return;
  }
  if (state.selection.kind === "calls") {
    callsView.classList.remove("hidden");
    const calls = (state.snapshot?.recent_calls ?? []).slice().reverse(); // newest first
    const ol = $("calls-list");
    ol.innerHTML = calls
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
              <span class="call-by">by ${escapeHtml(c.pseudonym)}</span>
              <span class="muted">${relTime(c.started_at)} · ${c.duration_ms}ms${c.is_error ? " · ERROR" : ""}</span>
            </div>
            ${args}
            <div class="call-body">${body}</div>
          </li>`;
      })
      .join("");
  }
}

function render() {
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
  const es = new EventSource("/api/stream");
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
}

connect();
// Periodically re-render so relative timestamps refresh even between snapshots.
setInterval(render, 1000);
