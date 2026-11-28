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

// Fetch a single integration summary, including the decrypted
// `value` for every non-`Password` field whose `echo_on_edit` flag
// is `true`. The list endpoint (`GET /api/integrations`)
// returns no `value` to keep the list cheap — the integrations page
// only needs `configured` + per-field `filled` to render the
// settings page. The edit modal does need the saved plaintexts
// (at minimum the calendar URL for CalDAV) so we re-fetch via
// the per-id endpoint the moment the user clicks Edit. 404 falls
// back to `null` so the caller can show a clean "service no
// longer available" message instead of a raw HTTP error.
async function fetchIntegration(id) {
  const resp = await fetch(
    `/api/integrations/${encodeURIComponent(id)}`,
    { credentials: "same-origin" },
  );
  if (resp.status === 404) return null;
  if (!resp.ok) throw new Error(`integration ${id} returned ${resp.status}`);
  return resp.json();
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
  // Edit / Configure click — refetch the single integration so the
  // form pre-fills the saved URL (and any other `echo_on_edit`
  // field). The list endpoint deliberately omits `field.value`
  // to keep the list cheap; see `fetchIntegration` for the full
  // rationale. The freshly-loaded summary replaces the list-row
  // snapshot we already have so `openEditor` sees the populated
  // `field.value` strings.
  btn.addEventListener("click", () => openIntegrationEditor(svc));
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
  btn.addEventListener("click", () => openIntegrationEditor(svc));
  actions.appendChild(btn);

  header.appendChild(actions);
  li.appendChild(header);

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

  // The form needs a single, unified way to read the user's
  // current value (the submit handler below iterates over
  // every form control and reads `dataset.key + value`).
  // Three different DOM elements share that contract: a
  // plain `<input>` (Text / Password / Url / Number), a
  // `<select>` (Select), a checkbox (`Checkbox`). We branch
  // on `field.kind` here.
  let input;
  if (field.kind === "checkbox") {
    // Wire format is "true" / "false". The submit handler reads
    // `checked` (not `value`, which is always "on" for a
    // checkbox) and always submits the box explicitly, so an
    // unchecked toggle persists "false" instead of being
    // skipped like an empty text input.
    input = document.createElement("input");
    input.type = "checkbox";
    input.name = field.key;
    input.dataset.key = field.key;
    input.checked = field.value === "true";
    wrap.classList.add("chat-integration-field-checkbox");
  } else if (field.kind === "select") {
    input = document.createElement("select");
    input.name = field.key;
    input.dataset.key = field.key;
    input.autocomplete = "off";
    // An empty placeholder option so the user sees a
    // "Pick a value" hint before they touch the dropdown.
    // When the server echoed back a saved value, that
    // option is removed below and the matching one is
    // selected — same UX as a native `<select>` with a
    // pre-filled choice.
    const blank = document.createElement("option");
    blank.value = "";
    blank.textContent = "— pick a value —";
    input.appendChild(blank);
    for (const opt of field.options || []) {
      const o = document.createElement("option");
      o.value = opt.value;
      o.textContent = opt.label;
      input.appendChild(o);
    }
    // Pre-select the saved value when the server echoed it
    // back. The submit handler skips empty values, so a
    // `<select>` left on the blank placeholder does not
    // overwrite the saved row.
    if (typeof field.value === "string" && field.value) {
      input.value = field.value;
      // Drop the blank placeholder once a saved value is
      // pre-selected — keeps the form honest about which
      // value the user is about to save.
      if (input.value === field.value && blank.value === "") {
        blank.remove();
      }
    }
  } else {
    input = document.createElement("input");
    input.name = field.key;
    input.dataset.key = field.key;
    input.type =
      field.kind === "password"
        ? "password"
        : field.kind === "url"
        ? "url"
        : field.kind === "number"
        ? "number"
        : "text";
    if (field.placeholder) input.placeholder = field.placeholder;
    // A filled Password field is never echoed (`value` is always
    // absent for secrets, AGENTS.md §6) — show a "saved" placeholder
    // instead so the user knows leaving it empty keeps the saved
    // token (the PUT handler carries over fields absent from the
    // body, it never wipes them).
    if (field.kind === "password" && field.filled && !field.value) {
      input.placeholder = "••• already saved — leave empty to keep";
    }
    // Pre-fill the field when the server echoed back a saved
    // value. The server only populates `field.value` for
    // non-`password` kinds (URL, username, host, …); secret
    // fields always come back as `undefined` so we never
    // auto-populate a password. Leaving the field empty on edit
    // is also a valid "keep what is already saved" signal —
    // the PUT handler skips empty inputs and the existing
    // ciphertext row stays untouched.
    if (typeof field.value === "string") input.value = field.value;
    input.autocomplete = "off";
    input.spellcheck = false;
    // Numeric fields store as strings on the wire; pin the
    // browser's `step` so a stray decimal does not slip
    // through client-side validation. The server is the
    // source of truth and rejects non-integers with a
    // 400-class error.
    if (field.kind === "number") {
      input.step = "1";
    }
  }
  // The input lives inside a flex group so a service that
  // exposes a probe affordance (CalDAV, email, GitHub)
  // can place a button next to it without changing the
  // markup contract for the other services.
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

// Plan 1791566269874 — IMAP / SMTP probe endpoints. The wire
// layer is a follow-up commit (Phases 3 + 5) so v1 returns a
// 503 with a clear "wire layer not implemented in v1" body;
// the UI still surfaces a "Test connection" button so the
// user gets the affordance today and the same affordance will
// light up the real probe once the wire layer lands.
const PROBE_PATH_EMAIL_IMAP = "/api/integrations/email_imap/probe";
const PROBE_PATH_EMAIL_SMTP = "/api/integrations/email_smtp/probe";
const GITHUB_SERVICE_ID = "github";
const PROBE_PATH_GITHUB = "/api/integrations/github/probe";
const SHORTCUT_SERVICE_ID = "shortcut";
const PROBE_PATH_SHORTCUT = "/api/integrations/shortcut/probe";

// One-line subtitle per service. Sets the user's mental
// model before they read the field labels. Falls back to a
// generic "encrypted at rest" line so future ServiceDefs
// don't render a bare title.
const MODAL_SUBTITLES = {
  caldav:
    "Connect your personal calendar (Nextcloud, Radicale, Fastmail, iCloud, …). Read + add events only — edit/delete are out of scope for v1.",
  x_account:
    "Sign in with X to let the assistant read your home timeline (Abonnements / Pour Vous). Read-only — the agent never posts, replies, or writes on your behalf.",
  email_imap:
    "Connect your IMAP mailbox so the assistant can list / read / search your messages and mark them as read. App-specific password recommended.",
  email_smtp:
    "Connect your SMTP submission server so the assistant can send mail on your behalf. The `from` address is validated against the server and never picked by the LLM.",
  github:
    "Connect your GitHub account with a personal access token so the assistant can read your repos, issues, PRs and notifications. Read-only — the agent never creates or modifies anything on your behalf.",
  shortcut:
    "Connect your Shortcut (app.shortcut.com) workspace so the assistant can read your backlog, stories, epics, teams and iterations, and create new stories in your default team when you ask in chat. Use \"Connect with Shortcut\" (OAuth) when offered; an API token also works as a fallback.",
};

const SHORTCUT_OAUTH_START = "/api/auth/login/shortcut/start";
const SHORTCUT_OAUTH_DISCONNECT = "/api/auth/login/shortcut/disconnect";

// Reads `features.shortcut_oauth` from `GET /api/features` (a fresh
// fetch per editor open is fine — the integrations drawer opens
// rarely).
async function shortcutOauthEnabled() {
  try {
    const resp = await fetch("/api/features", { credentials: "same-origin" });
    if (!resp.ok) return false;
    const body = await resp.json();
    return !!body?.shortcut_oauth;
  } catch {
    return false;
  }
}

// "Connect with Shortcut" / "Disconnect" OAuth buttons in the
// Shortcut editor. The connect button is a plain navigation to the
// `/start` route (the browser follows the 302 to Shortcut's
// authorize page and comes back to our `/callback`). The
// disconnect button POSTs and reloads so the status pill flips
// back. Only rendered when the operator enabled
// `[shortcut_oauth]` (`features.shortcut_oauth === true`).
async function attachShortcutOauthButtons(form, actions, svc) {
  if (!(await shortcutOauthEnabled())) return;
  const fieldValue = (key) => (svc.fields || []).find((f) => f.key === key)?.value || "";
  const oauthOn = fieldValue("auth_mode") === "oauth";
  const row = document.createElement("div");
  row.className = "chat-integration-oauth-row";
  const connect = document.createElement("button");
  connect.type = "button";
  connect.className = "ghost";
  connect.textContent = oauthOn ? "Reconnect with Shortcut" : "Connect with Shortcut";
  connect.addEventListener("click", () => {
    location.href = SHORTCUT_OAUTH_START;
  });
  row.appendChild(connect);
  if (oauthOn) {
    const disconnect = document.createElement("button");
    disconnect.type = "button";
    disconnect.className = "danger";
    disconnect.textContent = "Disconnect";
    disconnect.addEventListener("click", async () => {
      disconnect.disabled = true;
      try {
        const resp = await fetch(SHORTCUT_OAUTH_DISCONNECT, {
          method: "POST",
          credentials: "same-origin",
          headers: { ...(csrfHeaders() || {}) },
        });
        if (!resp.ok && resp.status !== 204) throw new Error(`HTTP ${resp.status}`);
        document.dispatchEvent(
          new CustomEvent("integrationsupdated", { detail: { service: "shortcut" } }),
        );
        location.reload();
      } catch (e) {
        disconnect.disabled = false;
        alert(`Disconnect failed: ${e.message}`);
      }
    });
    row.appendChild(disconnect);
  }
  // Read-only status line for the redirect-managed OAuth fields
  // (access_token / refresh_token / expiry / … are collected by the
  // callback, never typed here).
  const status = document.createElement("span");
  status.className = "chat-integration-oauth-hint";
  if (oauthOn) {
    const expiry = fieldValue("token_expires_at");
    status.textContent = `Connected via OAuth${expiry ? ` — access token expires ${expiry}` : ""}. The API token field below is unused.`;
  } else {
    status.textContent = "OAuth is managed by the redirect flow — no token to paste here.";
  }
  row.appendChild(status);
  form.insertBefore(row, actions);
}

// Surface the OAuth callback redirect (`?shortcut_connected=…` /
// `?shortcut_error=…`) as a one-shot banner, then clean the URL.
function showShortcutOauthResultBanner() {
  const params = new URLSearchParams(location.search);
  const connected = params.get("shortcut_connected");
  const error = params.get("shortcut_error");
  if (!connected && !error) return;
  params.delete("shortcut_connected");
  params.delete("shortcut_error");
  const qs = params.toString();
  history.replaceState(null, "", location.pathname + (qs ? `?${qs}` : ""));
  const anchor =
    document.getElementById(SETTINGS_LIST_ID) || document.getElementById(LIST_ID);
  if (!anchor || !anchor.parentElement) return;
  const banner = document.createElement("li");
  banner.className = "settings-integrations-banner";
  banner.textContent = connected
    ? "Shortcut connected via OAuth."
    : `Shortcut OAuth connection failed (${error || "unknown error"}).`;
  anchor.parentElement.insertBefore(banner, anchor);
}

// "Default group" dropdown: the rendered `default_team` text input
// becomes a hidden input (still submitted by the generic PUT as the
// raw `{"id","name"}` JSON), and a custom button + popover list the
// workspace groups with their icon (`display_icon.url`, projected
// as `icon_url`). Data comes from `GET /api/integrations/shortcut/teams`
// (vault token) or, before the token is saved, from
// `POST /api/integrations/shortcut/probe-teams` with the token typed
// in the form. A native <select> is not used because it cannot render
// images inside its options.
function attachShortcutGroupSelect(form) {
  const input = form.querySelector('[data-key="default_team"]');
  if (!input || input.tagName !== "INPUT") return;
  input.type = "hidden";
  const saved = input.value;

  const wrap = input.closest(".chat-integration-field") || input.parentElement;
  wrap.classList.add("sc-group-select-wrap");

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "sc-group-select-btn ghost";
  btn.setAttribute("aria-haspopup", "listbox");
  btn.setAttribute("aria-expanded", "false");
  const list = document.createElement("div");
  list.className = "sc-group-select-list";
  list.setAttribute("role", "listbox");
  list.hidden = true;
  wrap.appendChild(btn);
  wrap.appendChild(list);

  const iconEl = (g) => {
    if (g.icon_url) {
      const img = document.createElement("img");
      img.className = "sc-group-icon";
      img.src = g.icon_url;
      img.alt = "";
      img.loading = "lazy";
      return img;
    }
    const dot = document.createElement("span");
    dot.className = "sc-group-icon sc-group-icon-fallback";
    dot.textContent = (g.name || "?").trim().charAt(0).toUpperCase() || "?";
    return dot;
  };

  const renderBtn = (g) => {
    btn.replaceChildren();
    if (g) {
      btn.appendChild(iconEl(g));
      const label = document.createElement("span");
      label.textContent = g.name || String(g.id);
      btn.appendChild(label);
    } else {
      const label = document.createElement("span");
      label.textContent = "— choose a group —";
      btn.appendChild(label);
    }
  };

  let groups = [];
  const renderList = () => {
    list.replaceChildren();
    if (!groups.length) {
      const empty = document.createElement("div");
      empty.className = "sc-group-select-empty";
      empty.textContent = "No groups loaded — check the API token above.";
      list.appendChild(empty);
      return;
    }
    for (const g of groups) {
      if (!g || g.id == null) continue;
      const item = document.createElement("button");
      item.type = "button";
      item.className = "sc-group-select-item";
      item.setAttribute("role", "option");
      item.appendChild(iconEl(g));
      const label = document.createElement("span");
      label.className = "sc-group-select-name";
      label.textContent = g.name || String(g.id);
      item.appendChild(label);
      if (g.mention_name) {
        const mention = document.createElement("span");
        mention.className = "sc-group-select-mention";
        mention.textContent = `@${g.mention_name}`;
        item.appendChild(mention);
      }
      item.addEventListener("click", () => {
        input.value = JSON.stringify({ id: g.id, name: g.name || "" });
        renderBtn(g);
        close();
      });
      list.appendChild(item);
    }
  };

  const open = () => {
    list.hidden = false;
    btn.setAttribute("aria-expanded", "true");
  };
  const close = () => {
    list.hidden = true;
    btn.setAttribute("aria-expanded", "false");
  };
  btn.addEventListener("click", () => {
    if (list.hidden) {
      renderList();
      open();
    } else {
      close();
    }
  });
  document.addEventListener("click", (e) => {
    if (!list.hidden && !wrap.contains(e.target)) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !list.hidden) close();
  });

  const getTypedToken = () =>
    form.querySelector('[data-key="api_token"]')?.value.trim() || "";

  const load = async () => {
    renderBtn(null);
    btn.disabled = true;
    try {
      let loaded = null;
      try {
        const resp = await fetch("/api/integrations/shortcut/teams", {
          credentials: "same-origin",
        });
        if (resp.ok) {
          const body = await resp.json();
          loaded = (body && body.data && body.data.teams) || [];
        }
      } catch {
        // fall through to the typed-token probe
      }
      if (!loaded || !loaded.length) {
        const token = getTypedToken();
        if (token) {
          const resp = await fetch("/api/integrations/shortcut/probe-teams", {
            method: "POST",
            credentials: "same-origin",
            headers: {
              "content-type": "application/json",
              ...(csrfHeaders() || {}),
            },
            body: JSON.stringify({ api_token: token }),
          });
          if (resp.ok) {
            const body = await resp.json();
            loaded = (body && body.teams) || [];
          }
        }
      }
      groups = loaded || [];
      // Re-select the saved group on the button label.
      if (saved) {
        try {
          const parsed = JSON.parse(saved);
          const match = groups.find((g) => g && g.id === parsed.id);
          renderBtn(match || parsed);
        } catch {
          renderBtn(null);
        }
      } else {
        renderBtn(null);
      }
    } finally {
      btn.disabled = false;
    }
  };
  load();
}



