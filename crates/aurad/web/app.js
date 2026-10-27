// Aura web UI. A client of the daemon: it reads status and sends the same
// requests the `aura` CLI sends. Closing the page changes nothing about running work.
// All agent-written text is inserted with textContent, never as HTML.
(() => {
  "use strict";

  const TOKEN_KEY = "aura.token";
  const LAYOUT_KEY = "aura.layout.v1";
  const POLL_MS = 1000;
  const EVENT_KEEP = 400;

  // Keep the access token from the link printed by `aura web`, then remove it from the address bar.
  const fragment = new URLSearchParams(location.hash.slice(1));
  if (fragment.get("token")) {
    try {
      localStorage.setItem(TOKEN_KEY, fragment.get("token"));
    } catch (_) {
      /* private mode: the token is kept in memory for this page only */
    }
    history.replaceState(null, "", location.pathname);
  }
  let token = "";
  try {
    token = localStorage.getItem(TOKEN_KEY) || "";
  } catch (_) {
    token = "";
  }
  if (fragment.get("token")) token = fragment.get("token");

  // ---- API -------------------------------------------------------------------

  async function call(cmd, args = {}) {
    const res = await fetch("/api", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer " + token,
      },
      body: JSON.stringify({ cmd, args }),
    });
    if (res.status === 401) {
      throw new Error("not authorised: open the link printed by `aura web`");
    }
    const body = await res.json();
    if (!body.ok) throw new Error(body.error || "request failed");
    return body.data;
  }

  // ---- DOM helpers (text only) -----------------------------------------------

  function el(tag, props, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
      if (value === undefined || value === null || value === false) continue;
      if (key === "class") node.className = value;
      else if (key === "text") node.textContent = String(value);
      else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
      else if (key === "disabled") node.disabled = !!value;
      else node.setAttribute(key, String(value));
    }
    for (const child of children) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  function stateClass(state) {
    return "state-" + String(state || "").replace(/_/g, "-");
  }

  function stateSpan(state) {
    return el("span", { class: stateClass(state), text: state });
  }

  function emptyLine(text) {
    return el("div", { class: "empty", text });
  }

  function table(headers, rows) {
    return el(
      "table",
      {},
      el("thead", {}, el("tr", {}, ...headers.map((h) => el("th", { text: h })))),
      el("tbody", {}, ...rows)
    );
  }

  function shortTime(ts) {
    if (!ts) return "";
    const d = new Date(ts);
    return isNaN(d) ? ts : d.toLocaleTimeString();
  }

  // ---- shared state ----------------------------------------------------------

  const store = {
    status: null,
    runs: [],
    selectedTask: null,
    detail: null,
    selectedRun: null,
    log: [],
    events: [],
    cursor: 0,
    lastError: "",
  };

  let instances = new Set();
  let dock = null;

  function renderAll() {
    for (const panel of instances) {
      try {
        panel.render();
      } catch (err) {
        console.error(err);
      }
    }
    renderBar();
  }

  function setMessage(text) {
    document.getElementById("msg").textContent = text || "";
  }

  // Runs an action, reports the outcome, and refreshes straight away.
  async function act(description, fn) {
    try {
      const result = await fn();
      setMessage(typeof result === "string" ? result : description + " done");
    } catch (err) {
      setMessage(description + " failed: " + err.message);
    }
    await refresh();
  }

  // ---- panels ----------------------------------------------------------------

  // A panel is a dockview content renderer. `signature` decides when to rebuild,
  // so typing into a form is not wiped out by the one-second refresh.
  class Panel {
    constructor(title) {
      this.title = title;
      this.element = el("div", { class: "panel" });
      this.signature = null;
    }
    // Called by dockview once the panel is attached. Nothing to set up here.
    init() {}
    update(signature, build) {
      if (signature === this.signature) return;
      this.signature = signature;
      this.element.replaceChildren(build());
    }
    dispose() {
      instances.delete(this);
    }
  }

  class TasksPanel extends Panel {
    render() {
      const tasks = (store.status && store.status.tasks) || [];
      const sig = JSON.stringify([store.selectedTask, tasks.map((t) => [t.id, t.state, t.runs_used, t.title])]);
      this.update(sig, () => {
        if (tasks.length === 0) return emptyLine("No tasks yet. Give the planner a goal in Directives.");
        const rows = tasks.map((t) =>
          el(
            "tr",
            {
              class: "clickable" + (t.id === store.selectedTask ? " selected" : ""),
              onclick: () => {
                store.selectedTask = t.id;
                store.detail = null;
                renderAll();
                refresh();
              },
            },
            el("td", { text: t.id }),
            el("td", {}, stateSpan(t.state)),
            el("td", { text: t.runs_used + "/" + t.max_runs }),
            el("td", { text: t.title })
          )
        );
        return table(["id", "state", "runs", "title"], rows);
      });
    }
  }

  class TaskPanel extends Panel {
    render() {
      const detail = store.detail;
      const sig = JSON.stringify([store.selectedTask, detail]);
      this.update(sig, () => {
        if (!store.selectedTask || !detail) return emptyLine("Select a task in the Tasks panel.");
        const t = detail.task;
        const children = [
          el("h3", { text: t.id + "  " + t.title }),
          el("div", {}, "state ", stateSpan(t.state), el("span", { class: "muted", text: "   runs " + t.runs_used + "/" + t.max_runs })),
          el("div", { class: "muted", text: "branch " + t.branch }),
          el("div", { class: "muted", text: "write scope: " + (t.scope.write || []).join(", ") }),
          el("h3", { text: "brief" }),
          el("div", { style: "white-space:pre-wrap", text: t.brief || "(none)" }),
        ];

        const actions = [];
        if (["proposed", "gated", "blocked"].includes(t.state)) {
          actions.push(
            el("button", {
              type: "button",
              text: t.state === "gated" ? "Approve merge" : t.state === "blocked" ? "Unblock" : "Approve",
              onclick: () => act("approve", () => call("approve", { id: t.id })),
            })
          );
        }
        if (!["done", "cancelled"].includes(t.state)) {
          actions.push(
            el("button", {
              type: "button",
              class: "danger",
              text: "Cancel",
              onclick: () => {
                if (confirm("Cancel " + t.id + "? Its branch is kept.")) {
                  act("cancel", () => call("cancel", { id: t.id }));
                }
              },
            })
          );
        }
        if (actions.length) children.push(el("div", { class: "row" }, ...actions));

        if (t.notes && t.notes.length) {
          children.push(el("h3", { text: "notes" }));
          for (const n of t.notes) children.push(el("div", { class: "note", text: n }));
        }

        const open = (detail.questions || []).filter((q) => q.answer === null || q.answer === undefined);
        for (const q of detail.questions || []) {
          if (open.includes(q)) continue;
          children.push(el("div", { class: "muted", text: "answered " + q.id + ": " + q.text + " → " + q.answer }));
        }
        for (const q of open) {
          const input = el("input", { type: "text", placeholder: "your answer" });
          children.push(
            el(
              "div",
              { class: "question" },
              el("div", { text: q.id + ": " + q.text }),
              el(
                "div",
                { class: "row" },
                input,
                el("button", {
                  type: "button",
                  text: "Answer",
                  onclick: () => {
                    const text = input.value.trim();
                    if (!text) return setMessage("type an answer first");
                    act("answer", () => call("answer", { question: q.id, text }));
                  },
                })
              )
            )
          );
        }

        if (detail.runs && detail.runs.length) {
          children.push(el("h3", { text: "runs" }));
          children.push(
            table(
              ["run", "role", "provider", "state", "reason"],
              detail.runs.map((r) =>
                el(
                  "tr",
                  { class: "clickable", onclick: () => selectRun(r.id) },
                  el("td", { text: r.id }),
                  el("td", { text: r.role }),
                  el("td", { text: r.provider }),
                  el("td", {}, stateSpan(r.state)),
                  el("td", { text: r.reason || "" })
                )
              )
            )
          );
        }
        return el("div", {}, ...children);
      });
    }
  }

  class DirectivesPanel extends Panel {
    constructor(title) {
      super(title);
      this.text = el("textarea", { placeholder: "Describe the goal for the planner…" });
      this.autonomy = el(
        "select",
        {},
        el("option", { value: "", text: "default autonomy" }),
        el("option", { value: "ask-plan", text: "ask-plan" }),
        el("option", { value: "ask-each", text: "ask-each" }),
        el("option", { value: "auto", text: "auto" })
      );
      this.form = el(
        "div",
        {},
        el("h3", { text: "new directive" }),
        this.text,
        el(
          "div",
          { class: "row" },
          this.autonomy,
          el("button", {
            type: "button",
            text: "Send to planner",
            onclick: () => this.submit(),
          })
        )
      );
      this.list = el("div", {});
      this.element.append(this.form, el("h3", { text: "directives" }), this.list);
    }
    submit() {
      const text = this.text.value.trim();
      if (!text) return setMessage("write a directive first");
      const args = { text };
      if (this.autonomy.value) args.autonomy = this.autonomy.value;
      act("directive", () => call("directive.create", args)).then(() => {
        this.text.value = "";
      });
    }
    render() {
      const directives = (store.status && store.status.directives) || [];
      // Only the list is rebuilt, so the form keeps whatever was typed into it.
      const sig = JSON.stringify(directives.map((d) => [d.id, d.state, d.text]));
      if (sig !== this.listSig) {
        this.listSig = sig;
        this.list.replaceChildren(
          ...(directives.length
            ? directives.map((d) =>
                el(
                  "div",
                  { class: "row" },
                  el("span", { text: d.id }),
                  stateSpan(d.state),
                  el("span", { class: "muted", text: d.autonomy }),
                  el("span", { text: d.text })
                )
              )
            : [emptyLine("No directives yet.")])
        );
      }
    }
  }

  class LogPanel extends Panel {
    constructor(title) {
      super(title);
      this.select = el("select", { onchange: (e) => selectRun(e.target.value) });
      this.pre = el("pre", { class: "log" });
      this.element.append(el("h3", { text: "live log" }), el("div", { class: "row" }, this.select), this.pre);
    }
    render() {
      const runs = store.runs || [];
      const sig = JSON.stringify([runs.map((r) => r.id), store.selectedRun]);
      if (sig !== this.runSig) {
        this.runSig = sig;
        const options = runs.map((r) =>
          el("option", { value: r.id, text: r.id + "  " + r.role + "  " + r.state + "  " + (r.task_id || "") })
        );
        if (options.length === 0) options.push(el("option", { value: "", text: "no runs yet" }));
        this.select.replaceChildren(...options);
        this.select.value = store.selectedRun || "";
      }
      this.pre.textContent = store.selectedRun ? store.log.join("\n") || "(empty log)" : "Select a run to follow its log.";
    }
  }

  class QuotaPanel extends Panel {
    render() {
      const quotas = (store.status && store.status.quotas) || [];
      const active = (store.status && store.status.active_runs) || [];
      const sig = JSON.stringify([quotas, active.map((r) => r.id), store.status && store.status.next_quota_wake]);
      this.update(sig, () => {
        const children = [
          el("h3", { text: "provider quota" }),
          table(
            ["provider", "state", "running", "retry at"],
            quotas.map((q) =>
              el(
                "tr",
                {},
                el("td", { text: q.provider }),
                el("td", {}, el("span", { class: q.state === "available" ? "state-done" : "state-proposed", text: q.state })),
                el("td", { text: q.running }),
                el("td", { text: q.state === "available" ? "" : shortTime(q.retry_at) + (q.source ? " (" + q.source + ")" : "") })
              )
            )
          ),
          el("h3", { text: "active runs" }),
        ];
        if (active.length === 0) children.push(emptyLine("none"));
        for (const r of active) {
          children.push(el("div", { text: r.id + "  " + r.role + "  " + r.provider + "  task " + (r.task_id || "-") }));
        }
        if (store.status && store.status.next_quota_wake) {
          children.push(el("div", { class: "muted", text: "next refresh: " + shortTime(store.status.next_quota_wake) }));
        }
        return el("div", {}, ...children);
      });
    }
  }

  class EventsPanel extends Panel {
    render() {
      const sig = store.cursor + ":" + store.events.length;
      this.update(sig, () => {
        if (store.events.length === 0) return emptyLine("No events yet.");
        return el(
          "div",
          {},
          ...store.events
            .slice()
            .reverse()
            .map((e) =>
              el(
                "div",
                { class: "event" },
                el("span", { class: "ts", text: shortTime(e.ts) + "  " }),
                el("span", { class: "kind", text: e.kind + "  " }),
                el("span", { text: (e.subject || "") + "  " }),
                el("span", { class: "muted", text: JSON.stringify(e.payload) })
              )
            )
        );
      });
    }
  }

  const FACTORIES = {
    tasks: () => new TasksPanel("Tasks"),
    task: () => new TaskPanel("Task"),
    directives: () => new DirectivesPanel("Directives"),
    logs: () => new LogPanel("Live log"),
    quota: () => new QuotaPanel("Quota & runs"),
    events: () => new EventsPanel("Events"),
  };

  // ---- navigation ------------------------------------------------------------

  async function selectRun(id) {
    store.selectedRun = id || null;
    store.log = [];
    await refreshLog();
    renderAll();
  }

  // ---- refresh loop ----------------------------------------------------------

  async function refreshLog() {
    if (!store.selectedRun) return;
    try {
      const data = await call("logs", { run: store.selectedRun, tail: 300 });
      store.log = data.lines || [];
    } catch (err) {
      store.log = ["(cannot read log: " + err.message + ")"];
    }
  }

  let refreshing = false;
  async function refresh() {
    if (refreshing) return;
    refreshing = true;
    try {
      store.status = await call("status");
      store.runs = await call("runs", { limit: 40 });
      // Follow the newest run until the user picks another one.
      if (!store.selectedRun && store.runs.length) store.selectedRun = store.runs[0].id;
      if (store.selectedTask) {
        store.detail = await call("task.show", { id: store.selectedTask }).catch(() => null);
      }
      await refreshLog();
      const fresh = await call("events", { since: store.cursor, limit: 500 });
      for (const e of fresh) {
        store.events.push(e);
        store.cursor = e.seq;
      }
      if (store.events.length > EVENT_KEEP) store.events.splice(0, store.events.length - EVENT_KEEP);
      store.lastError = "";
      setConnection(true);
    } catch (err) {
      store.lastError = err.message;
      setConnection(false, err.message);
    } finally {
      refreshing = false;
      renderAll();
    }
  }

  function setConnection(ok, message) {
    const pill = document.getElementById("conn");
    pill.className = "pill " + (ok ? "ok" : "bad");
    pill.textContent = ok ? "connected" : "disconnected";
    pill.title = message || "";
  }

  function renderBar() {
    const s = store.status;
    const paused = !!(s && s.paused);
    const pauseButton = document.getElementById("pause");
    pauseButton.textContent = paused ? "Resume" : "Pause";
    pauseButton.disabled = !s;
    document.getElementById("kill").disabled = !s;
    document.getElementById("repo").textContent = s ? s.repo + " (" + s.main_branch + ")" : "";
  }

  // ---- layout ----------------------------------------------------------------

  function defaultLayout(api) {
    api.addPanel({ id: "tasks", component: "tasks", title: "Tasks" });
    api.addPanel({ id: "task", component: "task", title: "Task", position: { referencePanel: "tasks", direction: "right" } });
    api.addPanel({ id: "logs", component: "logs", title: "Live log", position: { referencePanel: "task", direction: "below" } });
    api.addPanel({ id: "directives", component: "directives", title: "Directives", position: { referencePanel: "tasks", direction: "below" } });
    api.addPanel({ id: "quota", component: "quota", title: "Quota & runs", position: { referencePanel: "logs", direction: "right" } });
    api.addPanel({ id: "events", component: "events", title: "Events", position: { referencePanel: "quota", direction: "below" } });
  }

  function startDock() {
    const { DockviewComponent } = window["dockview-core"];
    dock = new DockviewComponent(document.getElementById("dock"), {
      className: "dockview-theme-dark",
      createComponent: ({ name }) => {
        const make = FACTORIES[name];
        if (!make) throw new Error("unknown panel " + name);
        const panel = make();
        instances.add(panel);
        return panel;
      },
    });

    let restored = false;
    try {
      const saved = localStorage.getItem(LAYOUT_KEY);
      if (saved) {
        dock.fromJSON(JSON.parse(saved));
        restored = true;
      }
    } catch (_) {
      restored = false;
    }
    if (!restored) defaultLayout(dock);

    dock.onDidLayoutChange(() => {
      try {
        localStorage.setItem(LAYOUT_KEY, JSON.stringify(dock.toJSON()));
      } catch (_) {
        /* layout is kept for this session only */
      }
    });
  }

  // ---- wiring ----------------------------------------------------------------

  document.getElementById("pause").addEventListener("click", () => {
    const paused = !!(store.status && store.status.paused);
    act(paused ? "resume" : "pause", () => call(paused ? "resume" : "pause"));
  });
  document.getElementById("kill").addEventListener("click", () => {
    if (confirm("Stop every run now? Running work returns to the queue and the scheduler pauses.")) {
      act("kill", () => call("kill", { confirm: true }).then(() => "stopped; scheduler paused"));
    }
  });
  document.getElementById("reset-layout").addEventListener("click", () => {
    try {
      localStorage.removeItem(LAYOUT_KEY);
    } catch (_) {
      /* nothing to clear */
    }
    location.reload();
  });

  startDock();
  refresh();
  setInterval(refresh, POLL_MS);
})();
