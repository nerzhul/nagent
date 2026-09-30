// Per-user credentials + server-side agent catalogue UI.
//
// The drawer is a single flat list with two flavours of rows:
//
// 1. **Agents** (top half) — every server-side tool the LLM can call,
//    fetched from `/v1/agents`. These are *informational*: the server
//    ships them preconfigured, the user does not need to fill any
//    field. Listing them here gives the user a single place to audit
//    what the chat can actually do, rather than the separate banner
//    that only mentions names.
//
// 3. **Per-user integrations** (bottom half) — `ServiceDef` entries
//    that DO need per-user credentials, fetched from
//    `/api/integrations`. Each row carries a "Configure" / "Edit"
//    button that opens the modal PUT/DELETE flow. CSRF tokens come
//    from `window.nagentAuth.csrfHeaders()` so the same middleware
//    that protects every other mutating request also covers us.
//
// Auth gate: `/api/integrations*` routes are behind `RequireAuth`
// and return 401 for anonymous clients. `/v1/agents` is anonymous
// (same envelope shape — see `chat.js::loadAgentsBanner`). The
// section is hidden until `window.nagentAuth.getUser()` returns a
// user, and the open/close listener refreshes the list on toggle.

const SECTION_ID = "chat-integrations";
const LIST_ID = "chat-integrations-list";

function csrfHeaders() {
  return window.nagentAuth?.csrfHeaders?.() || undefined;
}

function isAuthed() {
  return !!window.nagentAuth?.getUser?.();
}

function setSectionVisible(visible) {
  const el = document.getElementById(SECTION_ID);
  if (!el) return;
  el.hidden = !visible;
}

async function fetchIntegrations() {
  const resp = await fetch("/api/integrations", { credentials: "same-origin" });
  if (resp.status === 401) return null;
  if (!resp.ok) throw new Error(`integrations list returned ${resp.status}`);
  const body = await resp.json();
  return body.data || [];
}

// Fetch the agent catalogue. `/v1/agents` is anonymous (same as the
// chat-completions route family) and returns `{ data: [{ name,
// description }, ...] }`. A 404 means the operator compiled the
// build without any agent cargo feature — return `[]` so the drawer
// still renders the per-user credentials rows below without a
// confusing "agents unavailable" placeholder.
async function fetchAgents() {
  try {
    const resp = await fetch("/v1/agents", {
      cache: "no-store",
      credentials: "same-origin",
    });
    if (resp.status === 404) return [];
    if (!resp.ok) throw new Error(`/v1/agents returned ${resp.status}`);
    const body = await resp.json();
    const arr = Array.isArray(body?.data) ? body.data : [];
    return arr.filter((a) => a && typeof a.name === "string");
  } catch (e) {
    console.warn("agents list failed:", e);
    return [];
  }
}

function renderAgentRow(agent) {
  const li = document.createElement("li");
  li.className = "chat-integration-row chat-integration-row--agent";
  li.dataset.agentName = agent.name;

  const icon = document.createElement("span");
  icon.className = "chat-integration-icon";
  icon.textContent = "🛠";
  li.appendChild(icon);

  const label = document.createElement("span");
  label.className = "chat-integration-label";
  label.textContent = agent.name;
  if (agent.description) label.title = agent.description;
  li.appendChild(label);

  const status = document.createElement("span");
  status.className = "chat-integration-status chat-integration-status--agent";
  status.textContent = "Server-side tool";
  li.appendChild(status);

  // Informational rows only — no Configure button. We still keep a
  // ghost link so the visual weight of the row matches the
  // per-user rows below (icon + label + status pill + button slot).
  const placeholder = document.createElement("span");
  placeholder.className = "chat-integration-edit chat-integration-edit--agent";
  placeholder.setAttribute("aria-hidden", "true");
  li.appendChild(placeholder);

  if (agent.description) {
    const desc = document.createElement("p");
    desc.className = "chat-integration-description";
    desc.textContent = agent.description;
    li.appendChild(desc);
  }
  return li;
}

function renderIntegrationRow(svc) {
  const li = document.createElement("li");
  li.className = "chat-integration-row";
  li.dataset.serviceId = svc.id;
  li.dataset.configured = svc.configured ? "1" : "0";

  const icon = document.createElement("span");
  icon.className = "chat-integration-icon";
  icon.textContent = svc.icon || "•";
  li.appendChild(icon);

  const label = document.createElement("span");
  label.className = "chat-integration-label";
  label.textContent = svc.display_name;
  li.appendChild(label);

  const status = document.createElement("span");
  status.className = "chat-integration-status";
  status.textContent = svc.configured ? "Configured" : "Not configured";
  li.appendChild(status);

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "chat-integration-edit ghost";
  btn.textContent = svc.configured ? "Edit" : "Configure";
  btn.addEventListener("click", () => openEditor(svc));
  li.appendChild(btn);
  return li;
}