// "Default workflow" dropdown: the rendered `default_workflow` text
// input becomes a hidden input (still submitted by the generic PUT as
// the raw `{"id","name"}` JSON), and a custom button + popover lists the
// workspace workflows. Data comes from
// `GET /api/integrations/shortcut/workflows` (vault token, `data.workflows`)
// or, before the token is saved, from
// `POST /api/integrations/shortcut/probe-workflows` (`body.workflows`).
// The state dropdown below is scoped to the workflow chosen here.
function attachShortcutWorkflowSelect(form) {
  const input = form.querySelector('[data-key="default_workflow"]');
  if (!input || input.tagName !== "INPUT") return;
  input.type = "hidden";
  const saved = input.value;

  const wrap = input.closest(".chat-integration-field") || input.parentElement;
  wrap.classList.add("sc-group-select-wrap");

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "sc-group-select-btn ghost";
  btn.setAttribute("aria-haspopup", "listbox");
  btn.setAttribute("aria-expanded", "false");
  const list = document.createElement("div");
  list.className = "sc-group-select-list";
  list.setAttribute("role", "listbox");
  list.hidden = true;
  wrap.appendChild(btn);
  wrap.appendChild(list);

  const dotEl = (w) => {
    const dot = document.createElement("span");
    dot.className = "sc-group-icon sc-group-icon-fallback";
    dot.textContent = (w.name || "?").trim().charAt(0).toUpperCase() || "?";
    return dot;
  };

  const renderBtn = (w) => {
    btn.replaceChildren();
    if (w) {
      btn.appendChild(dotEl(w));
      const label = document.createElement("span");
      label.textContent = w.name || String(w.id);
      btn.appendChild(label);
    } else {
      const label = document.createElement("span");
      label.textContent = "— team default workflow —";
      btn.appendChild(label);
    }
  };

  let workflows = [];
  const renderList = () => {
    list.replaceChildren();
    if (!workflows.length) {
      const empty = document.createElement("div");
      empty.className = "sc-group-select-empty";
      empty.textContent = "No workflows loaded — check the API token above.";
      list.appendChild(empty);
      return;
    }
    for (const w of workflows) {
      if (!w || w.id == null) continue;
      const item = document.createElement("button");
      item.type = "button";
      item.className = "sc-group-select-item";
      item.setAttribute("role", "option");
      item.appendChild(dotEl(w));
      const label = document.createElement("span");
      label.className = "sc-group-select-name";
      label.textContent = w.name || String(w.id);
      item.appendChild(label);
      if (w.state_count != null) {
        const count = document.createElement("span");
        count.className = "sc-group-select-mention";
        count.textContent = `${w.state_count} states`;
        item.appendChild(count);
      }
      item.addEventListener("click", () => {
        input.value = JSON.stringify({ id: w.id, name: w.name || "" });
        renderBtn(w);
        close();
      });
      list.appendChild(item);
    }
  };

  const open = () => {
    list.hidden = false;
    btn.setAttribute("aria-expanded", "true");
  };
  const close = () => {
    list.hidden = true;
    btn.setAttribute("aria-expanded", "false");
  };
  btn.addEventListener("click", () => {
    if (list.hidden) {
      renderList();
      open();
    } else {
      close();
    }
  });
  document.addEventListener("click", (e) => {
    if (!list.hidden && !wrap.contains(e.target)) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !list.hidden) close();
  });

  const getTypedToken = () =>
    form.querySelector('[data-key="api_token"]')?.value.trim() || "";

  const load = async () => {
    renderBtn(null);
    btn.disabled = true;
    try {
      let loaded = null;
      try {
        const resp = await fetch("/api/integrations/shortcut/workflows", {
          credentials: "same-origin",
        });
        if (resp.ok) {
          const body = await resp.json();
          loaded = (body && body.data && body.data.workflows) || [];
        }
      } catch {
        // fall through to the typed-token probe
      }
      if (!loaded || !loaded.length) {
        const token = getTypedToken();
        if (token) {
          const resp = await fetch("/api/integrations/shortcut/probe-workflows", {
            method: "POST",
            credentials: "same-origin",
            headers: {
              "content-type": "application/json",
              ...(csrfHeaders() || {}),
            },
            body: JSON.stringify({ api_token: token }),
          });
          if (resp.ok) {
            const body = await resp.json();
            loaded = (body && body.workflows) || [];
          }
        }
      }
      workflows = loaded || [];
      if (saved) {
        try {
          const parsed = JSON.parse(saved);
          const match = workflows.find((w) => w && w.id === parsed.id);
          renderBtn(match || parsed);
        } catch {
          renderBtn(null);
        }
      } else {
        renderBtn(null);
      }
    } finally {
      btn.disabled = false;
    }
  };
  load();
}


