// nagent — GitHub dashboard repo picker (plan 1795686864347).
//
// Dedicated UI for choosing which repositories the "PRs" dashboard
// shows. It lists the owners the saved PAT has access to (the user's
// own login + every org they belong to), then shows each owner's
// repos as a checkbox list; repos already present in the per-user
// `dashboard_repos` vault field render pre-checked.
//
// Data sources (pure API, never the LLM):
//   GET /api/integrations/github            → current dashboard_repos
//   GET /api/integrations/github/owners     → {user, orgs:[…]}
//   GET /api/integrations/github/owners/:owner/repos → {owner, repos:[…]}
// Every repo offered comes from an access-scoped GitHub call, so
// checking a box is inherently an access-verified add.
//
// Save writes ONLY the `dashboard_repos` field through the existing
// PUT /api/integrations/github/credentials flow (the PUT handler
// carries the other fields' ciphertext over verbatim, so the PAT is
// never re-sent). Classic deferred script; exposed as
// `window.__nagentGithubRepoPicker`.

(function () {
  "use strict";

  const INTEGRATION_PATH = "/api/integrations/github";
  const OWNERS_PATH = "/api/integrations/github/owners";

  function csrfHeaders() {
    return window.nagentAuth?.csrfHeaders?.() || undefined;
  }

  async function apiFetch(path, options) {
    const resp = await fetch(path, {
      credentials: "same-origin",
      ...(options || {}),
      headers: {
        ...((options && options.headers) || {}),
        ...(csrfHeaders() || {}),
      },
    });
    if (!resp.ok) {
      let msg = `HTTP ${resp.status}`;
      try {
        const body = await resp.json();
        if (body && typeof body.error === "string") msg = body.error;
      } catch {
        /* keep the status */
      }
      const err = new Error(msg);
      err.status = resp.status;
      throw err;
    }
    // The PUT credentials flow answers 204 No Content with an empty
    // body — parsing that as JSON throws "unexpected end of data",
    // which used to be misreported as "Save failed" even though the
    // save had succeeded. Only parse when there is a body.
    if (resp.status === 204) return null;
    const text = await resp.text();
    return text ? JSON.parse(text) : null;
  }

  // Read the currently saved dashboard_repos (JSON array string)
  // from the integration summary (echo_on_edit = true).
  async function loadSavedRepos() {
    const svc = await apiFetch(INTEGRATION_PATH);
    const field = (svc.fields || []).find((f) => f.key === "dashboard_repos");
    if (!field || typeof field.value !== "string" || !field.value) return [];
    try {
      const parsed = JSON.parse(field.value);
      return Array.isArray(parsed) ? parsed.filter((r) => typeof r === "string") : [];
    } catch {
      return [];
    }
  }

  function loaderEl(label) {
    const div = document.createElement("div");
    div.className = "nagent-loader";
    div.setAttribute("role", "status");
    div.textContent = label;
    return div;
  }

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
  }

  async function open() {
    // --- state -------------------------------------------------------------
    let saved = [];
    let owners = null; // {user, orgs:[{login, avatar_url}]}
    let selectedOwner = null;
    let ownerRepos = []; // [{full_name, private, archived, description, …}]
    let showArchived = false;
    const selected = new Set();
    let saving = false;

    // --- modal skeleton ------------------------------------------------------
    const overlay = el("div", "gh-picker-overlay");
    const modal = el("div", "gh-picker-modal");
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.setAttribute("aria-labelledby", "gh-picker-title");

    const closeBtn = el("button", "gh-picker-close", "×");
    closeBtn.type = "button";
    closeBtn.setAttribute("aria-label", "Close");

    const header = el("header", "gh-picker-header");
    header.appendChild(el("h3", null, "Dashboard repositories"));
    const sub = el(
      "p",
      "gh-picker-sub",
      "Tick the repositories shown in the PRs tab. Only owners and repos your GitHub token can access are listed.",
    );
    header.appendChild(sub);
    header.id = "gh-picker-title";

    const status = el("p", "gh-picker-status");
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");

    const ownersPane = el("div", "gh-picker-owners");
    const ownersTitle = el("h4", null, "Organizations & you");
    const ownersList = el("div", "gh-picker-owners-list");
    ownersPane.append(ownersTitle, ownersList);

    const reposPane = el("div", "gh-picker-repos-pane");
    const reposToolbar = el("div", "gh-picker-repos-toolbar");
    const search = document.createElement("input");
    search.type = "search";
    search.placeholder = "Filter repositories…";
    search.autocomplete = "off";
    search.spellcheck = false;
    search.className = "gh-picker-search";
    const archivedLabel = document.createElement("label");
    archivedLabel.className = "gh-picker-archived-toggle";
    const archivedCb = document.createElement("input");
    archivedCb.type = "checkbox";
    archivedCb.checked = showArchived;
    archivedCb.addEventListener("change", () => {
      showArchived = archivedCb.checked;
      if (selectedOwner) loadOwnerRepos(selectedOwner);
    });
    archivedLabel.append(archivedCb, document.createTextNode("Show archived"));
    reposToolbar.append(search, archivedLabel);
    const reposList = el("ul", "gh-picker-repos");
    reposPane.append(reposToolbar, reposList);

    const body = el("div", "gh-picker-body");
    body.append(ownersPane, reposPane);

    const count = el("span", "gh-picker-count", "0 selected");
    const cancel = el("button", "ghost", "Cancel");
    cancel.type = "button";
    const save = el("button", "primary", "Save");
    save.type = "button";
    const footer = el("div", "gh-picker-footer");
    footer.append(count, cancel, save);

    modal.append(closeBtn, header, status, body, footer);
    overlay.appendChild(modal);
    document.body.appendChild(overlay);

    const close = () => overlay.remove();
    closeBtn.addEventListener("click", close);
    cancel.addEventListener("click", close);
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) close();
    });
    document.addEventListener("keydown", function onKey(e) {
      if (e.key === "Escape") {
        close();
        document.removeEventListener("keydown", onKey);
      }
    });

    const setStatus = (msg, state) => {
      status.textContent = msg || "";
      status.dataset.state = state || "";
    };

    const updateCount = () => {
      count.textContent = `${selected.size} selected`;
    };

    // --- owners pane ---------------------------------------------------------

    function renderOwners() {
      ownersList.replaceChildren();
      if (!owners) return;
      // `owners.owners` = [{login, via: "self"|"org"|"repo"}]; the
      // self entry renders as "Your repositories (@login)", the rest
      // as plain owner names (orgs AND owners only reachable through
      // accessible repos, e.g. private outside-collaborator repos).
      const entries = (owners.owners || []).map((o) => ({
        login: o.login,
        self: o.via === "self",
      }));
      for (const entry of entries) {
        if (!entry.login) continue;
        const btn = el(
          "button",
          "gh-picker-owner" + (entry.login === selectedOwner ? " active" : ""),
          entry.self ? `Your repositories (@${entry.login})` : entry.login,
        );
        btn.type = "button";
        btn.addEventListener("click", () => {
          selectedOwner = entry.login;
          renderOwners();
          loadOwnerRepos(entry.login);
        });
        ownersList.appendChild(btn);
      }
    }

    // --- repos pane ----------------------------------------------------------

    function renderRepos() {
      const q = search.value.trim().toLowerCase();
      reposList.removeAttribute("aria-busy");
      reposList.replaceChildren();
      let shown = 0;
      for (const repo of ownerRepos) {
        const name = repo.full_name || "";
        if (q && !name.toLowerCase().includes(q)) continue;
        shown += 1;
        const li = document.createElement("li");
        li.className = "gh-picker-repo";
        const label = document.createElement("label");
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.checked = selected.has(name);
        cb.addEventListener("change", () => {
          if (cb.checked) selected.add(name);
          else selected.delete(name);
          updateCount();
        });
        const nameSpan = el("span", "gh-picker-repo-name", name);
        label.append(cb, nameSpan);
        if (repo.private) label.appendChild(el("span", "gh-picker-badge", "private"));
        if (repo.archived) label.appendChild(el("span", "gh-picker-badge", "archived"));
        if (repo.description) nameSpan.title = repo.description;
        li.appendChild(label);
        reposList.appendChild(li);
      }
      if (!shown) {
        reposList.appendChild(el("li", "gh-picker-empty", "No repositories here."));
      }
    }

    search.addEventListener("input", renderRepos);

    async function loadOwnerRepos(owner) {
      setStatus(`Loading ${owner}'s repositories…`, "pending");
      reposList.setAttribute("aria-busy", "true");
      reposList.replaceChildren();
      const loaderLi = document.createElement("li");
      loaderLi.className = "gh-picker-loader";
      loaderLi.appendChild(loaderEl(`Loading ${owner}'s repositories…`));
      reposList.appendChild(loaderLi);
      try {
        const qs = showArchived ? "?include_archived=true" : "";
        const body = await apiFetch(
          `${OWNERS_PATH}/${encodeURIComponent(owner)}/repos${qs}`,
        );
        ownerRepos = (body.data && body.data.repos) || [];
        renderRepos();
        setStatus("", "");
      } catch (e) {
        ownerRepos = [];
        renderRepos();
        setStatus(
          e.status === 404
            ? `No access to ${owner} with the saved token.`
            : `Failed to load ${owner}'s repositories: ${e.message}`,
          "error",
        );
      }
    }

    // --- save ----------------------------------------------------------------

    save.addEventListener("click", async () => {
      if (saving) return;
      saving = true;
      save.disabled = true;
      setStatus("Saving…", "pending");
      try {
        const payload = JSON.stringify([...selected]);
        await apiFetch(`${INTEGRATION_PATH}/credentials`, {
          method: "PUT",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ fields: { dashboard_repos: payload } }),
        });
        // Keep any open editor form in sync so its generic submit
        // does not overwrite the fresh list with a stale value.
        for (const input of document.querySelectorAll('[data-key="dashboard_repos"]')) {
          input.value = payload;
        }
        document.dispatchEvent(
          new CustomEvent("integrationsupdated", { detail: { service: "github" } }),
        );
        close();
      } catch (e) {
        setStatus(`Save failed: ${e.message}`, "error");
      } finally {
        saving = false;
        save.disabled = false;
      }
    });

    // --- boot ----------------------------------------------------------------

    setStatus("Loading…", "pending");
    ownersList.replaceChildren();
    ownersList.appendChild(loaderEl("Loading owners…"));
    try {
      saved = await loadSavedRepos();
      for (const r of saved) selected.add(r);
      updateCount();
      owners = (await apiFetch(OWNERS_PATH)).data;
      const ownerList = owners.owners || [];
      selectedOwner =
        owners.user || (ownerList[0] && ownerList[0].login) || null;
      renderOwners();
      setStatus("", "");
      if (selectedOwner) await loadOwnerRepos(selectedOwner);
    } catch (e) {
      ownersList.replaceChildren();
      ownersList.appendChild(el("div", "gh-picker-empty", "Could not load owners."));
      setStatus(
        e.status === 400
          ? "The GitHub integration is not configured — save a personal access token first."
          : `Failed to load: ${e.message}`,
        "error",
      );
    }
  }

  globalThis.__nagentGithubRepoPicker = { open };
})();