async function renderList() {
  const root = document.getElementById(LIST_ID);
  if (!root) return;
  root.replaceChildren();
  if (!isAuthed()) {
    setSectionVisible(false);
    return;
  }
  // Fetch in parallel — the two endpoints are independent and
  // parallelising shaves a round-trip off the first paint of the
  // drawer.
  let data = null;
  let agents = [];
  try {
    [data, agents] = await Promise.all([
      fetchIntegrations(),
      fetchAgents(),
    ]);
  } catch (e) {
    // `Promise.all` rejects on the first failure; treat the same as
    // a single-fetch rejection so we don't half-render the list.
    console.warn("integrations list failed:", e);
    return;
  }
  if (data === null) {
    setSectionVisible(false);
    return;
  }
  setSectionVisible(true);
  let appended = 0;
  for (const agent of agents) {
    root.appendChild(renderAgentRow(agent));
    appended++;
  }
  for (const svc of data) {
    root.appendChild(renderIntegrationRow(svc));
    appended++;
  }
  if (appended === 0) {
    const empty = document.createElement("li");
    empty.className = "chat-integrations-empty";
    empty.textContent = "No integrations available yet.";
    root.appendChild(empty);
  }
}

function buildFormField(field) {
  const wrap = document.createElement("label");
  wrap.className = "chat-integration-field";
  const text = document.createElement("span");
  text.textContent = field.label;
  wrap.appendChild(text);
  const input = document.createElement("input");
  input.name = field.key;
  input.dataset.key = field.key;
  input.type =
    field.kind === "password"
      ? "password"
      : field.kind === "url"
      ? "url"
      : "text";
  if (field.placeholder) input.placeholder = field.placeholder;
  // Never auto-fill; the server returns no plaintext, only the
  // `filled` boolean, so we leave the field empty.
  input.autocomplete = "off";
  input.spellcheck = false;
  wrap.appendChild(input);
  if (field.help) {
    const help = document.createElement("small");
    help.textContent = field.help;
    wrap.appendChild(help);
  }
  return wrap;
}

function openEditor(svc) {
  const overlay = document.createElement("div");
  overlay.className = "chat-integration-overlay";

  const modal = document.createElement("div");
  modal.className = "chat-integration-modal";
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");

  const title = document.createElement("h3");
  title.textContent = `Configure ${svc.display_name}`;
  modal.appendChild(title);

  const form = document.createElement("form");
  form.className = "chat-integration-form";
  for (const f of svc.fields) form.appendChild(buildFormField(f));
  modal.appendChild(form);

  const actions = document.createElement("div");
  actions.className = "chat-integration-actions";

  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "ghost";
  cancel.textContent = "Cancel";
  cancel.addEventListener("click", () => overlay.remove());
  actions.appendChild(cancel);

  if (svc.configured) {
    const del = document.createElement("button");
    del.type = "button";
    del.className = "danger";
    del.textContent = "Remove";
    del.addEventListener("click", async () => {
      try {
        await deleteCredentials(svc.id);
        overlay.remove();
        await renderList();
      } catch (e) {
        console.error("delete credentials failed:", e);
        alert(`Delete failed: ${e.message}`);
      }
    });
    actions.appendChild(del);
  }

  const save = document.createElement("button");
  save.type = "submit";
  save.className = "primary";
  save.textContent = "Save";
  actions.appendChild(save);
  modal.appendChild(actions);

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const fields = {};
    for (const input of form.querySelectorAll("input")) {
      if (!input.value) continue;
      fields[input.dataset.key] = input.value;
    }
    try {
      await putCredentials(svc.id, fields);
      overlay.remove();
      await renderList();
    } catch (err) {
      console.error("save credentials failed:", err);
      alert(`Save failed: ${err.message}`);
    }
  });
  overlay.appendChild(modal);
  document.body.appendChild(overlay);
}

async function putCredentials(serviceId, fields) {
  const resp = await fetch(
    `/api/integrations/${encodeURIComponent(serviceId)}/credentials`,
    {
      method: "PUT",
      credentials: "same-origin",
      headers: {
        "content-type": "application/json",
        ...(csrfHeaders() || {}),
      },
      body: JSON.stringify({ fields }),
    },
  );
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`HTTP ${resp.status}: ${text}`);
  }
}

async function deleteCredentials(serviceId) {
  const resp = await fetch(
    `/api/integrations/${encodeURIComponent(serviceId)}/credentials`,
    {
      method: "DELETE",
      credentials: "same-origin",
      headers: { ...(csrfHeaders() || {}) },
    },
  );
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`HTTP ${resp.status}: ${text}`);
  }
}

function bindOpenRefresh() {
  const section = document.getElementById(SECTION_ID);
  if (!section) return;
  section.addEventListener("toggle", () => {
    if (!section.open) return;
    renderList();
  });
}

function refreshOnAuthChange() {
  // `auth.js` toggles `nagentAuth.getUser()` whenever a login /
  // logout / probe completes. We poll once a second for the first
  // few seconds so a cold reload after auth has settled still
  // surfaces the section. Cheap (a getter + a single boolean
  // compare) and stops itself once the section has been shown at
  // least once.
  let attempts = 0;
  const tick = () => {
    attempts += 1;
    const visible = !document.getElementById(SECTION_ID)?.hidden;
    if (isAuthed() && !visible) {
      renderList();
    } else if (!isAuthed() && visible) {
      setSectionVisible(false);
    }
    if (attempts < 30) setTimeout(tick, 1000);
  };
  tick();
}

bindOpenRefresh();
refreshOnAuthChange();