// "Default workflow state" dropdown: the rendered
// `default_workflow_state` text input becomes a hidden input (still
// submitted by the generic PUT as the raw `{"id","name","category"}`
// JSON), and a custom button + popover lists the workspace workflow
// states. Data comes from `GET /api/integrations/shortcut/workflows`
// (vault token) or, before the token is saved, from
// `POST /api/integrations/shortcut/probe-workflows` with the token
// typed in the form. Reuses the `sc-group-select-*` styles.
function attachShortcutWorkflowStateSelect(form) {
  const input = form.querySelector('[data-key="default_workflow_state"]');
  if (!input || input.tagName !== "INPUT") return;
  input.type = "hidden";
  const saved = input.value;

  const wrap = input.closest(".chat-integration-field") || input.parentElement;
  wrap.classList.add("sc-group-select-wrap");

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "sc-group-select-btn ghost";
  btn.setAttribute("aria-haspopup", "listbox");
  btn.setAttribute("aria-expanded", "false");
  const list = document.createElement("div");
  list.className = "sc-group-select-list";
  list.setAttribute("role", "listbox");
  list.hidden = true;
  wrap.appendChild(btn);
  wrap.appendChild(list);

  const dotEl = (s) => {
    const dot = document.createElement("span");
    dot.className = "sc-group-icon sc-group-icon-fallback";
    dot.textContent = (s.category || "?").trim().charAt(0).toUpperCase() || "?";
    dot.title = s.category || "";
    return dot;
  };

  const labelOf = (s) => {
    const wf = s.workflow_name ? ` (${s.workflow_name})` : "";
    return `${s.name || String(s.id)}${wf}`;
  };

  const renderBtn = (s) => {
    btn.replaceChildren();
    if (s) {
      btn.appendChild(dotEl(s));
      const label = document.createElement("span");
      label.textContent = labelOf(s);
      btn.appendChild(label);
    } else {
      const label = document.createElement("span");
      label.textContent = "— team default initial state —";
      btn.appendChild(label);
    }
  };

  let states = [];
  // Scope the list to the workflow picked in the `default_workflow`
  // dropdown above (each flattened state carries `workflow_id`).
  const selectedWorkflowId = () => {
    const wfInput = form.querySelector('[data-key="default_workflow"]');
    if (!wfInput || !wfInput.value) return null;
    try {
      const parsed = JSON.parse(wfInput.value);
      return parsed && parsed.id != null ? parsed.id : null;
    } catch {
      return null;
    }
  };
  const renderList = () => {
    list.replaceChildren();
    const wfId = selectedWorkflowId();
    const visible =
      wfId == null ? states : states.filter((s) => s && s.workflow_id === wfId);
    if (!visible.length) {
      const empty = document.createElement("div");
      empty.className = "sc-group-select-empty";
      empty.textContent = wfId == null
        ? "No workflow states loaded — check the API token above."
        : "No states in the selected workflow.";
      list.appendChild(empty);
      return;
    }
    for (const s of visible) {
      if (!s || s.id == null) continue;
      const item = document.createElement("button");
      item.type = "button";
      item.className = "sc-group-select-item";
      item.setAttribute("role", "option");
      item.appendChild(dotEl(s));
      const label = document.createElement("span");
      label.className = "sc-group-select-name";
      label.textContent = labelOf(s);
      item.appendChild(label);
      if (s.category) {
        const cat = document.createElement("span");
        cat.className = "sc-group-select-mention";
        cat.textContent = s.category;
        item.appendChild(cat);
      }
      item.addEventListener("click", () => {
        input.value = JSON.stringify({
          id: s.id,
          name: s.name || "",
          category: s.category || "",
        });
        renderBtn(s);
        close();
      });
      list.appendChild(item);
    }
  };

  const open = () => {
    list.hidden = false;
    btn.setAttribute("aria-expanded", "true");
  };
  const close = () => {
    list.hidden = true;
    btn.setAttribute("aria-expanded", "false");
  };
  btn.addEventListener("click", () => {
    if (list.hidden) {
      renderList();
      open();
    } else {
      close();
    }
  });
  document.addEventListener("click", (e) => {
    if (!list.hidden && !wrap.contains(e.target)) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !list.hidden) close();
  });

  const getTypedToken = () =>
    form.querySelector('[data-key="api_token"]')?.value.trim() || "";

  const load = async () => {
    renderBtn(null);
    btn.disabled = true;
    try {
      let loaded = null;
      try {
        const resp = await fetch("/api/integrations/shortcut/workflows", {
          credentials: "same-origin",
        });
        if (resp.ok) {
          const body = await resp.json();
          loaded = (body && body.data && body.data.states) || [];
        }
      } catch {
        // fall through to the typed-token probe
      }
      if (!loaded || !loaded.length) {
        const token = getTypedToken();
        if (token) {
          const resp = await fetch("/api/integrations/shortcut/probe-workflows", {
            method: "POST",
            credentials: "same-origin",
            headers: {
              "content-type": "application/json",
              ...(csrfHeaders() || {}),
            },
            body: JSON.stringify({ api_token: token }),
          });
          if (resp.ok) {
            const body = await resp.json();
            loaded = (body && body.states) || [];
          }
        }
      }
      states = loaded || [];
      // Re-select the saved state on the button label.
      if (saved) {
        try {
          const parsed = JSON.parse(saved);
          const match = states.find((s) => s && s.id === parsed.id);
          renderBtn(match || parsed);
        } catch {
          renderBtn(null);
        }
      } else {
        renderBtn(null);
      }
    } finally {
      btn.disabled = false;
    }
  };
  load();
}


