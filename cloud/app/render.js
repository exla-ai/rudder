// Markdown rendering, icons and small DOM helpers.

const CDN = "https://cdn.jsdelivr.net/npm/";
let libs = null;
let libsPromise = null;
const listeners = new Set();

export function onMarkdownReady(fn) { listeners.add(fn); }
export function markdownReady() { return !!libs; }

export function loadMarkdown() {
  if (libsPromise) return libsPromise;
  libsPromise = Promise.all([
    import(CDN + "marked@12.0.2/lib/marked.esm.js"),
    import(CDN + "dompurify@3.1.6/dist/purify.es.mjs"),
    import(CDN + "@highlightjs/cdn-assets@11.9.0/es/highlight.min.js"),
  ]).then(([m, p, h]) => {
    const marked = new m.Marked({ gfm: true, breaks: false });
    const purify = p.default;
    purify.addHook("afterSanitizeAttributes", (node) => {
      if (node.tagName === "A" && node.getAttribute("href")) {
        node.setAttribute("target", "_blank");
        node.setAttribute("rel", "noopener noreferrer");
      }
    });
    libs = { marked, purify, hljs: h.default };
    for (const fn of listeners) { try { fn(); } catch (e) { console.error(e); } }
  }).catch((err) => {
    console.warn("markdown libraries unavailable, using plain text", err);
    libsPromise = null;
  });
  return libsPromise;
}

export function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

/** Render markdown into `el`. Falls back to escaped text until libraries load. */
export function renderMarkdown(el, text) {
  const src = String(text ?? "");
  if (!libs) {
    el.innerHTML = src.split(/\n{2,}/).map((p) => `<p>${escapeHtml(p).replace(/\n/g, "<br>")}</p>`).join("");
    el.dataset.plain = "1";
    return;
  }
  delete el.dataset.plain;
  const html = libs.purify.sanitize(libs.marked.parse(src), { USE_PROFILES: { html: true } });
  el.innerHTML = html;
  enhance(el);
}

function enhance(root) {
  for (const table of root.querySelectorAll("table")) {
    const wrap = document.createElement("div");
    wrap.className = "table-wrap";
    table.replaceWith(wrap);
    wrap.appendChild(table);
  }
  for (const pre of root.querySelectorAll("pre")) {
    const code = pre.querySelector("code");
    if (!code) continue;
    const m = /language-([\w+#-]+)/.exec(code.className || "");
    let lang = m ? m[1] : "";
    try {
      if (lang && libs.hljs.getLanguage(lang)) libs.hljs.highlightElement(code);
      else if (code.textContent.length < 20000) {
        const r = libs.hljs.highlightAuto(code.textContent);
        if (r.relevance > 5) { code.innerHTML = r.value; lang = lang || r.language || ""; }
      }
    } catch { /* leave plain */ }
    const box = document.createElement("div");
    box.className = "codeblock";
    box.innerHTML = `<div class="cb-head"><span class="cb-lang">${escapeHtml(lang || "text")}</span><button type="button" class="copy-btn" aria-label="Copy code">${icon("copy")}<span>Copy</span></button></div>`;
    pre.replaceWith(box);
    box.appendChild(pre);
  }
}

export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand("copy"); } catch { ok = false; }
    ta.remove();
    return ok;
  }
}

