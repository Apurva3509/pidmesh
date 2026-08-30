"use strict";

const state = {
  agentId: "",
  failures: 0,
  memoryQuery: "",
  mode: "message",
  refreshTimer: null,
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

function initials(value) {
  return value
    .split(/[-_\s]+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase())
    .join("") || "AI";
}

function relativeTime(timestamp) {
  if (!timestamp) return "unknown";
  const seconds = Math.max(0, Math.floor((Date.now() - timestamp) / 1000));
  if (seconds < 5) return "now";
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function durationRemaining(timestamp) {
  const seconds = Math.max(0, Math.ceil((timestamp - Date.now()) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.ceil(seconds / 60)}m`;
  return `${Math.ceil(seconds / 3600)}h`;
}

function basename(path) {
  return String(path || "local").split(/[\\/]/).filter(Boolean).at(-1) || "local";
}

async function api(path, options = {}) {
  if (!state.token) throw new Error("Run pidmesh dashboard and open the tokenized local URL.");
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
  const resources = snapshot.resources || [];
  const workers = snapshot.agents.filter((agent) => agent.provider !== "pidmesh-ui");
  const runningWorkers = workers.filter((agent) => agent.status === "running" && agent.pid_alive).length;
  byId("metric-running").textContent = runningWorkers;
  byId("metric-total").textContent = `${workers.length} registered`;
  byId("metric-claims").textContent = snapshot.stats.claims;
  byId("metric-resources").textContent = snapshot.stats.resources ?? resources.length;
  byId("metric-memories").textContent = snapshot.stats.memories;
  byId("metric-messages").textContent = `${snapshot.stats.messages} messages routed`;
  byId("nav-agent-count").textContent = runningWorkers;
  byId("nav-work-count").textContent = snapshot.stats.claims + resources.length;
  byId("message-count").textContent = `${snapshot.stats.messages} routed`;
}

function healthClass(agent) {
  if (agent.status !== "running" || !agent.pid_alive) return agent.status === "stopped" ? "stopped" : "dead";
  return agent.heartbeat_age_ms > 15000 ? "stale" : "running";
}

function resourceOwnerCounts(resources) {
  return resources.reduce((counts, resource) => {
    counts[resource.agent_id] = (counts[resource.agent_id] || 0) + 1;
    return counts;
  }, {});
}

function renderAgentOptions(agents) {
  const select = byId("message-recipient");
  const selected = select.value || "*";
  const options = [new Option("Everyone", "*")];
  agents
    .filter((agent) => agent.status === "running" && agent.id !== state.agentId)
    .forEach((agent) => options.push(new Option(`${agent.name} · PID ${agent.pid}`, agent.id)));
  select.replaceChildren(...options);
  select.value = [...select.options].some((option) => option.value === selected) ? selected : "*";
}

function renderAgents(agents, resources) {
  const list = byId("agent-list");
  const counts = resourceOwnerCounts(resources);
  if (!agents.length) {
    list.replaceChildren(empty("No agents joined. Run pidmesh swarm --workers 5 or connect the MCP server."));
    return;
  }
  const cards = agents.map((agent) => {
    const health = healthClass(agent);
    const card = element("article", `agent-card ${health}`);
    const title = element("div", "agent-title");
    const copy = element("div");
    copy.append(element("h4", "", agent.name), element("span", "", `${agent.provider} · ${health}`));
    title.append(element("span", "agent-avatar", initials(agent.name)), copy);

    const meta = element("div", "agent-meta");
    const values = [
      ["Process", `PID ${agent.pid}`],
      ["Heartbeat", health === "running" ? relativeTime(agent.heartbeat_at) : health],
      ["Resources", String(counts[agent.id] || 0)],
    ];
    values.forEach(([label, value]) => {
      const item = element("div");
      item.append(element("span", "", label), element("strong", "", value));
      meta.append(item);
    });

    const location = element("div", "agent-location");
    location.append(
      element("span", "", "Branch"),
      element("strong", "", agent.git_branch || agent.branch || "not reported"),
      element("span", "", "Checkout"),
      element("strong", "", agent.checkout_path || agent.worktree_root || agent.working_root || agent.checkout || "current workspace"),
    );
    card.append(title, meta, location);
    card.addEventListener("click", () => {
      setMode("message");
      byId("message-recipient").value = agent.id;
      byId("command-text").focus();
    });
    return card;
  });
  list.replaceChildren(...cards);
}

function renderAttention(snapshot) {
  const issues = [];
  snapshot.agents.forEach((agent) => {
    const health = healthClass(agent);
    if (health === "dead") issues.push({ severity: "critical", icon: "!", title: `${agent.name} PID is not alive`, detail: `PID ${agent.pid} · ownership can be recovered` });
    else if (health === "stale") issues.push({ severity: "", icon: "~", title: `${agent.name} heartbeat is delayed`, detail: `${Math.floor(agent.heartbeat_age_ms / 1000)} seconds since last pulse` });
  });
  (snapshot.resources || []).forEach((resource) => {
    const remaining = resource.lease_expires_at - Date.now();
    if (remaining > 0 && remaining < 60000) issues.push({ severity: "", icon: "⌁", title: `${resourceDisplay(resource)} expires soon`, detail: `${resource.agent_name || "agent"} · ${durationRemaining(resource.lease_expires_at)} remaining` });
  });
  snapshot.claims.forEach((claim) => {
    const remaining = claim.lease_expires_at - Date.now();
    if (remaining > 0 && remaining < 60000) issues.push({ severity: "", icon: "◇", title: `${claim.task_key} claim expires soon`, detail: `${claim.agent_name} · ${durationRemaining(claim.lease_expires_at)} remaining` });
  });

  const list = byId("attention-list");
  byId("attention-count").textContent = issues.length;
  if (!issues.length) {
    const item = element("div", "attention-item ok");
    const copy = element("div");
    copy.append(element("strong", "", "Fleet is coordinated"), element("small", "", "No dead processes or expiring leases need intervention"));
    item.append(element("span", "", "✓"), copy);
    list.replaceChildren(item);
    return;
  }
  list.replaceChildren(
    ...issues.slice(0, 6).map((issue) => {
      const item = element("div", `attention-item ${issue.severity}`);
      const copy = element("div");
      copy.append(element("strong", "", issue.title), element("small", "", issue.detail));
      item.append(element("span", "", issue.icon), copy);
      return item;
    }),
  );
}

function renderClaims(claims) {
  const list = byId("claim-list");
  if (!claims.length) {
    list.replaceChildren(empty("No task is exclusively claimed. Use Claim before assigning overlapping work."));
    return;
  }
  list.replaceChildren(
    ...claims.map((claim) => {
      const row = element("div", "claim-row");
      const main = element("div", "claim-main");
      const meta = element("div", "claim-meta");
      meta.append(element("span", "", "Owner"), element("strong", "", claim.agent_name), element("span", "", claim.detail || "No detail"));
      main.append(element("div", "claim-task", claim.task_key), meta);
      row.append(main);
      if (claim.agent_id === state.agentId) {
        const button = element("button", "release-button", "Release");
        button.type = "button";
        button.addEventListener("click", () => releaseClaim(claim.task_key, button));
        row.append(button);
      } else {
        row.append(element("span", "lease-time", durationRemaining(claim.lease_expires_at)));
      }
      return row;
    }),
  );
}

function resourceDisplay(resource) {
  if (resource.resource) return resource.resource;
  if (resource.resource_kind && resource.resource_key) return `${resource.resource_kind}:${resource.resource_key}`;
  return resource.resource_key || "resource";
}

function renderResources(resources) {
  const list = byId("resource-list");
  if (!resources.length) {
    list.replaceChildren(empty("No resources reserved. Reserve paths, ports, or services before concurrent work."));
    return;
  }
  const ordered = [...resources].sort((left, right) => left.lease_expires_at - right.lease_expires_at);
  list.replaceChildren(
    ...ordered.map((resource) => {
      const remaining = Math.max(0, resource.lease_expires_at - Date.now());
      const row = element("div", `resource-row ${remaining < 60000 ? "expiring" : ""}`);
      const main = element("div", "resource-main");
      const key = element("div", "resource-key");
      key.append(element("span", "", resource.resource_kind || resourceDisplay(resource).split(":", 1)[0]), document.createTextNode(resource.resource_key || resourceDisplay(resource).split(":").slice(1).join(":")));
      const meta = element("div", "resource-meta");
      meta.append(element("span", "", "Owner"), element("strong", "", resource.agent_name || resource.agent_id), element("span", "", resource.task_key || "unlinked"));
      main.append(key, meta);
      const lease = element("div", "lease-time");
      lease.style.setProperty("--lease-progress", `${Math.min(100, Math.max(5, remaining / 900000 * 100))}%`);
      lease.append(element("span", "", durationRemaining(resource.lease_expires_at)), element("i"));
      row.append(main, lease);
      return row;
    }),
  );
}

function renderMessages(messages) {
  const list = byId("message-list");
  if (!messages.length) {
    list.replaceChildren(empty("No handoffs routed yet. Send one directly to an agent or broadcast it."));
    return;
  }
  list.replaceChildren(
    ...messages.slice(0, 80).map((message) => {
      const row = element("article", "message-row");
      const copy = element("div", "message-copy");
      const header = element("header");
      header.append(element("strong", "", message.sender_name), element("small", "", `→ ${message.recipient_name || "everyone"}`));
      copy.append(header, element("p", "", message.body));
      row.append(element("span", "message-avatar", initials(message.sender_name)), copy, element("time", "", relativeTime(message.created_at)));
      return row;
    }),
  );
}

const eventCodes = {
  "agent.dead": "DEAD",
  "agent.joined": "ON",
  "agent.stopped": "OFF",
  "memory.created": "MEM",
  "message.sent": "MSG",
  "resource.released": "FREE",
  "resource.reserved": "RES",
  "task.claimed": "GET",
  "task.released": "REL",
};

function renderEvents(events) {
  const feed = byId("event-feed");
  if (!events.length) {
    feed.replaceChildren(empty("The ordered collaboration ledger is quiet."));
    return;
  }
  feed.replaceChildren(
    ...events.slice(0, 100).map((event) => {
      const item = element("article", "feed-item");
      const copy = element("div");
      copy.append(element("p", "", event.event_type.replaceAll(".", " / ")), element("small", "", [event.agent_name, event.subject].filter(Boolean).join(" · ") || "workspace event"));
      item.append(element("span", "event-code", eventCodes[event.event_type] || "EVT"), copy, element("time", "feed-time", relativeTime(event.created_at)));
      item.title = new Date(event.created_at).toLocaleString();
      return item;
    }),
  );
}

function renderMemories(memories) {
  const list = byId("memory-list");
  if (!memories.length) {
    list.replaceChildren(empty(state.memoryQuery ? `No memory matched “${state.memoryQuery}”.` : "Shared memory is empty. Record the first durable decision."));
    return;
  }
  list.replaceChildren(
    ...memories.slice(0, 80).map((memory) => {
      const card = element("article", "memory-card");
      const topline = element("div", "memory-topline");
      topline.append(element("span", "memory-kind", memory.kind), element("span", "eyebrow", memory.key || `#${memory.id}`));
      const footer = element("footer");
      footer.append(element("span", "", memory.agent_name || "unknown"), element("time", "", relativeTime(memory.created_at)));
      card.append(topline, element("p", "", memory.content), footer);
      return card;
    }),
  );
}