function attachShortcutTemplatesEditor(form) {
  const field = form.querySelector('[data-key="story_templates"]');
  if (!field || field.tagName !== "INPUT") return;
  const ta = document.createElement("textarea");
  ta.name = field.name;
  ta.dataset.key = field.dataset.key;
  ta.value = field.value || "";
  ta.rows = 6;
  ta.spellcheck = false;
  ta.autocomplete = "off";
  ta.className = "chat-integration-templates";
  field.replaceWith(ta);
}

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

// ---------------------------------------------------------------------------
// IMAP / SMTP probe (plan 1791566269874 §1.2 + §2.2).
//
// Mirrors `attachCalDavDiscover` so the email connectors get a
// "Test connection" affordance next to the host field. The
// probe is read-only — it does NOT save anything to the
// vault. The user must still hit Save to persist the values;
// the test only validates the credentials against the
// operator's `[email].allowlist` + the upstream host / port /
// TLS posture so a misconfigured row surfaces a clear error
// BEFORE the user has committed to it.
//
// v1 status: the wire layer is a follow-up commit, so the
// endpoint returns 503 with a clear "wire layer not
// implemented in v1" body. The button still surfaces the
// affordance so the UI is stable across the wire-layer
// follow-up; the same code path lights up automatically
// when the real probe lands.
// ---------------------------------------------------------------------------
async function probeEmail(path, payload) {
  const resp = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      ...(csrfHeaders() || {}),
    },
    body: JSON.stringify(payload),
  });
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
  return await resp.json();
}

// Plan 1791566269874 — IMAP / SMTP probe affordance. The
// `Test connection` button sits in the actions row (next
// to Cancel + Save), the status / result panel sits in a
// card above the actions row. The `tls_mode` field is a
// `<select>`; when the user changes the TLS mode, the
// port field is auto-filled with the canonical default
// for that mode + protocol (993 / 143 for IMAP, 465 / 587
// for SMTP) so a user who is not familiar with the port
// matrix does not have to look it up. The auto-fill only
// kicks in when the port field is empty OR matches one of
// the other modes' defaults — a custom port is preserved.

/// Default ports per (protocol, tls_mode) tuple.
const EMAIL_DEFAULT_PORTS = {
  email_imap: { implicit: 993, starttls: 143, none: 143 },
  email_smtp: { implicit: 465, starttls: 587, none: 587 },
};

/// Set of canonical port values per protocol. Used by
/// the TLS→port auto-fill to decide whether a stale
/// port value is a leftover from another mode (and
/// therefore safe to overwrite) or a custom value the
/// user typed (and therefore preserved).
const EMAIL_KNOWN_PORTS = Object.fromEntries(
  Object.entries(EMAIL_DEFAULT_PORTS).map(([proto, by_mode]) => [
    proto,
    new Set(Object.values(by_mode)),
  ]),
);

/// Wire the TLS→port auto-fill. The `tls_mode` field
/// is a `<select>` with `data-key="tls_mode"`; the
/// `port` field is a number input with `data-key="port"`.
/// On every `change` event we look at the current
/// port: if it is empty or matches one of the *other*
/// modes' defaults, we replace it with the new mode's
/// canonical default; otherwise we leave the custom
/// value alone.
function attachEmailPortAutofill(form, serviceId) {
  const tls = form.querySelector('[data-key="tls_mode"]');
  const port = form.querySelector('[data-key="port"]');
  if (!tls || !port) return;
  const known = EMAIL_KNOWN_PORTS[serviceId];
  const defaults = EMAIL_DEFAULT_PORTS[serviceId];
  if (!known || !defaults) return;
  tls.addEventListener("change", () => {
    const current = parseInt(port.value, 10);
    const mode = tls.value;
    const def = defaults[mode];
    if (def === undefined) return;
    if (!Number.isFinite(current) || known.has(current)) {
      port.value = String(def);
    }
  });
}

