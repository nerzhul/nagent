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
// Settings tab surface (plan 1790963194218 §2.7). Same data
// as the chat-coupled drawer, but rendered with the
// "settings-section" visual weight so users find the
// "Configure CalDAV" affordance without digging through the
// Discussion view's Advanced disclosure. The chat drawer
// stays for the chat-coupled "I can ask the model about X"
// hint; the Settings tab is the setup entry point.
const SETTINGS_LIST_ID = "settings-integrations-list";
const SETTINGS_SECTION_ID = "settings-integrations";

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

// Settings-tab variant: the visual weight matches
// `.settings-section` (no chat-integration-icon leading), the
// status pill is more compact, and the action button is the
// primary CTA so users spot "Configure CalDAV" without
// hunting. The two surfaces share `openEditor` so the
// discover flow stays single-sourced.
function renderSettingsIntegrationRow(svc) {
  const li = document.createElement("li");
  li.className = "settings-integration-row";
  li.dataset.serviceId = svc.id;
  li.dataset.configured = svc.configured ? "1" : "0";

  // Header row: icon + label on the left, status + CTA on the
  // right. Flex-wrap lets the status pill / CTA drop under the
  // label on narrow viewports (mobile / side panel).
  const header = document.createElement("div");
  header.className = "settings-integration-header";

  const titleGroup = document.createElement("div");
  titleGroup.className = "settings-integration-title";
  const icon = document.createElement("span");
  icon.className = "settings-integration-icon";
  icon.textContent = svc.icon || "•";
  icon.setAttribute("aria-hidden", "true");
  titleGroup.appendChild(icon);

  const label = document.createElement("span");
  label.className = "settings-integration-label";
  label.textContent = svc.display_name;
  titleGroup.appendChild(label);
  header.appendChild(titleGroup);

  const actions = document.createElement("div");
  actions.className = "settings-integration-actions";

  const status = document.createElement("span");
  status.className = `settings-integration-status${
    svc.configured ? " is-configured" : " is-unconfigured"
  }`;
  status.textContent = svc.configured ? "Configured" : "Not configured";
  actions.appendChild(status);

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = svc.configured ? "ghost" : "primary";
  btn.textContent = svc.configured ? "Edit" : "Configure";
  btn.addEventListener("click", () => openEditor(svc));
  actions.appendChild(btn);

  header.appendChild(actions);
  li.appendChild(header);

  // Surface the operator-facing docs URL when the ServiceDef
  // has one — the CalDAV connector ships a setup walkthrough
  // that explains Nextcloud / Radicale / Fastmail / iCloud
  // specifics. The docs line lives on its own line so the
  // card has clear vertical rhythm and the link has room to
  // breathe.
  if (svc.docs_url) {
    const docs = document.createElement("a");
    docs.className = "settings-integration-docs";
    docs.href = svc.docs_url;
    docs.target = "_blank";
    docs.rel = "noopener noreferrer";
    docs.textContent = "Setup guide →";
    li.appendChild(docs);
  }

  return li;
}

