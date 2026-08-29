"use strict";

const state = {
  agentId: "",
  mode: "memory",
  snapshot: null,
  token: "",
};

const byId = (id) => document.getElementById(id);

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function empty(message) {
  return element("div", "empty-state", message);
}

function relativeTime(timestamp) {
  const seconds = Math.max(0, Math.floor((Date.now() - timestamp) / 1000));
  if (seconds < 5) return "now";
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

async function api(path, options = {}) {
  if (!state.token) throw new Error("Open the tokenized dashboard URL printed by PidMesh.");
  const headers = new Headers(options.headers || {});
  headers.set("Authorization", `Bearer ${state.token}`);
  if (options.body) headers.set("Content-Type", "application/json");
  const response = await fetch(path, { ...options, headers });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `Request failed (${response.status})`);
  return payload;
}

function loadToken() {
  const fragment = new URLSearchParams(window.location.hash.slice(1));
  const token = fragment.get("token");
  if (token) {
    sessionStorage.setItem("pidmesh-dashboard-token", token);
    history.replaceState(null, "", `${location.pathname}${location.search}`);
  }
  state.token = token || sessionStorage.getItem("pidmesh-dashboard-token") || "";
}

function renderMetrics(snapshot) {
  byId("metric-running").textContent = snapshot.stats.running_agents;
  byId("metric-claims").textContent = snapshot.stats.claims;
  byId("metric-memories").textContent = snapshot.stats.memories;
  byId("metric-messages").textContent = snapshot.stats.messages;
}

function renderAgents(agents) {
  const list = byId("agent-list");
  const visible = agents.slice(0, 12);
  if (!visible.length) {
    list.replaceChildren(empty("No agents have joined this workspace yet."));
    return;
  }
  list.replaceChildren(
    ...visible.map((agent) => {
      const card = element("section", `agent-card ${agent.status}`);
      card.append(element("h3", "", agent.name), element("span", "provider", agent.provider));
      const meta = element("div", "agent-meta");
      const pid = element("div");
      pid.append(element("span", "", "Process"), element("strong", "", `PID ${agent.pid}`));
      const pulse = element("div");
      pulse.append(
        element("span", "", "Heartbeat"),
        element("strong", "", agent.status === "running" ? `${Math.floor(agent.heartbeat_age_ms / 1000)}s ago` : agent.status),
      );
      meta.append(pid, pulse);
      card.append(meta);
      return card;
    }),
  );
}

function renderClaims(claims) {
  const list = byId("claim-list");
  if (!claims.length) {
    list.replaceChildren(empty("No tasks are exclusively claimed."));
    return;
  }
  list.replaceChildren(
    ...claims.map((claim) => {
      const row = element("div", "claim-row");
      row.append(element("div", "claim-task", claim.task_key));
      const meta = element("div", "claim-meta");
      meta.append(element("span", "", "Owner"), element("strong", "", claim.agent_name));
      row.append(meta);
      if (claim.agent_id === state.agentId) {
        const button = element("button", "release-button", "Release");
        button.type = "button";
        button.addEventListener("click", () => releaseClaim(claim.task_key, button));
        row.append(button);
      } else {
        row.append(element("span", "eyebrow", relativeTime(claim.updated_at)));
      }
      return row;
    }),
  );
}

const eventCodes = {
  "agent.joined": "ON",
  "agent.stopped": "OFF",
  "memory.created": "MEM",
  "message.sent": "MSG",
  "task.claimed": "GET",
  "task.released": "REL",
};

function renderEvents(events) {
  const feed = byId("event-feed");
  if (!events.length) {
    feed.replaceChildren(empty("The ordered collaboration stream is quiet."));
    return;
  }
  feed.replaceChildren(
    ...events.slice(0, 80).map((event) => {
      const item = element("div", "feed-item");
      item.append(element("span", "event-code", eventCodes[event.event_type] || "EVT"));
      const copy = element("div");
      copy.append(
        element("p", "", event.event_type.replaceAll(".", " / ")),
        element("small", "", [event.agent_name, event.subject].filter(Boolean).join(" · ") || "workspace event"),
      );
      item.append(copy, element("time", "feed-time", relativeTime(event.created_at)));
      return item;
    }),
  );
}

