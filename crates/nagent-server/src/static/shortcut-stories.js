// nagent — Shortcut "Stories" dashboard tab.
//
// Shows the top-menu "Stories" tab only when the Shortcut
// integration has the "Show my stories dashboard" checkbox ticked
// (GET /api/integrations/shortcut → field `show_dashboard`). The
// list and the detail come from the pure-API endpoints
//   GET /api/integrations/shortcut/stories?query=&limit=
//   GET /api/integrations/shortcut/stories/:id
// which read the caller's vault token server-side — the LLM is
// never involved in this view.
//
// Classic deferred script (like mode.js): marked / DOMPurify are
// UMD globals loaded in <head>, and the mode toggle is exposed as
// `window.__nagentMode`.

(function () {
  "use strict";

  const LIST_PATH = "/api/integrations/shortcut/stories";
  const INTEGRATION_PATH = "/api/integrations/shortcut";

  const els = {
    list: document.getElementById("stories-list"),
    overlay: document.getElementById("stories-overlay"),
    detail: document.getElementById("stories-detail"),
    close: document.getElementById("stories-close"),
    status: document.getElementById("stories-status"),
    search: document.getElementById("stories-search"),
    includeDone: document.getElementById("stories-include-done"),
    refresh: document.getElementById("stories-refresh"),
    detailTitle: document.getElementById("stories-detail-title"),
    detailMeta: document.getElementById("stories-detail-meta"),
    detailLabels: document.getElementById("stories-detail-labels"),
    detailBody: document.getElementById("stories-detail-body"),
    commentsList: document.getElementById("stories-comments-list"),
  };

  // True when the tab is visible; drives whether we fetch at all.
  let tabVisible = false;
  // The query the currently-rendered list was loaded with, so the
  // Refresh button re-runs the same search.
  let currentQuery = "";

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, (c) => {
      switch (c) {
        case "&": return "&amp;";
        case "<": return "&lt;";
        case ">": return "&gt;";
        case '"': return "&quot;";
        default: return "&#39;";
      }
    });
  }

  // Same pipeline as chat.js's renderMarkdown: marked (gfm +
  // breaks) then DOMPurify, with anchors forced to open in a new
  // tab. Story descriptions and comments are untrusted upstream
  // content, so they only ever reach the DOM through this path.
  function renderMarkdown(text) {
    if (!text) return "";
    if (
      typeof window.marked?.parse === "function" &&
      typeof window.DOMPurify?.sanitize === "function"
    ) {
      const raw = window.marked.parse(String(text), { breaks: true, gfm: true });
      return window.DOMPurify.sanitize(raw, { ADD_ATTR: ["target", "rel"] });
    }
    // Vendor scripts missing: escaped plain text fallback.
    return escapeHtml(text).replace(/\n/g, "<br>");
  }

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
    return resp.json();
  }

  // --- Tab visibility -----------------------------------------------------

  async function dashboardEnabled() {
    try {
      const svc = await apiFetch(INTEGRATION_PATH);
      if (!svc || svc.configured !== true) return false;
      const field = (svc.fields || []).find((f) => f.key === "show_dashboard");
      return !!field && field.value === "true";
    } catch (e) {
      // Not logged in / server without the shortcut-agent feature:
      // the tab simply stays hidden.
      return false;
    }
  }

  async function refreshTabVisibility() {
    const on = await dashboardEnabled();
    tabVisible = on;
    if (window.__nagentMode?.setStoriesEnabled) {
      window.__nagentMode.setStoriesEnabled(on);
    }
  }

  // --- List ---------------------------------------------------------------

  function fmtDate(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? "" : d.toLocaleDateString();
  }

  // `app_url` comes from the upstream Shortcut API (untrusted), so it
  // is only turned into a link after a strict parse: https only.
  // The URL is assigned via `a.href =` (never innerHTML), and the
  // link always opens in a new tab with noopener.
  function safeAppUrl(url) {
    if (typeof url !== "string" || !url) return null;
    try {
      const u = new URL(url);
      if (u.protocol !== "https:") return null;
      return u.href;
    } catch {
      return null;
    }
  }

  function appLink(url, className, text) {
    const href = safeAppUrl(url);
    if (!href) return null;
    const a = document.createElement("a");
    a.className = className;
    a.href = href;
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    a.textContent = text;
    return a;
  }

  // Icon-only variant for the list rows: a quiet "↗" ghost icon at
  // the right edge of the row (tooltip on hover/focus), instead of
  // a bordered text button next to the row.
  function appIconLink(url) {
    const a = appLink(url, "stories-item-ext", "↗");
    if (!a) return null;
    a.setAttribute("aria-label", "Open in Shortcut");
    a.title = "Open in Shortcut";
    return a;
  }

  function labelBadge(label) {
    const span = document.createElement("span");
    span.className = "stories-label";
    span.textContent = label && label.name ? label.name : String(label);
    if (label && label.color) {
      span.style.borderColor = label.color;
    }
    return span;
  }

  function renderList(data) {
    if (!els.list) return;
    els.list.replaceChildren();
    const stories = (data && data.stories) || [];
    if (!stories.length) {
      const li = document.createElement("li");
      li.className = "stories-empty";
      li.textContent = "No stories match this query.";
      els.list.appendChild(li);
      return;
    }
    for (const s of stories) {
      const li = document.createElement("li");
      li.className = "stories-item-row";
      const item = document.createElement("button");
      item.type = "button";
      item.className = "stories-item";
      item.dataset.storyId = String(s.id);

      const title = document.createElement("span");
      title.className = "stories-item-title";
      title.textContent = s.name || `(story #${s.id})`;

      const meta = document.createElement("span");
      meta.className = "stories-item-meta";
      const bits = [];
      if (s.story_type) bits.push(s.story_type);
      if (s.estimate != null) bits.push(`est. ${s.estimate}`);
      const when = fmtDate(s.updated_at);
      if (when) bits.push(when);
      meta.textContent = bits.join(" · ");

      const labels = document.createElement("span");
      labels.className = "stories-item-labels";
      // List endpoint projects labels as plain names.
      for (const name of s.labels || []) {
        labels.appendChild(labelBadge({ name }));
      }

      item.append(title, meta, labels);
      item.addEventListener("click", () => openStory(s.id));
      li.appendChild(item);
      // Quiet external-link icon on each row — a real link to the
      // story's app_url, separate from the overlay-opening button.
      const ext = appIconLink(s.app_url);
      if (ext) li.appendChild(ext);
      els.list.appendChild(li);
    }
  }

  async function loadList(query) {
    if (!tabVisible || !els.list) return;
    currentQuery = query || "";
    setStatus("Loading…", "pending");
    els.list.replaceChildren();
    try {
      const q = new URLSearchParams();
      if (currentQuery) q.set("query", currentQuery);
      // The server always ANDs `owner:<me>` onto the query, so the
      // free-text filter can only ever match the user's own stories.
      if (els.includeDone?.checked) q.set("include_done", "true");
      q.set("limit", "50");
      const body = await apiFetch(`${LIST_PATH}?${q.toString()}`);
      renderList(body.data);
      const total = body.data && body.data.total;
      setStatus(
        typeof total === "number"
          ? `${body.data.count} of ${total} stories`
          : `${(body.data && body.data.count) || 0} stories`,
        "ok",
      );
    } catch (e) {
      setStatus(e.status === 401
        ? "Shortcut rejected the API token — reconnect the integration in Settings → Integrations."
        : `Failed to load stories: ${e.message}`, "error");
    }
  }

  // --- Detail (overlay) ---------------------------------------------------

  // The story detail opens as a modal overlay on top of the list —
  // closed by the × button, by clicking the backdrop, or with Esc.
  function closeOverlay() {
    if (els.overlay) els.overlay.hidden = true;
  }

  function openOverlay() {
    if (els.overlay) els.overlay.hidden = false;
  }

  function renderDetail(data) {
    const story = (data && data.story) || {};
    els.detailTitle.textContent = story.name || `Story #${story.id}`;
    const bits = [];
    if (story.story_type) bits.push(story.story_type);
    if (story.estimate != null) bits.push(`estimate ${story.estimate}`);
    const when = fmtDate(story.updated_at);
    if (when) bits.push(`updated ${when}`);
    // Meta line + clickable "Open in Shortcut" link in the overlay
    // header (the app_url is validated https-only before it becomes
    // an href).
    els.detailMeta.replaceChildren();
    if (bits.length) els.detailMeta.appendChild(document.createTextNode(bits.join(" · ")));
    const link = appLink(story.app_url, "stories-detail-link", "Open in Shortcut ↗");
    if (link) {
      if (bits.length) els.detailMeta.appendChild(document.createTextNode(" · "));
      els.detailMeta.appendChild(link);
    }

    els.detailLabels.replaceChildren();
    for (const label of story.labels || []) {
      els.detailLabels.appendChild(labelBadge(label));
    }

    // Description rendered as sanitized markdown — the only place
    // untrusted upstream HTML is allowed into the DOM.
    els.detailBody.innerHTML = renderMarkdown(story.description || "");
    for (const a of els.detailBody.querySelectorAll("a[href]")) {
      a.target = "_blank";
      a.rel = "noopener noreferrer";
    }

    els.commentsList.replaceChildren();
    const comments = (data && data.comments) || [];
    if (!comments.length) {
      const li = document.createElement("li");
      li.className = "stories-comment-empty";
      li.textContent = "No comments yet.";
      els.commentsList.appendChild(li);
      return;
    }
    for (const c of comments) {
      const li = document.createElement("li");
      li.className = "stories-comment";
      const head = document.createElement("div");
      head.className = "stories-comment-head";
      head.textContent = fmtDate(c.created_at) || "comment";
      const body = document.createElement("div");
      body.className = "stories-comment-body";
      body.innerHTML = renderMarkdown(c.text || "");
      for (const a of body.querySelectorAll("a[href]")) {
        a.target = "_blank";
        a.rel = "noopener noreferrer";
      }
      li.append(head, body);
      els.commentsList.appendChild(li);
    }
  }

  async function openStory(id) {
    setStatus("Loading story…", "pending");
    try {
      const body = await apiFetch(`${LIST_PATH}/${encodeURIComponent(id)}`);
      renderDetail(body.data);
      openOverlay();
      setStatus("", "");
    } catch (e) {
      setStatus(`Failed to load story ${id}: ${e.message}`, "error");
    }
  }

  // --- Wiring -------------------------------------------------------------

  function init() {
    if (!els.list) return; // view markup missing — nothing to do

    document.addEventListener("modechange", (e) => {
      if (e.detail && e.detail.mode === "stories" && tabVisible) {
        closeOverlay();
        loadList(currentQuery);
      }
    });

    // Re-evaluate tab visibility whenever an integration is saved or
    // removed in the Settings view.
    document.addEventListener("integrationsupdated", () => {
      refreshTabVisibility();
    });

    if (els.refresh) {
      els.refresh.addEventListener("click", () => loadList(currentQuery));
    }
    if (els.close) {
      els.close.addEventListener("click", closeOverlay);
    }
    if (els.overlay) {
      // Click on the backdrop (not on the card) closes the overlay.
      els.overlay.addEventListener("click", (e) => {
        if (e.target === els.overlay) closeOverlay();
      });
    }
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && els.overlay && !els.overlay.hidden) {
        closeOverlay();
      }
    });
    if (els.includeDone) {
      els.includeDone.addEventListener("change", () => loadList(currentQuery));
    }
    if (els.search) {
      let timer = null;
      els.search.addEventListener("input", () => {
        if (timer) clearTimeout(timer);
        timer = setTimeout(() => loadList(els.search.value.trim()), 400);
      });
      els.search.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          if (timer) clearTimeout(timer);
          loadList(els.search.value.trim());
        }
      });
    }

    refreshTabVisibility();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
