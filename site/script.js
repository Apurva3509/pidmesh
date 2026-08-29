(() => {
  "use strict";

  const header = document.querySelector("[data-header]");
  const navToggle = document.querySelector(".nav-toggle");
  const navigation = document.querySelector("#site-nav");
  const reducedMotion = window.matchMedia(
    "(prefers-reduced-motion: reduce)",
  ).matches;

  const updateHeader = () =>
    header?.classList.toggle("scrolled", window.scrollY > 24);
  updateHeader();
  window.addEventListener("scroll", updateHeader, { passive: true });

  navToggle?.addEventListener("click", () => {
    const isOpen = navToggle.getAttribute("aria-expanded") === "true";
    navToggle.setAttribute("aria-expanded", String(!isOpen));
    navigation?.classList.toggle("open", !isOpen);
  });

  navigation?.querySelectorAll("a").forEach((link) => {
    link.addEventListener("click", () => {
      navToggle?.setAttribute("aria-expanded", "false");
      navigation.classList.remove("open");
    });
  });

  const alignHashTarget = () => {
    const id = decodeURIComponent(window.location.hash.slice(1));
    if (!id) return;
    window.requestAnimationFrame(() =>
      document
        .getElementById(id)
        ?.scrollIntoView({ block: "start", behavior: "instant" }),
    );
  };
  if (document.readyState === "complete") alignHashTarget();
  else window.addEventListener("load", alignHashTarget, { once: true });

  const clock = document.querySelector("[data-clock]");
  const updateClock = () => {
    if (clock) {
      clock.textContent = new Intl.DateTimeFormat("en-US", {
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
        hour12: false,
      }).format(new Date());
    }
  };
  updateClock();
  window.setInterval(updateClock, 1000);

  document.querySelectorAll("[data-year]").forEach((element) => {
    element.textContent = String(new Date().getFullYear());
  });

  const revealElements = document.querySelectorAll(".reveal");
  if (reducedMotion || !("IntersectionObserver" in window)) {
    revealElements.forEach((element) => element.classList.add("visible"));
  } else {
    const revealObserver = new IntersectionObserver(
      (entries, observer) => {
        entries.forEach((entry) => {
          if (entry.isIntersecting) {
            entry.target.classList.add("visible");
            observer.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.12 },
    );
    revealElements.forEach((element) => revealObserver.observe(element));
  }

  const tabs = [...document.querySelectorAll("[role='tab']")];
  const activateTab = (tab) => {
    tabs.forEach((candidate) => {
      const selected = candidate === tab;
      candidate.setAttribute("aria-selected", String(selected));
      candidate.tabIndex = selected ? 0 : -1;
      const panel = document.querySelector(`#panel-${candidate.dataset.tab}`);
      if (panel) panel.hidden = !selected;
    });
  };

  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => activateTab(tab));
    tab.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key))
        return;
      event.preventDefault();
      let nextIndex = index;
      if (event.key === "ArrowRight") nextIndex = (index + 1) % tabs.length;
      if (event.key === "ArrowLeft")
        nextIndex = (index - 1 + tabs.length) % tabs.length;
      if (event.key === "Home") nextIndex = 0;
      if (event.key === "End") nextIndex = tabs.length - 1;
      tabs[nextIndex].focus();
      activateTab(tabs[nextIndex]);
    });
  });
  if (tabs.length) activateTab(tabs[0]);

  document.querySelectorAll("[data-copy]").forEach((button) => {
    button.addEventListener("click", async () => {
      const visiblePanel = document.querySelector(
        ".terminal [role='tabpanel']:not([hidden])",
      );
      if (!visiblePanel) return;
      const originalLabel = button.textContent;
      try {
        await navigator.clipboard.writeText(visiblePanel.textContent.trim());
        button.textContent = "Copied";
      } catch {
        button.textContent = "Select text";
      }
      window.setTimeout(() => {
        button.textContent = originalLabel;
      }, 1600);
    });
  });

  const playMesh = document.querySelector("[data-play-mesh]");
  const eventList = document.querySelector("[data-event-list]");
  const agentCount = document.querySelector("[data-agent-count]");
  const controls = document.querySelector(".play-controls");
  const initialEvents = eventList?.innerHTML ?? "";
  const initialAgents = playMesh?.innerHTML ?? "";
  const providers = ["cursor", "worker", "gemini", "ollama", "planner"];
  const tasks = [
    "docs.install",
    "api.review",
    "tests.swarm",
    "release.notes",
    "ui.polish",
  ];
  let startedAt = performance.now();
  let claimIndex = 1;
  let providerIndex = 0;

  const agents = () => [
    ...(playMesh?.querySelectorAll("[data-play-agent]") ?? []),
  ];

  const timestamp = () => {
    const seconds = Math.max(0, (performance.now() - startedAt) / 1000);
    return seconds.toFixed(1).padStart(4, "0");
  };

  const addEvent = (kind, description) => {
    if (!eventList) return;
    const item = document.createElement("li");
    const className = {
      JOIN: "event-join",
      CLAIM: "event-claim",
      MSG: "event-message",
      MEM: "event-memory",
    }[kind];
    const time = document.createElement("time");
    time.textContent = timestamp();
    const badge = document.createElement("span");
    badge.className = className ?? "event-memory";
    badge.textContent = kind;
    const text = document.createElement("p");
    text.innerHTML = description;
    item.append(time, badge, text);
    eventList.prepend(item);
    while (eventList.children.length > 9) eventList.lastElementChild?.remove();
  };

  const updateAgentCount = () => {
    if (agentCount) agentCount.textContent = `${agents().length} live`;
  };

  const addAgent = () => {
    if (!playMesh) return;
    const currentAgents = agents();
    if (currentAgents.length >= 8) {
      addEvent("MEM", "Mesh capacity reached for this demo");
      return;
    }
    const index = currentAgents.length;
    const provider = providers[providerIndex % providers.length];
    providerIndex += 1;
    const element = document.createElement("div");
    element.className = `play-agent play-agent-${index}`;
    element.dataset.playAgent = String(index);
    const status = index % 2 === 0 ? "ready" : "recalling";
    element.innerHTML = `<i></i><b>${provider}-${String(index + 1).padStart(2, "0")}</b><small>${status}</small>`;
    playMesh.append(element);
    updateAgentCount();
    addEvent(
      "JOIN",
      `<b>${provider}-${String(index + 1).padStart(2, "0")}</b> connected · PID ${4101 + index}`,
    );
  };

  const claimTask = () => {
    const currentAgents = agents();
    if (!currentAgents.length) return;
    currentAgents.forEach((agent) => agent.classList.remove("claimed"));
    const winner = currentAgents[claimIndex % currentAgents.length];
    claimIndex += 1;
    winner.classList.add("claimed");
    const status = winner.querySelector("small");
    if (status) status.textContent = "lease · 15m";
    const name = winner.querySelector("b")?.textContent ?? "agent";
    const task = tasks[(claimIndex - 2) % tasks.length];
    addEvent("CLAIM", `<b>${name}</b> owns ${task}`);
  };

  const sendHandoff = () => {
    const currentAgents = agents();
    if (currentAgents.length < 2) {
      addEvent("MEM", "Add another agent before sending a handoff");
      return;
    }
    const fromIndex = Math.floor(Math.random() * currentAgents.length);
    let toIndex = Math.floor(Math.random() * currentAgents.length);
    if (toIndex === fromIndex) toIndex = (toIndex + 1) % currentAgents.length;
    const from =
      currentAgents[fromIndex].querySelector("b")?.textContent ?? "agent";
    const to =
      currentAgents[toIndex].querySelector("b")?.textContent ?? "agent";
    const targetStatus = currentAgents[toIndex].querySelector("small");
    if (targetStatus) targetStatus.textContent = "inbox · 1 new";
    addEvent("MSG", `<b>${from}</b> → <b>${to}</b> · handoff delivered`);
  };

  const resetPlayground = () => {
    if (playMesh) playMesh.innerHTML = initialAgents;
    if (eventList) eventList.innerHTML = initialEvents;
    startedAt = performance.now();
    claimIndex = 1;
    providerIndex = 0;
    updateAgentCount();
  };

  controls?.addEventListener("click", (event) => {
    const button = event.target.closest("button[data-action]");
    if (!button) return;
    const actions = {
      add: addAgent,
      claim: claimTask,
      handoff: sendHandoff,
      reset: resetPlayground,
    };
    actions[button.dataset.action]?.();
  });
})();
