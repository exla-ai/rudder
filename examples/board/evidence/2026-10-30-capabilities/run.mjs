import fs from 'node:fs/promises';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { pathToFileURL, fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const base = path.dirname(fileURLToPath(import.meta.url));
const root = '/workspace/code/exla-ai-rudder';
const runtime = path.join(base, 'runtime'), fixture = path.join(base, 'fixture'), proof = path.join(base, 'proof');
await fs.mkdir(path.join(runtime, 'board'), { recursive: true, mode: 0o700 });
await fs.mkdir(proof, { recursive: true, mode: 0o700 });
await fs.symlink(path.join(root, 'node_modules'), path.join(runtime, 'node_modules'));
const esbuild = await import(pathToFileURL(path.join(root, 'node_modules/esbuild/lib/main.js')));
const entry = path.join(runtime, 'entry.mjs');
await fs.writeFile(entry, [
  `export { startBoardDaemon } from '${root}/src/board/daemon.ts';`,
  `export { requestBoardBootstrap } from '${root}/src/board/host-access.ts';`,
  `export { registerProject } from '${root}/src/state.ts';`,
  `export { RudderBus } from '${root}/src/bus.ts';`,
].join('\n'));
const built = await esbuild.build({ entryPoints: [entry], outfile: path.join(runtime, 'board/daemon.mjs'), bundle: true,
  platform: 'node', packages: 'external', format: 'esm', metafile: true, logLevel: 'silent' });
await esbuild.build({ entryPoints: [path.join(root, 'src/board/ui/index.tsx')], outfile: path.join(runtime, 'board/board.js'),
  bundle: true, minify: true, format: 'esm', jsx: 'automatic', jsxImportSource: 'preact', logLevel: 'silent' });
await esbuild.build({ entryPoints: [path.join(root, 'src/board/ui/board.css')], outfile: path.join(runtime, 'board/board.css'),
  bundle: true, minify: true, logLevel: 'silent' });
const hash = async (file) => createHash('sha256').update(await fs.readFile(file)).digest('hex');
const inputs = [];
for (const name of Object.keys(built.metafile.inputs).sort()) {
  const resolved = path.resolve(name);
  assert.ok(resolved === entry || resolved.startsWith(root + '/src/'));
  if (resolved !== entry) assert.ok(!path.relative(root, resolved).split(path.sep).some((part) => part.startsWith('.')));
  const stat = await fs.lstat(resolved); assert.ok(stat.isFile() && !stat.isSymbolicLink());
  inputs.push({ path: resolved === entry ? 'private/entry.mjs' : path.relative(root, resolved), sha256: await hash(resolved) });
}
await fs.writeFile(path.join(proof, 'inputs.json'), JSON.stringify({ format_version: 1, sources: inputs,
  products: await Promise.all(['daemon.mjs','board.js','board.css'].map(async (file) => ({ path: file, sha256: await hash(path.join(runtime, 'board', file)) }))) }, null, 2) + '\n');

let stage = 'launch', browser, board, next, activePage, externalFetches = 0, blockedBrowserNetwork = 0;
const result = { format_version: 1, kind: 'local-headless-board-capability', started_at: new Date().toISOString(),
  playwright: '1.64.1', source_manifest_sha256: 'b60292c260e1ff5add196567ffaa96eaad4addf2835c401a3fbc47f5161fd7e6',
  checks: {}, limits: ['Private staged server/UI bundles; no dist imports', 'Synthetic fixture/provider usage only; no real accounts or global session logs',
    'Loopback browser qualification only; no managed guest/network namespace or Cloud proof'] };
const transport = [], responses = [], errors = { page: 0, csp: 0 };
const originalFetch = globalThis.fetch;
globalThis.fetch = (input, init) => {
  const url = new URL(typeof input === 'string' || input instanceof URL ? input : input.url);
  if (url.hostname !== '127.0.0.1') { externalFetches++; throw new Error('External fixture fetch blocked'); }
  return originalFetch(input, init);
};
const poll = async (predicate) => {
  const until = Date.now() + 15000;
  while (!await predicate()) { if (Date.now() > until) throw new Error('Fixture condition timed out'); await new Promise((resolve) => setTimeout(resolve, 25)); }
};
try {
  stage = 'load browser tooling';
  const { chromium } = await import(pathToFileURL(path.join(base, 'tooling/node_modules/playwright/index.mjs')));
  stage = 'load staged server';
  const { startBoardDaemon, requestBoardBootstrap, registerProject, RudderBus } = await import(pathToFileURL(path.join(runtime, 'board/daemon.mjs')));
  const repo = path.join(fixture, 'repo');
  for (const directory of [repo, process.env.HOME, process.env.RUDDER_HOME, process.env.CLAUDE_CONFIG_DIR, process.env.CODEX_HOME]) await fs.mkdir(directory, { recursive: true, mode: 0o700 });
  stage = 'create fixture project';
  const project = await registerProject(repo);
  const runDirectory = path.join(repo, '.rudder/runs/browser-peer'); await fs.mkdir(runDirectory, { recursive: true });
  const run = { id: 'browser-peer', task: 'Browser fixture task', taskSummary: 'Browser fixture task', taskSummaryLlm: true,
    repoRoot: repo, backend: 'codex', status: 'completed', vcs: 'jj', createdAt: new Date().toISOString(), updatedAt: new Date().toISOString(),
    targetBranch: 'main', baseCommit: 'fixture', workspace: { enabled: false, path: repo }, autoSteer: { count: 0, max: 0 } };
  await fs.writeFile(path.join(runDirectory, 'run.json'), JSON.stringify(run));
  await fs.writeFile(path.join(runDirectory, 'output.txt'), 'Browser fixture output <img src=x onerror="window.__fixtureXss=1">\n');
  const synthetic = path.join(process.env.CLAUDE_CONFIG_DIR, 'projects/fixture'); await fs.mkdir(synthetic, { recursive: true });
  await fs.writeFile(path.join(synthetic, 'fixture.jsonl'), JSON.stringify({ type: 'assistant', timestamp: new Date().toISOString(), cwd: repo,
    requestId: 'fixture-request', sessionId: 'fixture-session', message: { id: 'fixture-message', model: 'claude-sonnet-4-5-20250929',
      usage: { input_tokens: 1000, output_tokens: 100, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 } } }) + '\n');
  stage = 'start staged board';
  const bus = new RudderBus(); board = await startBoardDaemon({ port: 0, repoRoot: repo, controlMode: 'projector', bus });
  stage = 'launch browser';
  browser = await chromium.launch({ headless: true }); result.chromium = browser.version();
  const context = await browser.newContext({ viewport: { width: 1280, height: 850 } });
  await context.route('**/*', async (route) => {
    const url = new URL(route.request().url());
    if (url.origin !== board.url) { blockedBrowserNetwork++; await route.abort(); } else await route.continue();
  });
  context.on('request', (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith('/api/')) transport.push({ method: request.method(), path: url.pathname,
      bearer: /^Bearer [a-f0-9]{64}$/.test(request.headers()['authorization'] ?? ''), fragment_absent: !url.hash,
      credential_query_absent: !Array.from(url.searchParams.keys()).some((key) => /token|ticket|auth|session/i.test(key)) && !/[a-f0-9]{64}/.test(url.search) });
  });
  context.on('response', (response) => { const url = new URL(response.url()); if (url.pathname.startsWith('/api/')) responses.push({ path: url.pathname, status: response.status() }); });
  const page = await context.newPage(); activePage = page; page.setDefaultTimeout(8000);
  page.on('pageerror', () => errors.page++);
  page.on('console', (message) => { if (message.type() === 'error' && message.text().includes('Content Security Policy')) errors.csp++; });
  await page.addInitScript(() => {
    const prior = window.fetch;
    window.__fixtureEarlyErase = [];
    window.fetch = function(input, init) {
      const url = new URL(typeof input === 'string' ? input : input.url, location.origin);
      if (url.pathname === '/api/board/descriptor') window.__fixtureEarlyErase.push(location.hash === '');
      return prior.call(this, input, init);
    };
  });

  stage = 'anonymous browser';
  const anonymous = await browser.newContext(); const deniedPage = await anonymous.newPage();
  await deniedPage.goto(`${board.url}/rudder/${project.slug}`);
  await deniedPage.getByRole('alert').waitFor();
  const statuses = [];
  for (const suffix of [`/api/projects/${project.slug}/state`, `/api/projects/${project.slug}/tasks/browser-peer/log`,
    `/api/projects/${project.slug}/events`, '/api/projects', '/api/usage/tokens', '/api/usage/quota', '/api/usage/machine']) {
    const response = await anonymous.request.get(board.url + suffix); statuses.push(response.status()); await response.dispose();
  }
  assert.ok(statuses.every((status) => status === 401));
  assert.equal(await deniedPage.evaluate(() => sessionStorage.length), 0);
  result.checks.anonymous_context = { private_api_statuses: statuses, neutral_shell: true, private_storage_absent: true };
  await anonymous.close();

  stage = 'private bootstrap and project';
  const bootstrap = await requestBoardBootstrap(repo);
  await page.goto(bootstrap);
  await page.locator('[data-task-id="browser-peer"]').waitFor();
  assert.equal(await page.evaluate(() => location.hash), '');
  assert.ok(await page.evaluate(() => window.__fixtureEarlyErase.length > 0 && window.__fixtureEarlyErase.every(Boolean)));
  await poll(() => responses.some((reply) => reply.path.endsWith('/events') && reply.status === 200));
  await page.locator('[data-task-id="browser-peer"]').click();
  await page.getByText('Browser fixture output', { exact: false }).first().waitFor();
  assert.equal(await page.locator('img[src="x"]').count(), 0);
  assert.equal(await page.evaluate(() => window.__fixtureXss === undefined), true);
  const sessionSummary = await page.evaluate(() => {
    const keys = Object.keys(sessionStorage).filter((key) => key.startsWith('rudder.board.session.v1:'));
    return { count: keys.length, scoped: keys.every((key) => key.startsWith(`rudder.board.session.v1:${location.origin}:`)),
      valid: keys.every((key) => /^[a-f0-9]{64}$/.test(JSON.parse(sessionStorage.getItem(key)).token)) };
  });
  assert.deepEqual(sessionSummary, { count: 1, scoped: true, valid: true });
  assert.equal((await context.cookies()).length, 0);
  const replay = await context.request.post(`${board.url}/api/board/session`, { data: { instance_id: board.instanceId, ticket: new URL(bootstrap).hash.slice('#rudder-board-ticket='.length) } });
  assert.equal(replay.status(), 401); await replay.dispose();
  result.checks.bootstrap = { one_use: true, fragment_erased_before_first_fetch: true, session_storage: sessionSummary, cookies: 0,
    project_rendered: true, log_rendered_as_text: true, script_injection_absent: true };
  await page.press('body', 'Escape');
  await page.screenshot({ path: path.join(proof, 'project.png') });

  stage = 'authenticated SSE';
  run.taskSummary = 'Browser fixture updated through SSE'; run.updatedAt = new Date().toISOString();
  await fs.writeFile(path.join(runDirectory, 'run.json'), JSON.stringify(run)); bus.publish({ type: 'run.updated', repoRoot: repo, runId: run.id });
  await page.getByText(run.taskSummary, { exact: true }).first().waitFor();
  assert.ok(transport.every((request) => request.credential_query_absent));
  result.checks.sse = { authenticated_fetch: transport.some((request) => request.path.endsWith('/events') && request.bearer), live_update: true, query_credentials_absent: true };

  stage = 'sessionStorage reload';
  const exchangesBefore = transport.filter((request) => request.path === '/api/board/session' && request.method === 'POST').length;
  await page.reload(); await page.getByText(run.taskSummary, { exact: true }).first().waitFor();
  assert.equal(transport.filter((request) => request.path === '/api/board/session' && request.method === 'POST').length, exchangesBefore);
  result.checks.reload = { same_session_reused: true, no_second_ticket_exchange: true };

  stage = 'authorized usage';
  const usageReply = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/usage/tokens' && response.status() === 200);
  await page.locator('a[href="/usage"]').first().click();
  await page.getByRole('button', { name: 'Token usage', exact: true }).waitFor();
  const report = await (await usageReply).json(); assert.equal(report.scannedFiles, 1); assert.equal(report.total.calls, 1);
  assert.ok(transport.some((request) => request.path === '/api/usage/tokens' && request.bearer));
  result.checks.usage = { authenticated: true, synthetic_scanned_files: report.scannedFiles, synthetic_calls: report.total.calls };
  await page.screenshot({ path: path.join(proof, 'usage.png') });

  stage = 'same-port restart';
  const oldInstance = board.instanceId; const port = board.port;
  await board.close(); next = await startBoardDaemon({ port, repoRoot: repo, controlMode: 'projector' });
  assert.notEqual(next.instanceId, oldInstance);
  stage = 'restart stale-session reload';
  await page.reload(); await page.getByRole('alert').waitFor();
  assert.equal(await page.evaluate(() => Object.keys(sessionStorage).filter((key) => key.startsWith('rudder.board.session.v1:')).length), 0);
  result.checks.restart_stale_reload = { old_ui_closed: true, old_session_storage_cleared: true };
  stage = 'restart fresh private open';
  // The supported host opener opens a new document/tab. A fragment-only goto
  // in the already loaded unavailable page would be same-document navigation.
  const fresh = await context.newPage(); activePage = fresh; fresh.setDefaultTimeout(8000);
  fresh.on('pageerror', () => errors.page++);
  fresh.on('console', (message) => { if (message.type() === 'error' && message.text().includes('Content Security Policy')) errors.csp++; });
  await fresh.goto(await requestBoardBootstrap(repo, 'usage'));
  await fresh.getByRole('button', { name: 'Token usage', exact: true }).waitFor();
  assert.equal(await fresh.evaluate(() => location.hash), '');
  result.checks.restart = { fresh_document: true, fresh_private_open_recovers: true };
  assert.equal(errors.page, 0); assert.equal(errors.csp, 0); assert.equal(externalFetches, 0); assert.equal(blockedBrowserNetwork, 0);
  result.checks.sanitized_transport = { api_requests: transport.length,
    private_requests_all_bearer: transport.filter((request) => request.path !== '/api/board/descriptor' && !(request.path === '/api/board/session' && request.method === 'POST')).every((request) => request.bearer),
    request_fragments_absent: transport.every((request) => request.fragment_absent), page_errors: errors.page, csp_violations: errors.csp,
    credential_query_absent: transport.every((request) => request.credential_query_absent),
    external_node_fetches: externalFetches, browser_external_requests: blockedBrowserNetwork };
  assert.equal(result.checks.sanitized_transport.private_requests_all_bearer, true);
  result.ok = true;
} catch (error) {
  result.ok = false; result.failed_stage = stage; result.error_type = error?.name ?? 'Error';
  if (activePage && stage.startsWith('restart ')) result.failure_state = await activePage.evaluate(() => ({
    ticket_fragment_present: /^#rudder-board-ticket=/.test(location.hash), alert_present: Boolean(document.querySelector('[role="alert"]')),
    usage_button_present: Array.from(document.querySelectorAll('button')).some((button) => button.textContent === 'Token usage') })).catch(() => ({ page_closed: true }));
  if (['load browser tooling', 'load staged server', 'create fixture project', 'start staged board', 'launch browser'].includes(stage)) {
    result.pre_bootstrap_error = String(error?.message ?? '').replace(/https?:\/\/\S+/g, '[URL]').replace(/[a-f0-9]{64}/g, '[redacted]').slice(0, 600);
  }
  // No exception text/stack, URLs, request headers, bodies or storage values.
  process.exitCode = 1;
} finally {
  await browser?.close().catch(() => undefined);
  await next?.close().catch(() => undefined);
  await board?.close().catch(() => undefined);
  result.finished_at = new Date().toISOString();
  await fs.writeFile(path.join(proof, 'result.json'), JSON.stringify(result, null, 2) + '\n', { mode: 0o600 });
  console.log(JSON.stringify({ ok: result.ok, stage: result.failed_stage ?? 'complete', proof: path.join(proof, 'result.json'), checks: Object.keys(result.checks) }));
}