function render(snapshot) {
  state.snapshot = snapshot;
  state.agentId = snapshot.dashboard_agent_id || "";
  const resources = snapshot.resources || [];
  const project = basename(snapshot.workspace);
  byId("project-name").textContent = project;
  byId("project-path").textContent = snapshot.workspace;
  byId("project-path").title = snapshot.workspace;
  byId("workspace-crumb").textContent = project;
  renderMetrics(snapshot);
  renderAttention(snapshot);
  renderAgentOptions(snapshot.agents);
  renderAgents(snapshot.agents, resources);
  renderClaims(snapshot.claims);
  renderResources(resources);
  renderMessages(snapshot.messages || []);
  renderEvents(snapshot.events);
  if (!state.memoryQuery) renderMemories(snapshot.memories);
}

function scheduleRefresh(delay) {
  clearTimeout(state.refreshTimer);
  state.refreshTimer = setTimeout(refresh, delay);
}

async function refresh() {
  if (document.hidden) {
    scheduleRefresh(2500);
    return;
  }
  try {
    const snapshot = await api("/api/v1/snapshot?limit=100");
    render(snapshot);
    state.failures = 0;
    byId("connection-label").textContent = "Live · just updated";
    document.body.classList.remove("disconnected");
    scheduleRefresh(1800);
  } catch (error) {
    state.failures += 1;
    byId("connection-label").textContent = error.message;
    document.body.classList.add("disconnected");
    scheduleRefresh(Math.min(15000, 1800 * 2 ** state.failures));
  }
}