function attachEmailTestButton(form, modal, actions, serviceId) {
  // The probe needs host / port / username / password /
  // tls_mode (and `from_address` for SMTP). Pull them
  // out of the form by `data-key` so a future
  // ServiceDef that re-orders the fields does not have
  // to re-thread this helper.
  const field = (key) => form.querySelector(`[data-key="${key}"]`);
  const host = field("host");
  const port = field("port");
  const username = field("username");
  const password = field("password");
  const tlsMode = field("tls_mode");
  const fromAddress = field("from_address");
  if (!host || !port || !username || !password || !tlsMode) return;

  // The `actions` row is passed in directly so the
  // function fails fast if the caller forgot to build
  // it first (the previous version looked it up via
  // `modal.querySelector` and silently bailed when the
  // row was not yet in the DOM — that's how the v1
  // button disappeared from the rendered modal).
  if (!actions) {
    console.warn("attachEmailTestButton: missing actions element");
    return;
  }

  // Insert the "Test connection" button right BEFORE
  // the Cancel button. The Cancel button is tagged
  // with `data-action="cancel"` so the lookup is
  // unambiguous — using `querySelector("button.ghost")`
  // would also match the test button once it's in the
  // DOM, creating a circular `insertBefore` no-op.
  const cancelBtn = actions.querySelector('[data-action="cancel"]');
  const testBtn = document.createElement("button");
  testBtn.type = "button";
  testBtn.className = "ghost";
  testBtn.textContent = "Test connection";
  if (cancelBtn) {
    actions.insertBefore(testBtn, cancelBtn);
  } else {
    actions.appendChild(testBtn);
  }

  // Status + result container live ABOVE the actions row,
  // not next to the host field. The full-width layout
  // means a multi-line error message can wrap without
  // breaking the form's visual rhythm, and the user reads
  // the status AFTER scrolling past the form — the same
  // "footer-style" affordance every other settings modal
  // uses.
  // Status + result container live ABOVE the actions row,
  // not next to the host field. The full-width layout
  // means a multi-line error message can wrap without
  // breaking the form's visual rhythm, and the user reads
  // the status AFTER scrolling past the form — the same
  // "footer-style" affordance every other settings modal
  // uses.
  //
  // The panel is created LAZILY on the first probe —
  // creating it up-front would leave an empty bordered
  // "results" container visible before the user clicks
  // the button (the empty-bar bug the v1.1 screenshot
  // caught).
  let probePanel = null;
  let status = null;
  let results = null;
  const ensureProbePanel = () => {
    if (probePanel) return { status, results };
    probePanel = document.createElement("div");
    probePanel.className = "chat-integration-probe-panel";
    probePanel.setAttribute("role", "group");
    probePanel.setAttribute("aria-label", "Test connection status");
    status = document.createElement("p");
    status.className = "chat-integration-discover-status";
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");
    results = document.createElement("div");
    results.className = "chat-integration-probe-results";
    probePanel.append(status, results);
    actions.parentNode.insertBefore(probePanel, actions);
    return { status, results };
  };

  const path =
    serviceId === "email_smtp" ? PROBE_PATH_EMAIL_SMTP : PROBE_PATH_EMAIL_IMAP;
  const label = serviceId === "email_smtp" ? "SMTP" : "IMAP";

  testBtn.addEventListener("click", async () => {
    // Disable the button while the probe is in flight so a
    // double-click cannot fire two concurrent requests. The
    // user can re-enable it by editing any field.
    testBtn.disabled = true;
    const panel = ensureProbePanel();
    panel.status.textContent = `Testing ${label} connection…`;
    panel.status.dataset.state = "pending";
    panel.results.replaceChildren();
    // Build the request body from the form's current
    // values. The server-side validator checks the host /
    // port / allowlist / TLS posture and replies with a
    // structured error; the catch below surfaces the
    // server's `error` field verbatim so a misconfigured
    // `[email].allowlist` or a bad host surfaces a clear
    // message rather than a generic "probe failed".
    const portNum = parseInt(port.value, 10);
    if (!host.value.trim() || !Number.isFinite(portNum) || portNum <= 0) {
      testBtn.disabled = false;
      panel.status.textContent = "Please fill in host and port first.";
      panel.status.dataset.state = "error";
      return;
    }
    const payload = {
      host: host.value.trim(),
      port: portNum,
      username: username.value,
      password: password.value,
      tls_mode: tlsMode.value,
    };
    if (serviceId === "email_smtp") {
      if (!fromAddress || !fromAddress.value.trim()) {
        testBtn.disabled = false;
        panel.status.textContent = "Please fill in the From address first.";
        panel.status.dataset.state = "error";
        return;
      }
      payload.from_address = fromAddress.value.trim();
    }
    try {
      const body = await probeEmail(path, payload);
      panel.status.dataset.state = "ok";
      if (serviceId === "email_imap") {
        const mailboxes = body?.data?.mailboxes || body?.mailboxes || [];
        const count = Array.isArray(mailboxes) ? mailboxes.length : 0;
        panel.status.textContent =
          `IMAP probe succeeded — found ${count} mailbox${count === 1 ? "" : "es"} on the server.`;
        renderProbeMailboxList(panel.results, mailboxes);
      } else {
        panel.status.textContent = "SMTP probe succeeded — the relay accepted the From address.";
        panel.results.replaceChildren();
      }
    } catch (e) {
      console.error(`${label} probe failed:`, e);
      panel.status.textContent = `Probe failed: ${e.message}`;
      panel.status.dataset.state = "error";
      panel.results.replaceChildren();
    } finally {
      testBtn.disabled = false;
    }
  });
}

/// Render the list of mailboxes the IMAP probe discovered.
/// Each row shows the mailbox name + an `EXISTS` count when
/// the server returned one. Empty / failure states are
/// handled by the caller (the status line already says
/// "no mailboxes" / "probe failed").
function renderProbeMailboxList(container, mailboxes) {
  container.replaceChildren();
  if (!Array.isArray(mailboxes) || mailboxes.length === 0) {
    const empty = document.createElement("p");
    empty.className = "chat-integration-probe-empty";
    empty.textContent = "The server returned no mailboxes.";
    container.appendChild(empty);
    return;
  }
  const list = document.createElement("ul");
  list.className = "chat-integration-probe-list";
  for (const mb of mailboxes) {
    const li = document.createElement("li");
    li.className = "chat-integration-probe-item";
    const name = document.createElement("span");
    name.className = "chat-integration-probe-name";
    name.textContent = mb.name || "(unnamed)";
    li.appendChild(name);
    if (typeof mb.total === "number" && mb.total >= 0) {
      const count = document.createElement("code");
      count.className = "chat-integration-probe-href";
      count.textContent = `${mb.total} message${mb.total === 1 ? "" : "s"}`;
      li.appendChild(count);
    }
    list.appendChild(li);
  }
  container.appendChild(list);
}

async function probeGithub(personalAccessToken) {
  const resp = await fetch(PROBE_PATH_GITHUB, {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      ...(csrfHeaders() || {}),
    },
    body: JSON.stringify({ personal_access_token: personalAccessToken }),
  });
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
  return await resp.json();
}

