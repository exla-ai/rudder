import test from "node:test";
import assert from "node:assert/strict";
import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const bundle = await build({
  stdin: { contents: `export * from './src/board/ui/project-files/index.tsx'; export * from './src/board/ui/project-files/render.ts'; export * from './src/board/ui/project-files/refresh.ts'; export {renderToString} from 'preact-render-to-string'; export {h} from 'preact';`, resolveDir: root },
  bundle: true, format: "esm", platform: "node", write: false, jsx: "automatic", jsxImportSource: "preact",
});
const ui = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString("base64")}`);
const scope = { projectId: "project", workspaceId: "workspace", attemptId: "attempt", ownerEpoch: 7 };
const listing = { path: "docs", entries: [], truncated: false };
const document = { path: "docs/readme.md", revision: "1".repeat(64), kind: "markdown", mime: "text/markdown", text: "# Saved" };
const signal = () => new AbortController().signal;

test("nearby assets stay in the explicit workspace and cannot become network URLs", () => {
  assert.equal(ui.nearbyPath("site/index.html", "../images/logo.png?v=2"), "images/logo.png");
  for (const value of ["../../secret", "/etc/passwd", "//server/x", "https://host/x", "javascript:alert(1)", "data:image/png,x", "..\\secret", "%2fsecret", "%5csecret", "x%00y", "x%2fy", "%E0%A4%A"]) assert.equal(ui.nearbyPath("site/index.html", value), null, value);
});

test("Markdown raw HTML stays text and supported image syntax remains scoped", () => {
  const rendered = ui.markdownHtml('# Read me\n<script>alert("x")</script>\n**Hello** `world`\n![logo](../logo.png)\n[unsafe](javascript:alert(1))');
  assert.match(rendered, /<h1>Read me<\/h1>/);
  assert.match(rendered, /&lt;script&gt;/);
  assert.doesNotMatch(rendered, /<script|href=/);
  assert.match(rendered, /<strong>Hello<\/strong>/);
  assert.match(rendered, /<img alt="logo" src="\.\.\/logo.png">/);
});

test("HTML frame has an opaque sandbox and source text cannot escape its attribute", () => {
  const rendered = ui.renderToString(ui.h(ui.DocumentFrame, { title: "site", source: '<img src=x onerror="parent.alert(1)"><script>parent.document.cookie</script>' }));
  assert.match(rendered, /sandbox(?:="")?\s/);
  assert.match(rendered, /referrerpolicy="no-referrer"/i);
  assert.doesNotMatch(rendered, /allow-scripts|allow-same-origin|<script|<img/);
  assert.match(ui.VIEWER_CSP, /connect-src 'none'/);
  assert.match(ui.VIEWER_CSP, /script-src 'none'/);
});

test("source rendering is escaped and image payloads enforce a non-SVG allowlist", () => {
  const rendered = ui.renderToString(ui.h(ui.FilePreview, { document: { ...document, text: "<script>oops</script>" }, source: true, scope, transport: {} }));
  assert.match(rendered, /&lt;script(?:&gt;|>)/);
  assert.equal(ui.imageData({ mime: "image/svg+xml", base64: "PHN2Zz4=" }), null);
  assert.equal(ui.imageData({ mime: "image/png", base64: 'x" onerror="x' }), null);
  assert.equal(ui.imageData({ mime: "image/png", base64: "cG5n" }), "data:image/png;base64,cG5n");
});

test("native PDFs use a signature-checked opaque data origin without disabling browser PDF support", () => {
  const pdf = { path: "report.pdf", revision: "a".repeat(64), kind: "pdf", mime: "application/pdf", base64: Buffer.from("%PDF-1.7\n%%EOF").toString("base64") };
  const rendered = ui.renderToString(ui.h(ui.NativePdfFrame, { document: pdf }));
  assert.match(rendered, /src="data:application\/pdf;base64,/);
  assert.match(rendered, /referrerpolicy="no-referrer"/i);
  assert.doesNotMatch(rendered, /sandbox|blob:|srcdoc=/);
  assert.equal(ui.pdfData(pdf), `data:application/pdf;base64,${pdf.base64}`);
  // A grouped quartet repetition can exhaust the regexp stack on normal PDFs.
  const large = Buffer.alloc(2 * 1024 * 1024, 32);
  large.write("%PDF-1.7\n");
  const largePdf = { ...pdf, base64: large.toString("base64") };
  assert.equal(ui.pdfData(largePdf), `data:application/pdf;base64,${largePdf.base64}`);
  for (const malformed of [
    { ...pdf, mime: "text/html" }, { ...pdf, kind: "html" },
    { ...pdf, base64: Buffer.from("<html>evil</html>").toString("base64") },
    { ...pdf, base64: `${pdf.base64}\n` }, { ...pdf, base64: `${pdf.base64.slice(0, -1)}!` },
    { ...pdf, base64: "JVBERi0=".repeat(4 * 1024 * 1024) },
  ]) assert.equal(ui.pdfData(malformed), null);
  assert.match(ui.renderToString(ui.h(ui.NativePdfFrame, { document: { ...pdf, base64: "aW52YWxpZA==" } })), /role="alert"/);
});

test("transport sends complete identity through the injected authorized private path", async () => {
  let request;
  const transport = ui.createProjectFileTransport("/api/project-files", async (url, init) => {
    request = { url, init }; return Response.json(document);
  });
  assert.deepEqual(await transport.read(scope, document.path, signal()), document);
  const query = new URL(request.url, "https://private.invalid").searchParams;
  for (const [key, value] of Object.entries(scope)) assert.equal(query.get(key), String(value));
  assert.equal(query.get("path"), document.path);
  assert.equal(query.get("version"), "1");
  assert.equal(request.init.credentials, "omit");
  assert.equal(request.init.redirect, "error");
  assert.throws(() => ui.createProjectFileTransport("https://outside.invalid/files", () => {}));
});

test("transport rejects wrong identities, malformed listings, binary abuse and revoked access", async () => {
  for (const response of [
    { ...document, path: "other/readme.md" },
    { ...document, kind: "iframe" },
    { ...document, base64: 'x"boom' },
    { ...document, revision: "stale" },
  ]) {
    const transport = ui.createProjectFileTransport("/files", async () => Response.json(response));
    await assert.rejects(transport.read(scope, document.path, signal()));
  }
  const badListing = ui.createProjectFileTransport("/files", async () => Response.json({ ...listing, entries: [{ path: "other/secret", name: "secret", kind: "file" }] }));
  await assert.rejects(badListing.list(scope, "docs", signal()));
  const revoked = ui.createProjectFileTransport("/files", async () => Response.json({ error: "Access expired" }, { status: 403 }));
  await assert.rejects(revoked.read(scope, document.path, signal()), /Access expired/);
  const huge = ui.createProjectFileTransport("/files", async () => new Response("{}", { headers: { "content-length": String(25 * 1024 * 1024) } }));
  await assert.rejects(huge.read(scope, document.path, signal()), /too large/);
  let fetched = false;
  const scoped = ui.createProjectFileTransport("/files", async () => { fetched = true; return Response.json(document); });
  for (const projectId of ["x".repeat(257), "project\nother", "project\u007f"]) await assert.rejects(scoped.read({ ...scope, projectId }, document.path, signal()), /identity/);
  assert.equal(fetched, false);
});

test("transport cancellation bounds an injected fetch that ignores its signal", async () => {
  const controller = new AbortController();
  const transport = ui.createProjectFileTransport("/files", () => new Promise(() => {}));
  const pending = transport.read(scope, document.path, controller.signal);
  controller.abort(new Error("Cancelled selection"));
  await assert.rejects(pending, /Cancelled selection/);
});

test("saved changes are observed without overlapping refresh cycles", async () => {
  let revision = 0;
  let active = 0;
  let peak = 0;
  const seen = [];
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => { stop(); reject(new Error("Refresh did not observe saved changes")); }, 2000);
    const stop = ui.refreshFiles({ scope, directory: "docs", path: document.path, intervalMs: 250,
      transport: { list: async () => listing, read: async () => { active++; peak = Math.max(peak, active); await new Promise((r) => setTimeout(r, 5)); active--; return { ...document, revision: String(++revision) }; } },
      onSnapshot: (snapshot) => { seen.push(snapshot.document.revision); if (seen.length === 2) { stop(); clearTimeout(timeout); resolve(); } }, onError: reject,
    });
  });
  assert.deepEqual(seen, ["1", "2"]);
  assert.equal(peak, 1);
});

test("navigation cancels old responses and timed out requests stop polling", async () => {
  let oldResolve;
  let callbacks = 0;
  let requestSignal;
  const stop = ui.refreshFiles({ scope, directory: "docs", path: document.path,
    transport: { list: async () => listing, read: (_scope, _path, signal) => { requestSignal = signal; return new Promise((resolve) => { oldResolve = resolve; }); } },
    onSnapshot: () => callbacks++, onError: () => callbacks++,
  });
  stop(); oldResolve(document);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(requestSignal.aborted, true);
  assert.equal(callbacks, 0);
  let reads = 0;
  await new Promise((resolve, reject) => {
    const stopTimeout = ui.refreshFiles({ scope, directory: "docs", path: document.path, timeoutMs: 10, intervalMs: 250,
      transport: { list: async () => listing, read: async () => { reads++; return new Promise(() => {}); } },
      onSnapshot: () => reject(new Error("Unexpected snapshot")), onError: () => { stopTimeout(); resolve(); },
    });
  });
  assert.equal(reads, 1);
});