// Icons: 24px stroke glyphs.
const P = {
  back: '<path d="M15 18l-6-6 6-6"/>',
  chevron: '<path d="M9 18l6-6-6-6"/>',
  refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 4v7h-7"/>',
  more: '<circle cx="5" cy="12" r="1.3" fill="currentColor"/><circle cx="12" cy="12" r="1.3" fill="currentColor"/><circle cx="19" cy="12" r="1.3" fill="currentColor"/>',
  send: '<path d="M12 19V5"/><path d="M5 12l7-7 7 7"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  terminal: '<path d="M4 17l6-6-6-6"/><path d="M12 19h8"/>',
  file: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/>',
  edit: '<path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M13.5 6.5l4 4"/>',
  globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"/>',
  stop: '<rect x="6" y="6" width="12" height="12" rx="2"/>',
  trash: '<path d="M4 7h16"/><path d="M10 11v6M14 11v6"/><path d="M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12"/><path d="M9 7V4h6v3"/>',
  merge: '<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="12" r="2"/><path d="M6 7v10"/><path d="M6 7c0 3 3 5 10 5"/>',
  external: '<path d="M14 4h6v6"/><path d="M20 4l-9 9"/><path d="M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a1 1 0 0 1 1-1h10"/>',
  check: '<path d="M5 12.5l4.5 4.5L19 7.5"/>',
  server: '<rect x="3" y="4" width="18" height="7" rx="2"/><rect x="3" y="13" width="18" height="7" rx="2"/><path d="M7 7.5h.01M7 16.5h.01"/>',
  power: '<path d="M12 3v8"/><path d="M6.3 6.8a8 8 0 1 0 11.4 0"/>',
  tool: '<path d="M14.7 6.3a4 4 0 0 0-5.4 5.2L4 16.8V20h3.2l5.3-5.3a4 4 0 0 0 5.2-5.4l-2.5 2.5-2.8-.7-.7-2.8z"/>',
  web: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/>',
  task: '<path d="M9 11l3 3 8-8"/><path d="M20 12v7a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1h11"/>',
  clip: '<path d="M20.5 11.5l-8.3 8.3a5 5 0 0 1-7.1-7.1l8.5-8.5a3.3 3.3 0 0 1 4.7 4.7l-8.5 8.5a1.7 1.7 0 0 1-2.4-2.4l7.8-7.8"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  pin: '<path d="M9 3h6"/><path d="M10 3v5.5L7 13h10l-3-4.5V3z"/><path d="M12 13v8"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>',
  down: '<path d="M12 5v14"/><path d="M6 13l6 6 6-6"/>',
  inbox: '<path d="M3 13l3-8h12l3 8"/><path d="M3 13v5a1 1 0 0 0 1 1h16a1 1 0 0 0 1-1v-5h-6l-1 2h-4l-1-2z"/>',
  github: '<path d="M9 19c-4.3 1.4-4.3-2.5-6-3m12 5v-3.5c0-1 .1-1.4-.5-2 2.8-.3 5.5-1.4 5.5-6a4.6 4.6 0 0 0-1.3-3.2 4.2 4.2 0 0 0-.1-3.2s-1.1-.3-3.5 1.3a12 12 0 0 0-6.2 0C6.5 2.8 5.4 3.1 5.4 3.1a4.2 4.2 0 0 0-.1 3.2A4.6 4.6 0 0 0 4 9.5c0 4.6 2.7 5.7 5.5 6-.6.6-.6 1.2-.5 2V21"/>',
  settings: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>',
  alert: '<path d="M12 9v4M12 17h.01"/><path d="M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/>',
};
const G = '<path fill="#4285F4" d="M22.6 12.2c0-.7-.1-1.4-.2-2H12v3.9h5.9a5 5 0 0 1-2.2 3.3v2.7h3.6c2.1-1.9 3.3-4.8 3.3-7.9z"/><path fill="#34A853" d="M12 23c3 0 5.5-1 7.3-2.7l-3.6-2.8c-1 .7-2.3 1.1-3.7 1.1-2.9 0-5.3-1.9-6.2-4.5H2.1v2.9A11 11 0 0 0 12 23z"/><path fill="#FBBC05" d="M5.8 14.1a6.6 6.6 0 0 1 0-4.2V7H2.1a11 11 0 0 0 0 9.9z"/><path fill="#EA4335" d="M12 5.4c1.6 0 3.1.6 4.2 1.7l3.2-3.2A11 11 0 0 0 2.1 7l3.7 2.9C6.7 7.3 9.1 5.4 12 5.4z"/>';

export function icon(name) {
  if (name === "google") return `<svg viewBox="0 0 24 24" aria-hidden="true">${G}</svg>`;
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${P[name] || P.tool}</svg>`;
}

export function toolIcon(name) {
  const n = String(name || "").toLowerCase();
  if (/bash|shell|exec|command|run/.test(n)) return "terminal";
  if (/read|view|cat|open/.test(n)) return "file";
  if (/grep|glob|search|find|ls/.test(n)) return "search";
  if (/edit|write|patch|apply|multiedit|notebook/.test(n)) return "edit";
  if (/web|fetch|browser|http/.test(n)) return "globe";
  if (/task|agent|todo/.test(n)) return "task";
  return "tool";
}

/** Build an element from an HTML string (single root). */
export function h(html) {
  const t = document.createElement("template");
  t.innerHTML = html.trim();
  return t.content.firstElementChild;
}

/** Set textContent only when it changed (keeps layout stable during polls). */
export function setText(el, text) {
  const v = text ?? "";
  if (el && el.textContent !== v) el.textContent = v;
}

/**
 * Keyed reconcile: reuse children by key, create missing, remove stale, keep order.
 * create(item) -> Element; update(el, item) mutates in place.
 */
export function reconcile(container, items, keyOf, create, update) {
  const existing = new Map();
  for (const child of Array.from(container.children)) {
    if (child.dataset.key != null) existing.set(child.dataset.key, child);
  }
  let cursor = container.firstElementChild;
  const seen = new Set();
  for (const item of items) {
    const key = String(keyOf(item));
    seen.add(key);
    let el = existing.get(key);
    if (!el) {
      el = create(item);
      el.dataset.key = key;
    }
    update(el, item);
    if (el !== cursor) container.insertBefore(el, cursor);
    else cursor = cursor.nextElementSibling;
  }
  for (const [key, el] of existing) if (!seen.has(key)) el.remove();
}