// Plan 1795686864347 — the GitHub `dashboard_repos` field is managed
// through the dedicated repo picker (github-repo-picker.js): it lists
// the orgs the saved PAT belongs to and lets the user tick repos.
// The field itself stays a hidden input holding the JSON array so the
// generic form submit still picks it up unchanged; the button opens
// the picker, which saves through its own PUT and syncs this input.
function attachGithubReposPickerButton(form) {
  const field = form.querySelector('[data-key="dashboard_repos"]');
  if (!field) return;
  field.type = "hidden";
  const wrap = field.closest(".chat-integration-field");
  const row = document.createElement("div");
  row.className = "gh-repo-picker-row";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "ghost";
  btn.textContent = "Choose repositories…";
  btn.addEventListener("click", () => {
    window.__nagentGithubRepoPicker?.open();
  });
  row.appendChild(btn);
  if (wrap && wrap.parentNode) {
    wrap.parentNode.insertBefore(row, wrap.nextSibling);
  } else {
    form.appendChild(row);
  }
}

// Plan 1795793328177 — the GitHub `accounts` vault field is a
// multi-token JSON blob managed ONLY through the dedicated endpoints
// below (GET/POST /api/integrations/github/accounts, DELETE
// …/accounts/:name). The generic form submit never carries the blob
// (`accounts` is in `managed_fields`), and tokens are write-only in
// this UI: never pre-filled, never echoed back (AGENTS.md §6).
const GITHUB_ACCOUNTS_PATH = "/api/integrations/github/accounts";

async function githubAccountsApi(path, options) {
  const resp = await fetch(path, {
    credentials: "same-origin",
    ...options,
    headers: {
      ...(options && options.body ? { "content-type": "application/json" } : {}),
      ...(csrfHeaders() || {}),
      ...((options && options.headers) || {}),
    },
  });
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
  if (resp.status === 204) return null;
  return await resp.json();
}

async function fetchGithubAccounts() {
  return await githubAccountsApi(GITHUB_ACCOUNTS_PATH, { method: "GET" });
}

// Renders the "Connected accounts" section of the GitHub editor:
// the account list (name, @login, scopes, default badge) with
// add / edit-token / remove per account, plus the "Default account"
// selector. The selector replaces the plain `default_account` text
// input inside the form so the generic submit persists it through
// the normal PUT; everything token-shaped goes through the dedicated
// endpoints above.
function attachGithubAccountsSection(form, actions, _svc) {
  // Swap the rendered `default_account` text input for a <select>
  // fed by the accounts endpoint. The submit handler walks every
  // form control with a data-key, so the select keeps that key and
  // rides the generic PUT.
  const defaultInput = form.querySelector('[data-key="default_account"]');
  const defaultSelect = document.createElement("select");
  defaultSelect.name = "default_account";
  defaultSelect.dataset.key = "default_account";
  defaultSelect.autocomplete = "off";
  if (defaultInput && defaultInput.closest(".chat-integration-field")) {
    defaultInput.replaceWith(defaultSelect);
    if (!defaultInput.value) {
      // Unset: pre-select "default" once options arrive.
    } else {
      defaultSelect.dataset.saved = defaultInput.value;
    }
  } else {
    // Field missing from the ServiceDef (older server): still render
    // the section, just without the selector swap.
    form.appendChild(defaultSelect);
  }

  const section = document.createElement("div");
  section.className = "chat-integration-field gh-accounts-section";
  const heading = document.createElement("span");
  heading.textContent = "Connected accounts (extra tokens)";
  section.appendChild(heading);
  const hint = document.createElement("small");
  hint.textContent =
    "Extra GitHub tokens (e.g. client orgs) live here as named accounts. " +
    "Tokens are write-only and never displayed again.";
  section.appendChild(hint);
  const list = document.createElement("ul");
  list.className = "gh-accounts-list";
  section.appendChild(list);

  // Add-account row: name + write-only token input.
  const addRow = document.createElement("div");
  addRow.className = "gh-accounts-add";
  const nameInput = document.createElement("input");
  nameInput.type = "text";
  nameInput.placeholder = "account name (e.g. client-a)";
  nameInput.autocomplete = "off";
  nameInput.pattern = "[a-z0-9][a-z0-9-_]{0,31}";
  nameInput.title = "Lowercase letters, digits, dashes, underscores; max 32 chars; \"default\" is reserved";
  const tokenInput = document.createElement("input");
  tokenInput.type = "password";
  tokenInput.placeholder = "ghp_… personal access token";
  tokenInput.autocomplete = "new-password";
  const addBtn = document.createElement("button");
  addBtn.type = "button";
  addBtn.className = "ghost";
  addBtn.textContent = "Add account";
  addRow.appendChild(nameInput);
  addRow.appendChild(tokenInput);
  addRow.appendChild(addBtn);
  section.appendChild(addRow);

  const status = document.createElement("p");
  status.className = "prs-status";
  status.setAttribute("role", "status");
  section.appendChild(status);

  function setStatus(message, state) {
    status.textContent = message || "";
    status.dataset.state = state || "";
  }

  function renderDefaultSelect(data) {
    const accounts = (data && data.accounts) || [];
    const saved = defaultSelect.dataset.saved || (data && data.default_account) || "default";
    defaultSelect.replaceChildren();
    for (const acc of accounts) {
      const opt = document.createElement("option");
      opt.value = acc.name;
      opt.textContent = acc.name === "default" ? "default (personal token)" : acc.name;
      defaultSelect.appendChild(opt);
    }
    defaultSelect.value = accounts.some((a) => a.name === saved) ? saved : "default";
    delete defaultSelect.dataset.saved;
  }

  async function renderAccounts() {
    try {
      const data = await fetchGithubAccounts();
      renderDefaultSelect(data);
      const accounts = (data && data.accounts) || [];
      list.replaceChildren();
      for (const acc of accounts) {
        const li = document.createElement("li");
        li.className = "gh-accounts-row";
        const top = document.createElement("div");
        top.className = "gh-accounts-row-top";
        const label = document.createElement("span");
        label.className = "gh-accounts-label";
        const nameStrong = document.createElement("strong");
        nameStrong.textContent = acc.name;
        label.appendChild(nameStrong);
        if (acc.username) {
          const at = document.createElement("span");
          at.textContent = ` @${acc.username}`;
          label.appendChild(at);
        }
        if (acc.scopes) {
          const scopes = document.createElement("code");
          scopes.textContent = ` ${acc.scopes}`;
          label.appendChild(scopes);
        }
        if (acc.is_default) {
          const badge = document.createElement("span");
          badge.className = "prs-state";
          badge.dataset.state = "default";
          badge.textContent = "default";
          label.appendChild(badge);
        }
        top.appendChild(label);
        li.appendChild(top);

        const controls = document.createElement("div");
        controls.className = "gh-accounts-row-controls";
        if (acc.name === "default") {
          // The "default" account IS the personal access token
          // field above; the accounts endpoint refuses to
          // overwrite it, so no token input here.
          const note = document.createElement("span");
          note.className = "gh-accounts-note";
          note.textContent = "Token = the personal access token field above.";
          controls.appendChild(note);
        } else {
          // Edit = replace the token for this account (write-only).
          const editToken = document.createElement("input");
          editToken.type = "password";
          editToken.placeholder = "new token (write-only)";
          editToken.autocomplete = "new-password";
          editToken.className = "gh-accounts-token";
          const saveBtn = document.createElement("button");
          saveBtn.type = "button";
          saveBtn.className = "ghost";
          saveBtn.textContent = "Update token";
          saveBtn.addEventListener("click", async () => {
            if (!editToken.value) {
              setStatus("Enter a new token first.", "error");
              return;
            }
            saveBtn.disabled = true;
            try {
              await githubAccountsApi(GITHUB_ACCOUNTS_PATH, {
                method: "POST",
                body: JSON.stringify({ name: acc.name, token: editToken.value }),
              });
              editToken.value = "";
              setStatus(`Updated token for \`${acc.name}\`.`, "ok");
              await renderAccounts();
            } catch (e) {
              setStatus(e.message, "error");
            } finally {
              saveBtn.disabled = false;
            }
          });
          controls.appendChild(editToken);
          controls.appendChild(saveBtn);

          const removeBtn = document.createElement("button");
          removeBtn.type = "button";
          removeBtn.className = "danger";
          removeBtn.textContent = "Remove";
          removeBtn.addEventListener("click", async () => {
            if (!confirm(`Remove the GitHub account \`${acc.name}\`?`)) return;
            removeBtn.disabled = true;
            try {
              await githubAccountsApi(
                `${GITHUB_ACCOUNTS_PATH}/${encodeURIComponent(acc.name)}`,
                { method: "DELETE" },
              );
              setStatus(`Removed \`${acc.name}\`.`, "ok");
              await renderAccounts();
            } catch (e) {
              setStatus(e.message, "error");
              removeBtn.disabled = false;
            }
          });
          controls.appendChild(removeBtn);
        }
        li.appendChild(controls);
        list.appendChild(li);
      }
      if (!accounts.length) {
        const li = document.createElement("li");
        li.textContent = "No accounts yet.";
        list.appendChild(li);
      }
    } catch (e) {
      // The accounts endpoint is only mounted with the github-agent
      // feature; on older servers the section just shows the error.
      setStatus(e.message, "error");
    }
  }

  addBtn.addEventListener("click", async () => {
    const name = nameInput.value.trim();
    const token = tokenInput.value;
    if (!name || !token) {
      setStatus("Both an account name and a token are required.", "error");
      return;
    }
    addBtn.disabled = true;
    try {
      await githubAccountsApi(GITHUB_ACCOUNTS_PATH, {
        method: "POST",
        body: JSON.stringify({ name, token }),
      });
      nameInput.value = "";
      tokenInput.value = "";
      setStatus(`Added \`${name}\`.`, "ok");
      await renderAccounts();
    } catch (e) {
      setStatus(e.message, "error");
    } finally {
      addBtn.disabled = false;
    }
  });

  form.insertBefore(section, actions);
  renderAccounts();
}

