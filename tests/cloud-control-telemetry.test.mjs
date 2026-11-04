// Control plane metrics (cloud/src/telemetry.ts, built to cloud/dist) with a
// stand-in OpenTelemetry SDK.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";

const built = new URL("../cloud/dist/telemetry.js", import.meta.url);
const lib = fs.existsSync(built) ? await import(built.href) : null;
const test = lib ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built" }, fn);

test("routes collapse ids so one route is one series", () => {
  const { normalizeRoute } = lib;
  assert.equal(normalizeRoute("/api/rudder/home/home-53c2/control"), "/api/rudder/home/:id/control");
  assert.equal(normalizeRoute("/api/rudder/workspace/ws-8f2a1c/heartbeat?x=1"), "/api/rudder/workspace/:id/heartbeat");
  assert.equal(normalizeRoute("/api/rudder/sail/3f9e2b7c-1d2a-4c55-9a0b-9d1f0e2c3b4a"), "/api/rudder/sail/:id");
  assert.equal(normalizeRoute("/api/app/state"), "/api/app/state");
  assert.equal(normalizeRoute("/app/render.js"), "/app/*");
  assert.equal(normalizeRoute("/health/"), "/health");
  assert.equal(normalizeRoute("/"), "/");
});

test("off without an endpoint", async () => {
  const telemetry = await lib.startControlTelemetry({ env: {}, snapshot: () => { throw new Error("no"); } });
  assert.equal(telemetry.enabled, false);
  telemetry.recordRequest("GET", "/", 200, 0.01);
  telemetry.recordHomeDisconnect("home-1");
});

test("requests, disconnects and the snapshot reach the catalog instruments", async () => {
  const calls = { histograms: {}, counters: {}, callback: null, resource: null };
  const sdk = {
    resourceFromAttributes: (attrs) => { calls.resource = attrs; return attrs; },
    OTLPMetricExporter: class { constructor(options) { calls.exporter = options; } },
    PeriodicExportingMetricReader: class { async forceFlush() {} },
    MeterProvider: class {
      getMeter() {
        return {
          createHistogram: (name, options) => {
            calls.histogramOptions = options;
            return { record: (value, attrs) => (calls.histograms[name] ??= []).push([value, attrs]) };
          },
          createCounter: (name) => ({ add: (value, attrs) => (calls.counters[name] ??= []).push([value, attrs]) }),
          createObservableGauge: (name, options) => ({ name, ...options }),
          addBatchObservableCallback: (callback) => { calls.callback = callback; },
        };
      }
      async shutdown() {}
    },
  };
  const telemetry = await lib.startControlTelemetry({
    env: { RUDDER_OTEL_ENDPOINT: "http://collector:4318" },
    version: "deployment-01",
    sdk,
    snapshot: () => ({
      homes: { running: 2, stopped: 1 },
      sails: { completed: 4 },
      perHome: [
        { homeId: "home-53c2", state: "running", heartbeatAgeSeconds: 12, controlConnected: true, clients: 3 },
        { homeId: "home-9999", state: "stopped", heartbeatAgeSeconds: null, controlConnected: false, clients: 0 },
      ],
      ws: { worker: 2, client: 5, homeControl: 1 },
    }),
  });
  assert.equal(calls.resource["service.name"], "rudder-control");
  assert.equal(calls.exporter.url, "http://collector:4318/v1/metrics");
  assert.ok(calls.histogramOptions.advice.explicitBucketBoundaries.includes(0.1));

  telemetry.recordRequest("get", "/api/rudder/home/home-53c2/control", 101, 0.004);
  telemetry.recordRequest("BREW", "/x", 418, 0.5);
  assert.deepEqual(calls.histograms["http.server.request.duration"][0], [0.004, {
    "http.request.method": "GET", "http.route": "/api/rudder/home/:id/control", "http.response.status_code": 101,
  }]);
  assert.equal(calls.histograms["http.server.request.duration"][1][1]["http.request.method"], "_OTHER");

  // A scanner cannot mint unbounded routes.
  for (let i = 0; i < 400; i++) telemetry.recordRequest("GET", `/probe${i}/x`, 404, 0.001);
  const routes = new Set(calls.histograms["http.server.request.duration"].map(([, attrs]) => attrs["http.route"]));
  assert.ok(routes.size <= 201, `${routes.size} routes`);
  assert.ok(routes.has("/other"));

  telemetry.recordHomeDisconnect("home-53c2");
  assert.deepEqual(calls.counters["rudder.control.home.disconnects"], [[1, { "rudder.home.id": "home-53c2" }]]);

  const seen = [];
  calls.callback({ observe: (instrument, value, attrs) => seen.push({ name: instrument.name, value, attrs }) });
  const find = (name, attrs = {}) => seen.filter((s) => s.name === name && Object.entries(attrs).every(([k, v]) => s.attrs?.[k] === v));
  assert.equal(find("rudder.control.homes", { state: "running" })[0].value, 2);
  assert.equal(find("rudder.control.home.heartbeat.age").length, 1, "a home with no heartbeat yet has no age");
  assert.equal(find("rudder.control.home.control_connected", { "rudder.home.id": "home-9999" })[0].value, 0);
  assert.equal(find("rudder.control.home.clients", { "rudder.home.id": "home-53c2" })[0].value, 3);
  assert.equal(find("rudder.control.home.state", { "rudder.home.id": "home-9999", state: "stopped" })[0].value, 1);
  assert.equal(find("rudder.control.ws.connections", { role: "home-control" })[0].value, 1);
  assert.ok(find("rudder.control.process.memory", { kind: "rss" })[0].value > 0);
  const catalog = fs.readFileSync(new URL("../observability/metrics.md", import.meta.url), "utf8");
  for (const name of new Set([...seen.map((s) => s.name), ...Object.keys(calls.histograms), ...Object.keys(calls.counters)])) {
    assert.ok(catalog.includes(`\`${name}\``), `${name} is in observability/metrics.md`);
  }
  await telemetry.shutdown();
});