function setMode(mode) {
  state.mode = mode;
  document.querySelectorAll(".tab").forEach((tab) => {
    const active = tab.dataset.tab === mode;
    tab.classList.toggle("active", active);
    tab.setAttribute("aria-selected", String(active));
  });
  byId("memory-options").classList.toggle("hidden", mode !== "memory");
  byId("message-options").classList.toggle("hidden", mode !== "message");
  byId("lease-options").classList.toggle("hidden", !["claim", "reserve"].includes(mode));
  byId("reserve-task-label").classList.toggle("hidden", mode !== "reserve");
  const settings = {
    memory: ["Memory", "Record a decision every agent should know…", "Commit memory"],
    message: ["Message", "Send a direction or handoff to an agent…", "Route message"],
    claim: ["Task key", "e.g. dashboard-api", "Acquire task"],
    reserve: ["Resources", "path:src/store.rs, port:4399", "Reserve atomically"],
  }[mode];
  byId("command-label").textContent = settings[0];
  byId("command-text").placeholder = settings[1];
  byId("submit-label").textContent = settings[2];
}

function parseResources(text) {
  return [...new Set(text.split(/[\n,]+/).map((value) => value.trim()).filter(Boolean))];
}

async function submitCommand(event) {
  event.preventDefault();
  const text = byId("command-text").value.trim();
  if (!text) return;
  const button = event.currentTarget.querySelector("button[type=submit]");
  const status = byId("form-status");
  button.disabled = true;
  status.className = "form-status";
  status.textContent = "Writing to the local mesh…";
  try {
    if (state.mode === "memory") {
      await api("/api/v1/memories", { method: "POST", body: JSON.stringify({ text, kind: byId("memory-kind").value.trim() || "note", key: byId("memory-key").value.trim() || null, importance: 0.7 }) });
    } else if (state.mode === "message") {
      await api("/api/v1/messages", { method: "POST", body: JSON.stringify({ text, recipient: byId("message-recipient").value || "*" }) });
    } else if (state.mode === "claim") {
      const result = await api("/api/v1/claims", { method: "POST", body: JSON.stringify({ task: text, lease_seconds: Number(byId("claim-lease").value) || 900 }) });
      if (!result.acquired) throw new Error(`Task is owned by ${result.agent_name || result.agent_id}.`);
    } else {
      const result = await api("/api/v1/resources", { method: "POST", body: JSON.stringify({ resources: parseResources(text), task: byId("reserve-task").value.trim() || null, lease_seconds: Number(byId("claim-lease").value) || 900 }) });
      if (!result.acquired) {
        const conflict = result.conflicts?.[0];
        throw new Error(`Collision: ${resourceDisplay(conflict || {})} is held by ${conflict?.agent_name || "another agent"}.`);
      }
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
    byId("form-status").className = "form-status error";
    byId("form-status").textContent = error.message;
  } finally {
    button.disabled = false;
  }
}

let searchTimer;
function searchMemory(event) {
  clearTimeout(searchTimer);
  state.memoryQuery = event.currentTarget.value.trim();
  searchTimer = setTimeout(async () => {
    if (!state.memoryQuery) {
      renderMemories(state.snapshot?.memories || []);
      return;
    }
    try {
      renderMemories(await api(`/api/v1/memories?q=${encodeURIComponent(state.memoryQuery)}&limit=80`));
    } catch (error) {
      byId("memory-list").replaceChildren(empty(error.message));
    }
  }, 180);
}

function focusComposer() {
  byId("command-panel").scrollIntoView({ behavior: "smooth", block: "center" });
  setTimeout(() => byId("command-text").focus(), 250);
}

loadToken();
document.querySelectorAll(".tab").forEach((tab) => tab.addEventListener("click", () => setMode(tab.dataset.tab)));
document.querySelectorAll(".nav-item").forEach((item) => item.addEventListener("click", () => {
  document.querySelectorAll(".nav-item").forEach((nav) => nav.classList.toggle("active", nav === item));
  document.body.classList.remove("nav-open");
}));
byId("command-form").addEventListener("submit", submitCommand);
byId("refresh-button").addEventListener("click", refresh);
byId("memory-search").addEventListener("input", searchMemory);
byId("mobile-menu").addEventListener("click", () => document.body.classList.toggle("nav-open"));
byId("new-action-button").addEventListener("click", focusComposer);
document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });
document.addEventListener("keydown", (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    focusComposer();
  }
  if ((event.metaKey || event.ctrlKey) && event.key === "Enter" && document.activeElement === byId("command-text")) {
    event.preventDefault();
    byId("command-form").requestSubmit();
  }
});
setMode("message");
refresh();