async function probeShortcut(apiToken, username, accessToken) {
  const resp = await fetch(PROBE_PATH_SHORTCUT, {
    method: "POST",
    credentials: "same-origin",
    headers: {
      "content-type": "application/json",
      ...(csrfHeaders() || {}),
    },
    body: JSON.stringify(
      accessToken
        ? { access_token: accessToken, username: username || undefined }
        : { api_token: apiToken, username: username || undefined },
    ),
  });
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
  return await resp.json();
}

// Wires the "Test connection" button for the Shortcut editor and
// returns a `showStatus(text, state)` handle on the probe panel so
// the caller can also display the identity loaded from the vault
// (see `loadShortcutIdentity` below).
function attachShortcutTestButton(form, actions) {
  const tokenField = form.querySelector('[data-key="api_token"]');
  const oauthTokenField = form.querySelector('[data-key="access_token"]');
  const usernameField = form.querySelector('[data-key="username"]');
  if (!tokenField || !usernameField || !actions) return null;

  const cancelBtn = actions.querySelector('[data-action="cancel"]');
  const testBtn = document.createElement("button");
  testBtn.type = "button";
  testBtn.className = "ghost";
  testBtn.textContent = "Test connection";
  if (cancelBtn) {
    actions.insertBefore(testBtn, cancelBtn);
  } else {
    actions.appendChild(testBtn);
  }

  let probePanel = null;
  let status = null;
  const ensureProbePanel = () => {
    if (probePanel) return status;
    probePanel = document.createElement("div");
    probePanel.className = "chat-integration-probe-panel";
    probePanel.setAttribute("role", "group");
    probePanel.setAttribute("aria-label", "Test connection status");
    status = document.createElement("p");
    status.className = "chat-integration-discover-status";
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");
    probePanel.appendChild(status);
    actions.parentNode.insertBefore(probePanel, actions);
    return status;
  };

  testBtn.addEventListener("click", async () => {
    testBtn.disabled = true;
    const statusEl = ensureProbePanel();
    statusEl.textContent = "Testing Shortcut connection…";
    statusEl.dataset.state = "pending";
    const token = tokenField.value.trim();
    const oauthToken = oauthTokenField?.value.trim() || "";
    if (!token && !oauthToken) {
      testBtn.disabled = false;
      statusEl.textContent = "Please fill in the API token (or OAuth access token) first.";
      statusEl.dataset.state = "error";
      return;
    }
    try {
      const body = await probeShortcut(token, usernameField.value.trim(), oauthToken || null);
      statusEl.dataset.state = "ok";
      const name = body.name ? ` — ${body.name}` : "";
      statusEl.textContent = `Connected as @${body.mention_name}${name}.`;
      if (!usernameField.value) {
        usernameField.value = body.mention_name;
        usernameField.dispatchEvent(new Event("input", { bubbles: true }));
      }
    } catch (e) {
      console.error("shortcut probe failed:", e);
      statusEl.textContent = `Probe failed: ${e.message}`;
      statusEl.dataset.state = "error";
    } finally {
      testBtn.disabled = false;
    }
  });

  return {
    showStatus(text, state) {
      const el = ensureProbePanel();
      el.textContent = text;
      el.dataset.state = state || "";
    },
  };
}

// When the Shortcut integration is already configured, load the
// saved identity from the vault (`GET /api/integrations/shortcut/me`,
// a pure-API endpoint that reads the token server-side): pre-fill the
// mention-name field and show "Connected as @jane". The API token is
// never echoed — only the mention name, same as the probe response.
async function loadShortcutIdentity(form, actions, svc, probe) {
  if (!svc.configured || !probe) return;
  const usernameField = form.querySelector('[data-key="username"]');
  try {
    const resp = await fetch("/api/integrations/shortcut/me", {
      credentials: "same-origin",
    });
    if (!resp.ok) return; // not configured / token rejected — stay quiet
    const body = await resp.json();
    const mention = body?.data?.mention_name;
    if (!mention) return;
    const name = body.data.name ? ` — ${body.data.name}` : "";
    probe.showStatus(`Connected as @${mention}${name}.`, "ok");
    if (usernameField && !usernameField.value) {
      usernameField.value = mention;
      usernameField.dispatchEvent(new Event("input", { bubbles: true }));
    }
  } catch (e) {
    console.error("shortcut identity load failed:", e);
  }
}