async function renderList() {
  const root = document.getElementById(LIST_ID);
  const settingsRoot = document.getElementById(SETTINGS_LIST_ID);
  if (!root && !settingsRoot) return;
  if (root) root.replaceChildren();
  if (settingsRoot) settingsRoot.replaceChildren();
  if (!isAuthed()) {
    setSectionVisible(false);
    if (settingsRoot) {
      // The Settings tab is auth-gated: replace the
      // "Loading…" placeholder with a quiet hint so the
      // user knows the section is not broken.
      const empty = document.createElement("li");
      empty.className = "settings-integrations-empty";
      empty.textContent = "Sign in to configure your integrations.";
      settingsRoot.appendChild(empty);
    }
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
  // The chat-coupled drawer is hidden when the build has no
  // per-user services to show (the same `data` array drives both
  // surfaces — if `data` is empty the drawer stays hidden via
  // setSectionVisible(false), and the Settings tab gets an empty
  // state hint so the user does not think the section is broken).
  let appended = 0;
  if (root) {
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
  if (settingsRoot) {
    if (data.length === 0) {
      // The empty state is intentionally *not* technical. The
      // operator-facing reason (build feature) is documented in
      // the deployment guide; the user just needs to know "this
      // section is empty because there are no integrations to
      // configure right now". A future "Request an integration"
      // link would live here.
      const empty = document.createElement("li");
      empty.className = "settings-integrations-empty";
      empty.textContent =
        "No integrations available yet. New connectors (CalDAV, IMAP, Home Assistant, GitHub, …) will appear here as they are added to your server.";
      settingsRoot.appendChild(empty);
    } else {
      for (const svc of data) {
        settingsRoot.appendChild(renderSettingsIntegrationRow(svc));
      }
    }
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
  // The input lives inside a flex group so a service that
  // exposes a probe affordance (currently CalDAV) can place a
  // button next to the URL input without changing the markup
  // contract for the other services.
  const group = document.createElement("div");
  group.className = "chat-integration-input-group";
  group.appendChild(input);
  wrap.appendChild(group);
  if (field.help) {
    const help = document.createElement("small");
    help.textContent = field.help;
    wrap.appendChild(help);
  }
  return wrap;
}

// ---------------------------------------------------------------------------
// CalDAV setup-only probe (plan 1790963194218 §2.7).
//
// `POST /api/integrations/caldav/probe-calendars` authenticates against
// the principal URL the user pasted and returns the discovered
// `<C:calendar/>` resources. The UI lets the user pick one and
// auto-fills the `url` field with the chosen calendar's href so the
// chat agents target the right collection.
// ---------------------------------------------------------------------------
const CALDAV_SERVICE_ID = "caldav";
const PROBE_PATH = "/api/integrations/caldav/probe-calendars";

// One-line subtitle per service. Sets the user's mental
// model before they read the field labels. Falls back to a
// generic "encrypted at rest" line so future ServiceDefs
// don't render a bare title.
const MODAL_SUBTITLES = {
  caldav:
    "Connect your personal calendar (Nextcloud, Radicale, Fastmail, iCloud, …). Read + add events only — edit/delete are out of scope for v1.",
};

async function probeCalendars(principalUrl, username, password) {
  const resp = await fetch(PROBE_PATH, {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      ...(csrfHeaders() || {}),
    },
    body: JSON.stringify({
      principal_url: principalUrl,
      username,
      password,
    }),
  });
  // The probe handler never returns 401 — auth rejections from
  // CalDAV are mapped to 502 with the upstream body verbatim. So
  // any non-2xx is an error path; we surface the JSON `error`
  // field when present, the raw text otherwise.
  if (!resp.ok) {
    let msg = `HTTP ${resp.status}`;
    try {
      const body = await resp.json();
      if (body && typeof body.error === "string") msg = body.error;
    } catch {
      try {
        msg = `${msg}: ${await resp.text()}`;
      } catch {
        /* fall through */
      }
    }
    throw new Error(msg);
  }
  const body = await resp.json();
  return Array.isArray(body?.data) ? body.data : [];
}

function renderProbeResults(container, calendars, urlField) {
  // Replace any previous probe results.
  container.replaceChildren();
  if (!calendars.length) {
    const empty = document.createElement("p");
    empty.className = "chat-integration-probe-empty";
    empty.textContent =
      "No calendars found at this URL. Double-check the principal URL and credentials, then try again.";
    container.appendChild(empty);
    return;
  }
  const list = document.createElement("ul");
  list.className = "chat-integration-probe-list";
  for (const cal of calendars) {
    const li = document.createElement("li");
    li.className = "chat-integration-probe-item";
    const name = document.createElement("span");
    name.className = "chat-integration-probe-name";
    name.textContent = cal.display_name || cal.href;
    const href = document.createElement("code");
    href.className = "chat-integration-probe-href";
    href.textContent = cal.href;
    const pick = document.createElement("button");
    pick.type = "button";
    pick.className = "primary";
    pick.textContent = "Use this";
    pick.addEventListener("click", () => {
      // Auto-fill the `url` field with the picked calendar's
      // href and visually highlight the change so the user
      // knows what happened before they hit Save.
      urlField.value = cal.href;
      urlField.dispatchEvent(new Event("input", { bubbles: true }));
      urlField.focus();
      urlField.select();
    });
    li.append(name, href, pick);
    list.appendChild(li);
  }
  container.appendChild(list);
}

function attachCalDavDiscover(form, modal) {
  // Pull the three fields the probe needs out of the form.
  const urlField = form.querySelector('input[data-key="url"]');
  const usernameField = form.querySelector('input[data-key="username"]');
  const passwordField = form.querySelector('input[data-key="password"]');
  if (!urlField || !usernameField || !passwordField) return;

  // The URL field is now wrapped in a `.chat-integration-input-group`
  // by `buildFormField`; we drop the discover button into that
  // group so the layout reads as a single "URL + action" row.
  const urlGroup = urlField.parentElement;
  if (!urlGroup || !urlGroup.classList.contains("chat-integration-input-group")) {
    return;
  }
  const discoverBtn = document.createElement("button");
  discoverBtn.type = "button";
  discoverBtn.className = "ghost";
  discoverBtn.textContent = "Discover";
  urlGroup.appendChild(discoverBtn);

  // Status + results live in a container that we insert right
  // after the URL field group. Keeps the affordance visually
  // attached to the URL row without crowding the input.
  const discoverStatus = document.createElement("span");
  discoverStatus.className = "chat-integration-discover-status";
  discoverStatus.setAttribute("role", "status");
  discoverStatus.setAttribute("aria-live", "polite");
  const results = document.createElement("div");
  results.className = "chat-integration-probe-results";
  urlField.closest(".chat-integration-field").append(
    discoverStatus,
    results,
  );

  discoverBtn.addEventListener("click", async () => {
    // Disable the button while the probe is in flight so a
    // double-click cannot fire two concurrent requests.
    discoverBtn.disabled = true;
    discoverStatus.textContent = "Probing…";
    discoverStatus.dataset.state = "pending";
    results.replaceChildren();
    try {
      // The probe needs the principal URL (where the user is
      // authenticated), not the calendar collection URL. The
      // `help` text on the URL field says "Use the 'Discover'
      // button on this form to probe the server and pick the
      // calendar you want" — the field is initially the
      // principal URL the user pastes; once they pick a
      // calendar we overwrite it with the collection href.
      const calendars = await probeCalendars(
        urlField.value.trim(),
        usernameField.value,
        passwordField.value,
      );
      discoverStatus.textContent = calendars.length
        ? `Found ${calendars.length} calendar(s).`
        : "No calendars found.";
      discoverStatus.dataset.state = calendars.length ? "ok" : "empty";
      renderProbeResults(results, calendars, urlField);
    } catch (e) {
      console.error("caldav probe failed:", e);
      discoverStatus.textContent = `Probe failed: ${e.message}`;
      discoverStatus.dataset.state = "error";
    } finally {
      discoverBtn.disabled = false;
    }
  });
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
  // Subtitle gives a one-liner about the connector so the
  // user lands in the right mental model before reading the
  // field labels. Different services get a hand-written
  // blurb; falls back to the generic "encrypted at rest"
  // reassurance for any future ServiceDef.
  const subtitle = document.createElement("span");
  subtitle.className = "chat-integration-modal-subtitle";
  subtitle.textContent = MODAL_SUBTITLES[svc.id] ||
    "Credentials are encrypted with AES-256-GCM at rest. The chat agents use them when you ask the assistant to read or act on this service.";
  title.appendChild(subtitle);
  modal.appendChild(title);

  const form = document.createElement("form");
  form.className = "chat-integration-form";
  for (const f of svc.fields) form.appendChild(buildFormField(f));
  modal.appendChild(form);

  // Plan 1790963194218: the CalDAV `ServiceDef` ships a
  // setup-only probe endpoint. The chat agents only know how
  // to read / create against a *calendar collection* URL; the
  // probe endpoint authenticates with the principal URL and
  // lets the user pick the collection they want. This is a
  // UX layer on top of the existing credential form — the
  // auto-filled `url` is saved through the same `PUT
  // /api/integrations/caldav/credentials` flow as any other
  // integration.
  if (svc.id === CALDAV_SERVICE_ID) {
    attachCalDavDiscover(form, modal);
  }

  const actions = document.createElement("div");
  actions.className = "chat-integration-actions";

  // Remove (danger) sits on the left when the service is
  // already configured; the spacer pushes Cancel + Save to
  // the right so the destructive action is visually isolated
  // from the positive "Save" CTA — a hard requirement for
  // any modal that can lose data.
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
    const spacer = document.createElement("span");
    spacer.className = "chat-integration-actions-spacer";
    actions.appendChild(spacer);
  } else {
    const spacer = document.createElement("span");
    spacer.className = "chat-integration-actions-spacer";
    actions.appendChild(spacer);
  }

  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "ghost";
  cancel.textContent = "Cancel";
  cancel.addEventListener("click", () => overlay.remove());
  actions.appendChild(cancel);

  const save = document.createElement("button");
  save.type = "submit";
  save.className = "primary";
  save.textContent = svc.configured ? "Save changes" : "Save";
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
