// nagent — GitHub "PRs" dashboard tab.
//
// Shows the top-menu "PRs" tab only when the GitHub integration has
// the "Show my PRs dashboard" checkbox ticked (GET
// /api/integrations/github → field `show_dashboard`). The PR table
// comes from the pure-API endpoint
//   GET /api/integrations/github/prs?repo=&state=open&limit=
// which reads the caller's vault token and the configured
// `dashboard_repos` list server-side — the LLM is never involved in
// this view.
//
// Classic deferred script (like mode.js / shortcut-stories.js): the
// mode toggle is exposed as `window.__nagentMode`.

(function () {
  "use strict";

  const LIST_PATH = "/api/integrations/github/prs";
  const INTEGRATION_PATH = "/api/integrations/github";
  // Plan 1795793328177: the named GitHub accounts behind the
  // cross-account `?account=` filter dropdown.
  const ACCOUNTS_PATH = "/api/integrations/github/accounts";

  const els = {
    tbody: document.getElementById("prs-list"),
    status: document.getElementById("prs-status"),
    account: document.getElementById("prs-account-filter"),
    filter: document.getElementById("prs-repo-filter"),
    state: document.getElementById("prs-state"),
    enrich: document.getElementById("prs-enrich"),
    refresh: document.getElementById("prs-refresh"),
    manageRepos: document.getElementById("prs-manage-repos"),
    table: document.getElementById("prs-table"),
  };

  // True when the tab is visible; drives whether we fetch at all.
  let tabVisible = false;
  // The filters the currently-rendered table was loaded with, so the
  // Refresh button re-runs the same query.
  let currentRepo = "";
  let currentState = "open";
  // `?account=` filter: "all" (cross-account aggregation, the
  // server default) or one named account.
  let currentAccount = "all";
  // Whether to attach CI status (?enrich=true): one extra cached call
  // per PR, so opt-in.
  let enrichCi = false;
  // The rows of the last successful load, so header clicks can
  // re-sort client-side without refetching.
  let lastRows = [];
  // Current client-side sort: { key, dir } — null means "server
  // order" (updated_at desc).
  let sortSpec = null;

  function setStatus(message, state) {
    if (!els.status) return;
    els.status.textContent = message || "";
    els.status.dataset.state = state || "";
  }

  async function apiFetch(path) {
    const resp = await fetch(path, { credentials: "same-origin" });
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
    // Defensive: never JSON.parse an empty body (204 / empty 200).
    if (resp.status === 204) return null;
    const text = await resp.text();
    return text ? JSON.parse(text) : null;
  }

  // --- Tab visibility -----------------------------------------------------

  async function dashboardEnabled() {
    try {
      const svc = await apiFetch(INTEGRATION_PATH);
      if (!svc || svc.configured !== true) return false;
      const field = (svc.fields || []).find((f) => f.key === "show_dashboard");
      return !!field && field.value === "true";
    } catch (e) {
      // Not logged in / server without the github-agent feature:
      // the tab simply stays hidden.
      return false;
    }
  }

  async function refreshTabVisibility() {
    const on = await dashboardEnabled();
    tabVisible = on;
    if (window.__nagentMode?.setPrsEnabled) {
      window.__nagentMode.setPrsEnabled(on);
    }
  }

  // --- Rendering ----------------------------------------------------------

  function fmtDate(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? "" : d.toLocaleDateString();
  }

  function fmtDateTimeTitle(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? "" : d.toLocaleString();
  }

  // `html_url` comes from the upstream GitHub API (untrusted), so it
  // is only turned into a link after a strict parse: https only.
  // The URL is assigned via `a.href =` (never innerHTML), and the
  // link always opens in a new tab with noopener.
  function safeHttpsUrl(url) {
    if (typeof url !== "string" || !url) return null;
    try {
      const u = new URL(url);
      if (u.protocol !== "https:") return null;
      return u.href;
    } catch {
      return null;
    }
  }

  function prLink(pr) {
    const href = safeHttpsUrl(pr.html_url);
    if (!href) return null;
    const a = document.createElement("a");
    a.href = href;
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    a.className = "prs-link";
    a.textContent = `#${pr.number}`;
    return a;
  }

  // CI badge for one row's `ci` object ({state, detail, url}); the
  // badge links to the PR's /checks tab when a URL is present.
  function ciBadge(ci) {
    const state = (ci && ci.state) || "none";
    const label = {
      success: "passing",
      failure: "failing",
      pending: "pending",
      none: "no checks",
      unknown: "n/a",
    }[state] || state;
    const span = document.createElement("span");
    span.className = "prs-ci";
    span.dataset.state = state;
    span.textContent = label;
    if (ci && ci.detail) span.title = ci.detail;
    const href = ci && safeHttpsUrl(ci.url);
    if (!href) return span;
    const a = document.createElement("a");
    a.href = href;
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    a.className = "prs-ci-link";
    a.appendChild(span);
    return a;
  }

  function labelBadge(label) {
    const span = document.createElement("span");
    span.className = "prs-label";
    span.textContent = label && label.name ? label.name : String(label);
    if (label && label.color) {
      span.style.borderColor = `#${label.color}`;
    }
    return span;
  }

  function cell(text) {
    const td = document.createElement("td");
    if (text != null && text !== "") td.textContent = String(text);
    return td;
  }

  function renderRow(pr) {
    const tr = document.createElement("tr");

    const repoTd = cell(pr.repo);
    tr.appendChild(repoTd);

    // Account facet (plan 1795793328177): every row is tagged with
    // the account whose token fetched it.
    tr.appendChild(cell(pr.account));

    const numTd = document.createElement("td");
    const link = prLink(pr);
    numTd.appendChild(link || document.createTextNode(`#${pr.number ?? ""}`));
    tr.appendChild(numTd);

    tr.appendChild(cell(pr.title));
    tr.appendChild(cell(pr.user));

    const labelsTd = document.createElement("td");
    labelsTd.className = "prs-labels";
    for (const label of pr.labels || []) {
      labelsTd.appendChild(labelBadge(label));
    }
    tr.appendChild(labelsTd);

    const branchTd = cell(
      pr.head_ref && pr.base_ref ? `${pr.head_ref} → ${pr.base_ref}` : pr.head_ref || "",
    );
    tr.appendChild(branchTd);

    const stateTd = document.createElement("td");
    const stateSpan = document.createElement("span");
    stateSpan.className = "prs-state";
    stateSpan.dataset.state = pr.draft ? "draft" : pr.state || "";
    stateSpan.textContent = pr.draft ? "draft" : pr.state || "";
    stateTd.appendChild(stateSpan);
    tr.appendChild(stateTd);

    const ciTd = document.createElement("td");
    ciTd.appendChild(ciBadge(pr.ci));
    tr.appendChild(ciTd);

    const createdTd = cell(fmtDate(pr.created_at));
    createdTd.title = fmtDateTimeTitle(pr.created_at);
    tr.appendChild(createdTd);

    const updatedTd = cell(fmtDate(pr.updated_at));
    updatedTd.title = fmtDateTimeTitle(pr.updated_at);
    tr.appendChild(updatedTd);

    return tr;
  }

  function sortRows(rows, spec) {
    if (!spec) return rows;
    const { key, dir } = spec;
    const mul = dir === "asc" ? 1 : -1;
    const val = (pr) => {
      if (key === "author") return pr.user || "";
      return pr[key] ?? "";
    };
    return [...rows].sort((a, b) => {
      const va = val(a);
      const vb = val(b);
      if (typeof va === "number" && typeof vb === "number") {
        return (va - vb) * mul;
      }
      return String(va).localeCompare(String(vb)) * mul;
    });
  }

  function loaderEl(label) {
    const div = document.createElement("div");
    div.className = "nagent-loader";
    div.setAttribute("role", "status");
    div.textContent = label;
    return div;
  }

  function showTableLoader(label) {
    if (!els.tbody) return;
    if (els.table) els.table.setAttribute("aria-busy", "true");
    els.tbody.replaceChildren();
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 11;
    td.className = "prs-loader-cell";
    td.appendChild(loaderEl(label));
    tr.appendChild(td);
    els.tbody.appendChild(tr);
  }

  function hideTableLoader() {
    if (els.table) els.table.removeAttribute("aria-busy");
  }

  function showTableError(message) {
    hideTableLoader();
    if (!els.tbody) return;
    els.tbody.replaceChildren();
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 11;
    td.className = "prs-empty prs-error";
    td.textContent = message;
    tr.appendChild(td);
    els.tbody.appendChild(tr);
  }

  function renderTable(data) {
    if (!els.tbody) return;
    hideTableLoader();
    const rows = sortRows(lastRows, sortSpec);
    els.tbody.replaceChildren();
    if (!rows.length) {
      const tr = document.createElement("tr");
      const td = document.createElement("td");
      td.colSpan = 11;
      td.className = "prs-empty";
      td.textContent = "No pull requests to show.";
      tr.appendChild(td);
      els.tbody.appendChild(tr);
      return;
    }
    for (const pr of rows) {
      els.tbody.appendChild(renderRow(pr));
    }
  }

  function renderRepoFilter(repos) {
    if (!els.filter) return;
    const previous = currentRepo;
    els.filter.replaceChildren();
    const all = document.createElement("option");
    all.value = "";
    all.textContent = "All repositories";
    els.filter.appendChild(all);
    for (const repo of repos || []) {
      const opt = document.createElement("option");
      opt.value = repo;
      opt.textContent = repo;
      els.filter.appendChild(opt);
    }
    els.filter.value = repos && repos.includes(previous) ? previous : "";
    currentRepo = els.filter.value;
  }

  // Populate the account filter from GET …/accounts ("all" plus one
  // option per configured account name). Tokens are never part of
  // that response (AGENTS.md §6).
  async function renderAccountFilter() {
    if (!els.account) return;
    const previous = currentAccount || "all";
    try {
      const body = await apiFetch(ACCOUNTS_PATH);
      const accounts = (body && body.accounts) || [];
      els.account.replaceChildren();
      const all = document.createElement("option");
      all.value = "all";
      all.textContent = "All accounts";
      els.account.appendChild(all);
      for (const acc of accounts) {
        const opt = document.createElement("option");
        opt.value = acc.name;
        opt.textContent = acc.is_default ? `${acc.name} (default)` : acc.name;
        els.account.appendChild(opt);
      }
      els.account.value = accounts.some((a) => a.name === previous) || previous === "all"
        ? previous
        : "all";
      currentAccount = els.account.value;
    } catch {
      // Endpoint absent (older server / feature off): keep the
      // single "all" option already in the markup.
      if (!els.account.options.length) {
        const all = document.createElement("option");
        all.value = "all";
        all.textContent = "All accounts";
        els.account.appendChild(all);
      }
      els.account.value = "all";
      currentAccount = "all";
    }
  }

  // --- List ---------------------------------------------------------------

  async function loadList(refresh) {
    if (!tabVisible || !els.tbody) return;
    setStatus("Loading…", "pending");
    showTableLoader("Loading pull requests…");
    try {
      const q = new URLSearchParams();
      if (currentRepo) q.set("repo", currentRepo);
      q.set("state", currentState || "open");
      q.set("limit", "50");
      // Cross-account aggregation is the server default; send the
      // filter explicitly so a named account narrows the list.
      q.set("account", currentAccount || "all");
      if (enrichCi) q.set("enrich", "true");
      // Refresh bypasses the server-side response cache so a
      // just-merged PR disappears immediately.
      if (refresh) q.set("refresh", "true");
      const body = await apiFetch(`${LIST_PATH}?${q.toString()}`);
      const data = (body && body.data) || {};
      lastRows = data.prs || [];
      renderRepoFilter(data.repos);
      renderTable(data);
      const errs = data.repo_errors || [];
      const errNote = errs.length
        ? ` · ${errs.length} repo(s) unreachable (${errs
            .map((e) => (e.account ? `${e.repo} via ${e.account}` : e.repo))
            .join(", ")})`
        : "";
      setStatus(`${data.count || 0} of ${data.total ?? data.count ?? 0} pull requests${errNote}`, "ok");
    } catch (e) {
      const msg =
        e.status === 401
          ? "GitHub rejected the personal access token — reconnect the integration in Settings → Integrations."
          : e.status === 400
            ? e.message
            : `Failed to load pull requests: ${e.message}`;
      showTableError(msg);
      setStatus(msg, "error");
    }
  }

  // --- Wiring -------------------------------------------------------------

  function onSortClick(th) {
    const key = th.dataset.sort;
    if (!key) return;
    if (sortSpec && sortSpec.key === key) {
      sortSpec = { key, dir: sortSpec.dir === "asc" ? "desc" : "asc" };
    } else {
      sortSpec = { key, dir: "asc" };
    }
    for (const other of els.table.querySelectorAll("th[data-sort]")) {
      other.removeAttribute("aria-sort");
    }
    th.setAttribute(
      "aria-sort",
      sortSpec.dir === "asc" ? "ascending" : "descending",
    );
    renderTable();
  }

  function init() {
    if (!els.tbody) return; // view markup missing — nothing to do

    document.addEventListener("modechange", (e) => {
      if (e.detail && e.detail.mode === "prs" && tabVisible) {
        loadList();
      }
    });

    // Re-evaluate tab visibility whenever an integration is saved or
    // removed in the Settings view; if the PRs dashboard itself was
    // what changed (repo picker save), reload the table too.
    document.addEventListener("integrationsupdated", (e) => {
      refreshTabVisibility();
      if (tabVisible && (!e.detail || !e.detail.service || e.detail.service === "github")) {
        // Account add/remove changes the filter options too.
        renderAccountFilter().then(() => loadList());
      }
    });

    if (els.refresh) {
      els.refresh.addEventListener("click", () => loadList(true));
    }
    if (els.manageRepos) {
      els.manageRepos.addEventListener("click", () => {
        window.__nagentGithubRepoPicker?.open();
      });
    }
    if (els.filter) {
      els.filter.addEventListener("change", () => {
        currentRepo = els.filter.value;
        sortSpec = null;
        loadList();
      });
    }
    if (els.account) {
      els.account.addEventListener("change", () => {
        currentAccount = els.account.value || "all";
        sortSpec = null;
        loadList();
      });
    }
    if (els.state) {
      els.state.addEventListener("change", () => {
        currentState = els.state.value || "open";
        sortSpec = null;
        loadList();
      });
    }
    if (els.enrich) {
      els.enrich.addEventListener("change", () => {
        enrichCi = els.enrich.checked;
        sortSpec = null;
        loadList();
      });
    }
    if (els.table) {
      for (const th of els.table.querySelectorAll("th[data-sort]")) {
        th.addEventListener("click", () => onSortClick(th));
      }
    }

    refreshTabVisibility();
    renderAccountFilter();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