function attachGithubTestButton(form, actions) {
  const tokenField = form.querySelector('[data-key="personal_access_token"]');
  const usernameField = form.querySelector('[data-key="username"]');
  if (!tokenField || !usernameField || !actions) return;

  const cancelBtn = actions.querySelector('[data-action="cancel"]');
  const testBtn = document.createElement("button");
  testBtn.type = "button";
  testBtn.className = "ghost";
  testBtn.textContent = "Test connection";
  if (cancelBtn) {
    actions.insertBefore(testBtn, cancelBtn);
  } else {
    actions.appendChild(testBtn);
  }

  let probePanel = null;
  let status = null;
  const ensureProbePanel = () => {
    if (probePanel) return status;
    probePanel = document.createElement("div");
    probePanel.className = "chat-integration-probe-panel";
    probePanel.setAttribute("role", "group");
    probePanel.setAttribute("aria-label", "Test connection status");
    status = document.createElement("p");
    status.className = "chat-integration-discover-status";
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");
    probePanel.appendChild(status);
    actions.parentNode.insertBefore(probePanel, actions);
    return status;
  };

  testBtn.addEventListener("click", async () => {
    testBtn.disabled = true;
    const statusEl = ensureProbePanel();
    statusEl.textContent = "Testing GitHub connection…";
    statusEl.dataset.state = "pending";
    const token = tokenField.value.trim();
    if (!token) {
      testBtn.disabled = false;
      statusEl.textContent = "Please fill in the personal access token first.";
      statusEl.dataset.state = "error";
      return;
    }
    try {
      const body = await probeGithub(token);
      statusEl.dataset.state = "ok";
      const name = body.name ? ` — ${body.name}` : "";
      const plan = body.plan ? ` (${body.plan} plan)` : "";
      statusEl.textContent = `Connected as @${body.login}${name}${plan}.`;
      if (!usernameField.value) {
        usernameField.value = body.login;
        usernameField.dispatchEvent(new Event("input", { bubbles: true }));
      }
    } catch (e) {
      console.error("github probe failed:", e);
      statusEl.textContent = `Probe failed: ${e.message}`;
      statusEl.dataset.state = "error";
    } finally {
      testBtn.disabled = false;
    }
  });
}

// Async wrapper around `openEditor`. Refetches the single
// integration summary (which decrypts the `echo_on_edit` fields)
// and only opens the modal once we have the populated data —
// otherwise the form would render with every input empty. The
// list endpoint never populates `field.value` so we cannot reuse
// the row we already have on hand.
async function openIntegrationEditor(svc) {
  let fresh = svc;
  try {
    const fetched = await fetchIntegration(svc.id);
    if (fetched) fresh = fetched;
  } catch (e) {
    // Network / 5xx — fall back to the list-row snapshot so the
    // user can still edit (they just won't see the saved URL
    // until the network recovers). Surface the error to the
    // console so an operator debugging the page gets a signal.
    console.error(`failed to refetch integration ${svc.id}:`, e);
  }
  openEditor(fresh);
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

  // Operator-facing setup guide. Lives inside the modal (not
  // on the card) so the link is always one click away from
  // the form the user is filling out — CalDAV's
  // Nextcloud/Fastmail/iCloud specifics and the X Developer
  // app registration both require non-trivial context that
  // fits in a separate tab, not a sentence under a status
  // pill. The link is omitted when the ServiceDef did not
  // ship a `docs_url` so future connectors without
  // documentation still render cleanly.
  if (svc.docs_url) {
    const docs = document.createElement("a");
    docs.className = "chat-integration-modal-docs";
    docs.href = svc.docs_url;
    docs.target = "_blank";
    docs.rel = "noopener noreferrer";
    docs.textContent = "Setup guide →";
    modal.appendChild(docs);
  }

  const form = document.createElement("form");
  form.className = "chat-integration-form";
  // Fields listed in `managed_fields` are written out of band
  // (e.g. by the OAuth callback redirect) — they are NOT rendered
  // as inputs, so the form never submits them and the saved rows
  // are preserved on PUT. The per-service block below renders a
  // read-only status for them instead.
  const managed = new Set(svc.managed_fields || []);
  for (const f of svc.fields) {
    if (managed.has(f.key)) continue;
    form.appendChild(buildFormField(f));
  }
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
        document.dispatchEvent(
          new CustomEvent("integrationsupdated", { detail: { service: svc.id } }),
        );
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
  cancel.dataset.action = "cancel";
  cancel.addEventListener("click", () => overlay.remove());
  actions.appendChild(cancel);

  const save = document.createElement("button");
  save.type = "submit";
  save.className = "primary";
  save.textContent = svc.configured ? "Save changes" : "Save";
  actions.appendChild(save);
  form.appendChild(actions);

  // Plan 1791566269874: the IMAP / SMTP `ServiceDef`s each
  // ship a setup-only probe endpoint so the user can
  // validate the credentials + the operator's
  // `[email].allowlist` before committing them to the
  // per-user vault. The probe is real (Phases 3 + 5 of
  // the plan landed in the same commit): IMAP runs
  // `LOGIN` + `LIST "" *"`, SMTP runs `EHLO` /
  // `STARTTLS` / `AUTH PLAIN` / `MAIL FROM:<addr>`. The
  // button is wired to the actions row rather than
  // placed next to the host field — see the comment in
  // `attachEmailTestButton` for the UX rationale. The
  // call MUST happen after `form.appendChild(actions)` so
  // the actions row is in the DOM and the function can
  // find it.
  if (svc.id === "email_imap" || svc.id === "email_smtp") {
    // The TLS→port auto-fill listener must be wired
    // before the user starts changing the dropdown so
    // the very first `change` event surfaces a default.
    attachEmailPortAutofill(form, svc.id);
    attachEmailTestButton(form, modal, actions, svc.id);
  }
  if (svc.id === GITHUB_SERVICE_ID) {
    attachGithubTestButton(form, actions);
    // Plan 1795686864347: the PRs dashboard repo list is chosen in the
    // dedicated repo picker (orgs + checkbox list), not typed by hand.
    attachGithubReposPickerButton(form);
    // Plan 1795793328177: named extra GitHub accounts (multi-PAT) are
    // managed through the dedicated accounts endpoints; the section
    // also hosts the "Default account" selector.
    attachGithubAccountsSection(form, actions, svc);
  }
  if (svc.id === SHORTCUT_SERVICE_ID) {
    const probe = attachShortcutTestButton(form, actions);
    loadShortcutIdentity(form, actions, svc, probe);
    attachShortcutGroupSelect(form);
    attachShortcutWorkflowSelect(form);
    attachShortcutWorkflowStateSelect(form);
    attachShortcutTemplatesEditor(form);
    attachShortcutOauthButtons(form, actions, svc);
  }

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const fields = {};
    // Walk every form control (inputs AND selects) so a
    // `Select`-kind field ships its chosen value too. Empty
    // values are skipped — the PUT handler treats a missing
    // key as "keep the existing ciphertext row untouched",
    // which is the expected behaviour when the user reopens
    // the form and just clicks Save without changing anything.
    // Checkboxes are the exception: they always submit, as
    // "true" / "false", because an unchecked box is a real
    // value to persist (not an empty input to skip).
    for (const ctrl of form.querySelectorAll("input, select, textarea")) {
      if (ctrl.type === "checkbox") {
        fields[ctrl.dataset.key] = ctrl.checked ? "true" : "false";
        continue;
      }
      if (!ctrl.value) continue;
      fields[ctrl.dataset.key] = ctrl.value;
    }
    try {
      await putCredentials(svc.id, fields);
      overlay.remove();
      await renderList();
      // Let the top-level UI re-evaluate integration-driven
      // surfaces (e.g. the Shortcut "Stories" tab visibility)
      // after a save.
      document.dispatchEvent(
        new CustomEvent("integrationsupdated", { detail: { service: svc.id } }),
      );
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
showShortcutOauthResultBanner();
