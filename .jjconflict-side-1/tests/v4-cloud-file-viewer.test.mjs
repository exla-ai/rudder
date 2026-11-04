import test from "node:test";
import assert from "node:assert/strict";
import { build } from "esbuild";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const rendered = await build({ entryPoints: [path.join(root, "cloud/app/project-files.js")], bundle: true,
  format: "esm", platform: "browser", write: false,
  banner: { js: "const window=globalThis.__cloudViewer.window, DOMParser=globalThis.__cloudViewer.DOMParser, Date={now:()=>globalThis.__cloudViewer.clock.now}, setTimeout=(...args)=>globalThis.__cloudViewer.clock.set(...args), clearTimeout=(id)=>globalThis.__cloudViewer.clock.clear(id);" },
  plugins: [{ name: "browser-fixture", setup(builder) {
    builder.onResolve({ filter: /^\/app\// }, args => ({ path: args.path, namespace: "fixture" }));
    builder.onLoad({ filter: /.*/, namespace: "fixture" }, async args => {
      if (args.path === "/app/core.js") return { contents: "export const api=(...args)=>globalThis.__cloudViewer.api(...args);" };
      if (args.path === "/app/render.js") return { contents: "export const h=()=>globalThis.__cloudViewer.details();" };
      // Compile the actual shared renderer, including its asset confinement and
      // sanitizer. This fixture replaces DOM primitives, never renderer logic.
      if (args.path === "/app/project-file-render.js") return { contents: await readFile(path.join(root, "cloud/src/project-files/browser-render.ts"), "utf8"), loader: "ts" };
      throw new Error("Unexpected application module");
    });
  } }],
});
let serial = 0;
const escape = value => String(value).replaceAll("&", "&amp;").replaceAll('"', "&quot;").replaceAll("<", "&lt;");
class Element {
  constructor(tag) { this.tagName = tag.toUpperCase(); this.children = []; this.attrs = new Map(); this.listeners = new Map(); this.style = {}; this.scrollTop = 0; this._text = ""; }
  setAttribute(name, value) { this.attrs.set(name, value); }
  getAttribute(name) { return this.attrs.get(name) ?? null; }
  removeAttribute(name) { this.attrs.delete(name); }
  get attributes() { return Array.from(this.attrs, ([name, value]) => ({ name, value })); }
  append(...nodes) { for (let node of nodes) { if (typeof node === "string") { const text = new Element("#text"); text._text = node; node = text; } node.parent = this; this.children.push(node); } }
  replaceChildren(...nodes) { for (const child of this.children) child.parent = null; this.children = []; this._text = ""; this.append(...nodes); }
  get textContent() { return this._text + this.children.map(child => child.textContent).join(""); }
  set textContent(value) { this.replaceChildren(); this._text = String(value); }
  addEventListener(name, run) { this.listeners.set(name, run); }
  async fire(name) { this.listeners.get(name)?.(); await settle(); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter(child => child !== this); this.parent = null; }
  replaceWith(node) { const parent = this.parent; if (!parent) return; parent.children.splice(parent.children.indexOf(this), 1, node); node.parent = parent; this.parent = null; }
  querySelectorAll(selector) {
    const matches = selector.split(","); const found = [];
    const visit = node => { for (const child of node.children) { if (matches.includes("*") || matches.includes(child.tagName.toLowerCase()) || matches.some(item => item[0] === "." && child.getAttribute("class")?.split(" ").includes(item.slice(1)))) found.push(child); visit(child); } };
    visit(this); return found;
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
  get outerHTML() {
    if (this.tagName === "#TEXT") return this._text;
    const attrs = Array.from(this.attrs, ([name, value]) => ` ${name}="${escape(value)}"`).join("");
    return `<${this.tagName.toLowerCase()}${attrs}>${this._text}${this.children.map(child => child.outerHTML).join("")}${["IMG", "LINK", "META", "BR"].includes(this.tagName) ? "" : `</${this.tagName.toLowerCase()}>`}`;
  }
}
// A small inert DOM fixture for the template operations the shared renderer
// uses. It performs no navigation, networking, rendering or sanitization.
class Template extends Element {
  constructor() { super("template"); this.content = new Element("fragment"); }
  set innerHTML(value) {
    this.content.replaceChildren(); const stack = [this.content];
    for (const token of value.match(/<[^>]*>|[^<]+/g) ?? []) {
      if (token.startsWith("</")) { if (stack.length > 1) stack.pop(); continue; }
      const tag = /^<([a-z][\w-]*)\b([^>]*)>/i.exec(token);
      if (!tag) { stack.at(-1).append(token); continue; }
      const node = new Element(tag[1]);
      for (const match of tag[2].matchAll(/([\w-]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) node.setAttribute(match[1], match[2] ?? match[3] ?? match[4] ?? "");
      stack.at(-1).append(node);
      if (!["img", "link", "meta", "br"].includes(tag[1].toLowerCase())) stack.push(node);
    }
  }
  get innerHTML() { return this.content.children.map(child => child.outerHTML).join(""); }
}
class Clock {
  constructor() { this.now = 0; this.tasks = new Map(); this.id = 0; }
  set(run, delay) { const id = ++this.id; this.tasks.set(id, { at: this.now + delay, run }); return id; }
  clear(id) { this.tasks.delete(id); }
  async advance(ms) {
    const target = this.now + ms;
    for (;;) {
      const next = Array.from(this.tasks).filter(([, item]) => item.at <= target).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break; this.now = next[1].at; this.tasks.delete(next[0]); next[1].run(); await settle();
    }
    this.now = target; await settle();
  }
}
async function settle() { for (let count = 0; count < 100; count++) await Promise.resolve(); }
const scope = { projectId: "project", workspaceId: "actual-execution", attemptId: "attempt", ownerEpoch: 7 };
const document = (filePath, kind, text, extra = {}) => ({ path: filePath, kind, text, mime: kind === "html" ? "text/html" : "text/plain", revision: "a".repeat(64), ...extra });
async function fixture(t, handler) {
  const clock = new Clock(); const calls = [];
  const dom = { createElement: tag => tag === "template" ? new Template() : new Element(tag) };
  const state = { clock, calls, window: { document: dom }, DOMParser: class { parseFromString() { return dom; } },
    details() { const el = new Element("details"); for (const name of ["file-workspaces", "file-actions", "file-result"]) { const node = new Element("div"); node.setAttribute("class", name); el.append(node); } return el; },
    async api(input, init) { const url = new URL(input, "https://cloud.invalid"); const call = { url, signal: init.signal, at: clock.now }; calls.push(call); return handler(call); },
  };
  globalThis.__cloudViewer = state;
  const module = await import(`data:text/javascript;base64,${Buffer.from(rendered.outputFiles[0].text).toString("base64")}#${++serial}`);
  const panel = module.ProjectFiles("repository");
  t.after(() => { panel.destroy(); delete globalThis.__cloudViewer; });
  const section = name => panel.el.querySelector(`.${name}`);
  const click = async (name, area = "file-actions") => { const button = section(area).children.find(node => node.textContent === name); assert.ok(button, name); await button.fire("click"); };
  await panel.el.fire("toggle"); assert.equal(calls.length, 0, "closed panels do not read");
  panel.el.open = true; await panel.el.fire("toggle");
  return { panel, state, clock, calls, section, click, select: () => click(scope.workspaceId, "file-workspaces") };
}

test("Cloud controller refreshes nearby HTML assets and retains unchanged frames and selection", async t => {
  let image = "cG5n", css = "body{color:red}";
  const page = document("site/index.html", "html", '<h1>Saved</h1><script>parent.bad()</script><link rel="stylesheet" href="theme.css"><img src="logo.png" onerror="bad()"><img src="https://outside.invalid/track">');
  const f = await fixture(t, ({ url }) => {
    if (url.pathname.endsWith("project-file-scopes")) return { scopes: [scope] };
    const p = url.searchParams.get("path");
    if (url.searchParams.get("operation") === "list") return { path: p, entries: [{ name: "site", path: "site", kind: "folder" }], truncated: false };
    if (p === page.path) return page;
    if (p === "site/theme.css") return document(p, "text", css, { mime: "text/css" });
    if (p === "site/logo.png") return document(p, "image", undefined, { mime: "image/png", base64: image });
    assert.fail(`unexpected asset ${p}`);
  });
  await f.select();
  // Folder navigation uses actual listing data; select a file from its folder.
  const folderCall = f.state.api;
  f.state.api = async (url, init) => {
    const query = new URL(url, "https://cloud.invalid").searchParams;
    if (query.get("operation") === "list" && query.get("path") === "site") {
      f.calls.push({ url: new URL(url, "https://cloud.invalid"), signal: init.signal, at: f.clock.now });
      return { path: "site", entries: [{ name: "index.html", path: page.path, kind: "file" }], truncated: false };
    }
    return folderCall(url, init);
  };
  await f.click("Folder: site", "file-result"); await f.click("index.html", "file-result");
  let frame = f.section("file-result").children[0];
  assert.equal(frame.tagName, "IFRAME"); assert.equal(frame.getAttribute("sandbox"), "");
  assert.match(frame.srcdoc, /data:image\/png;base64,cG5n/); assert.match(frame.srcdoc, /body\{color:red\}/);
  assert.doesNotMatch(frame.srcdoc, /parent\.bad|onerror|https:\/\/outside/);
  frame.selectedText = "Saved"; f.section("file-result").scrollTop = 125;
  await f.clock.advance(2000);
  assert.equal(f.section("file-result").children[0], frame); assert.equal(frame.selectedText, "Saved"); assert.equal(f.section("file-result").scrollTop, 125);
  image = "bmV3"; css = "body{color:blue}"; await f.clock.advance(2000);
  const updated = f.section("file-result").children[0]; assert.notEqual(updated, frame);
  assert.match(updated.srcdoc, /base64,bmV3/); assert.match(updated.srcdoc, /color:blue/);
  for (const call of f.calls.filter(call => call.url.pathname.endsWith("project-files"))) {
    for (const [key, value] of Object.entries(scope)) assert.equal(call.url.searchParams.get(key), String(value));
  }
  await f.clock.advance(36000);
  assert.ok(f.calls.filter(call => call.url.pathname.endsWith("project-file-scopes")).length >= 2, "binding renewed before 60s expiry");
  assert.ok(!f.calls.some(call => call.url.hostname !== "cloud.invalid"));
});

test("Cloud navigation and close cancel stale reads, bound stalled work and stop polling", async t => {
  let release, slowSignal, mode = "slow", reads = 0;
  const f = await fixture(t, ({ url, signal }) => {
    if (url.pathname.endsWith("project-file-scopes")) return { scopes: [scope] };
    const p = url.searchParams.get("path");
    if (url.searchParams.get("operation") === "list") return { path: p, entries: [{ name: "note.txt", path: "note.txt", kind: "file" }], truncated: false };
    reads++; slowSignal = signal;
    if (mode === "slow") return new Promise(resolve => { release = resolve; });
    return document(p, "text", "Current");
  });
  await f.select(); await f.click("note.txt", "file-result");
  assert.equal(reads, 1);
  const stale = release, navigationSignal = slowSignal;
  await f.select(); assert.equal(navigationSignal.aborted, true);
  stale(document("note.txt", "text", "Stale navigation")); await settle();
  assert.equal(f.section("file-result").textContent, "note.txt");
  await f.click("note.txt", "file-result"); assert.equal(reads, 2);
  f.panel.el.open = false; await f.panel.el.fire("toggle"); assert.equal(slowSignal.aborted, true);
  release(document("note.txt", "text", "Stale")); await settle();
  assert.notEqual(f.section("file-result").textContent, "Stale");
  const before = f.calls.length; await f.clock.advance(20000); assert.equal(f.calls.length, before);
  mode = "ready"; f.panel.el.open = true; await f.panel.el.fire("toggle");
  assert.equal(f.section("file-result").textContent, "Current");
  mode = "slow"; await f.clock.advance(2000); await f.clock.advance(10000);
  assert.equal(slowSignal.aborted, true); assert.match(f.section("file-result").textContent, /timed out/);
  const stopped = f.calls.length; await f.clock.advance(20000); assert.equal(f.calls.length, stopped);
  f.panel.destroy(); assert.equal(f.clock.tasks.size, 0);
});

test("Cloud scope renewal refuses an omitted attempt without synthesizing replacement grants", async t => {
  let allowed = true;
  const f = await fixture(t, ({ url }) => {
    if (url.pathname.endsWith("project-file-scopes")) return { scopes: allowed ? [scope] : [{ ...scope, ownerEpoch: 8, attemptId: "new-attempt" }] };
    return { path: "", entries: [], truncated: false };
  });
  await f.select(); allowed = false; await f.clock.advance(40000);
  assert.match(f.section("file-result").textContent, /access changed/);
  const fileCalls = f.calls.filter(call => call.url.pathname.endsWith("project-files"));
  assert.ok(fileCalls.every(call => call.url.searchParams.get("attemptId") === "attempt"));
  assert.equal(f.calls.filter(call => call.url.pathname.endsWith("project-file-scopes")).at(-1).at, 40000);
  const before = f.calls.length; await f.clock.advance(20000); assert.equal(f.calls.length, before);
  await f.click(scope.workspaceId, "file-workspaces");
  assert.equal(f.calls.at(-1).url.searchParams.get("attemptId"), "new-attempt", "only explicit selection uses the newly observed scope");
});

test("Cloud document controls use shared Markdown, source, image and opaque PDF renderers", async t => {
  const docs = [
    document("readme.md", "markdown", "# Saved\n**Text**", { mime: "text/markdown" }),
    document("logo.png", "image", undefined, { mime: "image/png", base64: "cG5n" }),
    document("report.pdf", "pdf", undefined, { mime: "application/pdf", base64: Buffer.from("%PDF-1.7\n%%EOF").toString("base64") }),
  ];
  const f = await fixture(t, ({ url }) => {
    if (url.pathname.endsWith("project-file-scopes")) return { scopes: [scope] };
    if (url.searchParams.get("operation") === "list") return { path: "", entries: docs.map(doc => ({ name: doc.path, path: doc.path, kind: "file" })), truncated: false };
    return docs.find(doc => doc.path === url.searchParams.get("path"));
  });
  await f.select(); await f.click("readme.md", "file-result");
  assert.match(f.section("file-result").children[0].srcdoc, /<h1>Saved<\/h1>/);
  await f.click("Rendered / source");
  const source = f.section("file-result").children[0]; assert.equal(source.tagName, "PRE"); assert.equal(source.textContent, docs[0].text);
  await f.clock.advance(2000); assert.equal(f.section("file-result").children[0], source, "source mode survives refresh");
  await f.click("Up"); await f.click("logo.png", "file-result");
  const image = f.section("file-result").children[0]; assert.equal(image.tagName, "IMG"); assert.equal(image.src, "data:image/png;base64,cG5n");
  await f.click("Up"); await f.click("report.pdf", "file-result");
  const pdf = f.section("file-result").children[0]; assert.equal(pdf.tagName, "IFRAME"); assert.match(pdf.src, /^data:application\/pdf;base64,/);
  assert.equal(pdf.getAttribute("sandbox"), null); assert.equal(pdf.referrerPolicy, "no-referrer");
});