function renderMemories(memories) {
  const list = byId("memory-list");
  if (!memories.length) {
    list.replaceChildren(empty("Shared memory is empty. Record the first decision above."));
    return;
  }
  list.replaceChildren(
    ...memories.slice(0, 60).map((memory) => {
      const card = element("article", "memory-card");
      const topline = element("div", "memory-topline");
      topline.append(
        element("span", "memory-kind", memory.kind),
        element("span", "eyebrow", memory.key || `#${memory.id}`),
      );
      const footer = element("footer");
      footer.append(
        element("span", "", memory.agent_name || "unknown"),
        element("time", "", relativeTime(memory.created_at)),
      );
      card.append(topline, element("p", "", memory.content), footer);
      return card;
    }),
  );
}

function render(snapshot) {
  state.snapshot = snapshot;
  state.agentId = snapshot.dashboard_agent_id || "";
  byId("workspace-name").textContent = snapshot.workspace;
  renderMetrics(snapshot);
  renderAgents(snapshot.agents);
  renderClaims(snapshot.claims);
  renderEvents(snapshot.events);
  renderMemories(snapshot.memories);
}

async function refresh() {
  try {
    const snapshot = await api("/api/v1/snapshot?limit=100");
    render(snapshot);
    byId("connection-label").textContent = "Local mesh";
    document.body.classList.remove("disconnected");
  } catch (error) {
    byId("connection-label").textContent = error.message;
    document.body.classList.add("disconnected");
  }
}

function setMode(mode) {
  state.mode = mode;
  document.querySelectorAll(".tab").forEach((tab) => {
    tab.classList.toggle("active", tab.dataset.tab === mode);
  });
  byId("memory-options").classList.toggle("hidden", mode !== "memory");
  byId("message-options").classList.toggle("hidden", mode !== "message");
  byId("claim-options").classList.toggle("hidden", mode !== "claim");
  const settings = {
    memory: ["Memory", "Record a decision every agent should know…", "Commit to mesh"],
    message: ["Message", "Send a direct instruction or broadcast…", "Route message"],
    claim: ["Task key", "e.g. src/store.rs:dashboard-snapshot", "Acquire lease"],
  }[mode];
  byId("command-label").textContent = settings[0];
  byId("command-text").placeholder = settings[1];
  byId("submit-label").textContent = settings[2];
}

async function submitCommand(event) {
  event.preventDefault();
  const text = byId("command-text").value.trim();
  if (!text) return;
  const button = event.currentTarget.querySelector("button[type=submit]");
  const status = byId("form-status");
  button.disabled = true;
  status.className = "form-status";
  status.textContent = "Writing locally…";
  try {
    if (state.mode === "memory") {
      await api("/api/v1/memories", {
        method: "POST",
        body: JSON.stringify({
          text,
          kind: byId("memory-kind").value.trim() || "note",
          key: byId("memory-key").value.trim() || null,
          importance: 0.7,
        }),
      });
    } else if (state.mode === "message") {
      await api("/api/v1/messages", {
        method: "POST",
        body: JSON.stringify({ text, recipient: byId("message-recipient").value.trim() || "*" }),
      });
    } else {
      await api("/api/v1/claims", {
        method: "POST",
        body: JSON.stringify({ task: text, lease_seconds: Number(byId("claim-lease").value) || 300 }),
      });
    }
    byId("command-text").value = "";
    status.textContent = "Mesh updated.";
    await refresh();
  } catch (error) {
    status.className = "form-status error";
    status.textContent = error.message;
  } finally {
    button.disabled = false;
  }
}

async function releaseClaim(task, button) {
  button.disabled = true;
  try {
    await api(`/api/v1/claims/${encodeURIComponent(task)}`, { method: "DELETE" });
    await refresh();
  } catch (error) {
    byId("form-status").textContent = error.message;
  } finally {
    button.disabled = false;
  }
}

let searchTimer;
async function searchMemory(event) {
  clearTimeout(searchTimer);
  const query = event.currentTarget.value.trim();
  searchTimer = setTimeout(async () => {
    if (!query) {
      renderMemories(state.snapshot?.memories || []);
      return;
    }
    try {
      renderMemories(await api(`/api/v1/memories?q=${encodeURIComponent(query)}&limit=60`));
    } catch (error) {
      byId("memory-list").replaceChildren(empty(error.message));
    }
  }, 180);
}

loadToken();
document.querySelectorAll(".tab").forEach((tab) => tab.addEventListener("click", () => setMode(tab.dataset.tab)));
byId("command-form").addEventListener("submit", submitCommand);
byId("refresh-button").addEventListener("click", refresh);
byId("memory-search").addEventListener("input", searchMemory);
setMode("memory");
refresh();
setInterval(refresh, 1500);
