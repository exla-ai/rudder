// Control plane metrics for the observability stack (observability/metrics.md).
// The home side (cloud/worker/telemetry.mjs) says how a machine is doing; this
// side says what users feel: request latency, which homes have their control
// socket up, how stale each heartbeat is, and every control disconnect, which
// is exactly the "Cloud machine reconnecting" badge.
//
// Inert unless RUDDER_OTEL_ENDPOINT is set; the SDK is imported only then.

import os from "node:os";
import { monitorEventLoopDelay } from "node:perf_hooks";

export type ControlSnapshot = {
  homes: Record<string, number>;
  sails: Record<string, number>;
  /** One entry per live home row. */
  perHome: Array<{ homeId: string; state: string; heartbeatAgeSeconds: number | null; controlConnected: boolean; clients: number }>;
  ws: { worker: number; client: number; homeControl: number };
  /** Burst build pool, when enabled: builders by state. */
  builders?: Record<string, number>;
};

export type ControlTelemetry = {
  enabled: boolean;
  recordRequest(method: string | undefined, pathname: string, status: number, seconds: number): void;
  recordHomeDisconnect(homeId: string): void;
  recordBuilderLease(outcome: "granted" | "busy" | "failed", warm?: boolean): void;
  flush(): Promise<void>;
  shutdown(): Promise<void>;
};

const OFF: ControlTelemetry = {
  enabled: false,
  recordRequest() {},
  recordHomeDisconnect() {},
  recordBuilderLease() {},
  async flush() {},
  async shutdown() {},
};

// OTel's recommended boundaries for http.server.request.duration (seconds).
const DURATION_BUCKETS = [0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10, 30];
const MAX_ROUTES = 200;
const METHODS = new Set(["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]);

/**
 * A request path with its ids replaced, so one route is one series:
 * /api/rudder/home/home-53c2/control -> /api/rudder/home/:id/control.
 * Static app files collapse to /app/*.
 */
export function normalizeRoute(pathname: string): string {
  const clean = (pathname.split("?")[0] || "/").replace(/\/+$/, "") || "/";
  if (clean.startsWith("/app/")) return "/app/*";
  const parts = clean.split("/").map((segment) => {
    if (!segment) return segment;
    if (/^\d+$/.test(segment)) return ":id";
    if (/^[0-9a-f]{8,}$/i.test(segment) || /^[0-9a-f]{8}-[0-9a-f-]{27,}$/i.test(segment)) return ":id";
    if (/^(home|ws|sail|run|plan|org|task|wf|repo|team|inv|call)[-_][A-Za-z0-9_-]+$/.test(segment)) return ":id";
    if (segment.length > 24 || (/\d/.test(segment) && /^[A-Za-z0-9_.-]{12,}$/.test(segment))) return ":id";
    return segment;
  });
  return parts.join("/") || "/";
}

type Sdk = {
  MeterProvider: new (options: { resource: unknown; readers: unknown[] }) => {
    getMeter(name: string): any;
    shutdown(): Promise<void>;
  };
  PeriodicExportingMetricReader: new (options: Record<string, unknown>) => { forceFlush(): Promise<void> };
  OTLPMetricExporter: new (options: Record<string, unknown>) => unknown;
  resourceFromAttributes(attributes: Record<string, string>): unknown;
};

export async function startControlTelemetry({
  env = process.env,
  version = "unknown",
  snapshot,
  log = () => {},
  sdk,
}: {
  env?: NodeJS.ProcessEnv;
  version?: string;
  snapshot: () => ControlSnapshot;
  log?: (line: string) => void;
  sdk?: Sdk;
}): Promise<ControlTelemetry> {
  const endpoint = (env.RUDDER_OTEL_ENDPOINT || "").trim().replace(/\/$/, "");
  if (!endpoint) return OFF;
  let otel: Sdk;
  try {
    otel = sdk ?? await loadSdk();
  } catch (error) {
    log(`[telemetry] off (OpenTelemetry SDK not installed: ${error instanceof Error ? error.message : String(error)})`);
    return OFF;
  }
  const intervalMs = Math.max(5_000, Number(env.RUDDER_OTEL_INTERVAL_MS) || 30_000);
  const headers: Record<string, string> = env.RUDDER_OTEL_TOKEN ? { authorization: `Bearer ${env.RUDDER_OTEL_TOKEN}` } : {};
  const machineId = env.FLY_MACHINE_ID || "";
  const resource = otel.resourceFromAttributes({
    "service.name": "rudder-control",
    "service.instance.id": machineId || os.hostname(),
    "service.version": version,
    "fly.machine.id": machineId,
    "fly.region": env.FLY_REGION || "",
    "deployment.environment.name": env.RUDDER_OTEL_ENVIRONMENT || "production",
  });
  const reader = new otel.PeriodicExportingMetricReader({
    exporter: new otel.OTLPMetricExporter({ url: `${endpoint}/v1/metrics`, headers, timeoutMillis: 10_000 }),
    exportIntervalMillis: intervalMs,
    exportTimeoutMillis: 10_000,
  });
  const provider = new otel.MeterProvider({ resource, readers: [reader] });
  const meter = provider.getMeter("rudder-control");

  const duration = meter.createHistogram("http.server.request.duration", {
    unit: "s",
    description: "HTTP request duration",
    advice: { explicitBucketBoundaries: DURATION_BUCKETS },
  });
  const disconnects = meter.createCounter("rudder.control.home.disconnects", {
    unit: "{disconnect}",
    description: "Home control socket closes (what users see as reconnecting)",
  });
  const builderLeases = meter.createCounter("rudder.control.builder.leases", {
    unit: "{lease}",
    description: "Build pool lease requests by outcome",
  });
  for (const outcome of ["granted", "busy", "failed"]) builderLeases.add(0, { outcome, start: outcome === "granted" ? "warm" : "none" });
  builderLeases.add(0, { outcome: "granted", start: "cold" });
  const gauge = (name: string, unit: string, description: string) => meter.createObservableGauge(name, { unit, description });
  const g = {
    homes: gauge("rudder.control.homes", "{home}", "Home machines by state"),
    homeState: gauge("rudder.control.home.state", "", "1 for a home's current state (running, stopped, ...)"),
    sails: gauge("rudder.control.sails", "{sail}", "Sails by state"),
    heartbeat: gauge("rudder.control.home.heartbeat.age", "s", "Seconds since a home's last heartbeat"),
    connected: gauge("rudder.control.home.control_connected", "1", "1 while a home's control socket is open"),
    clients: gauge("rudder.control.home.clients", "{client}", "Terminals attached to a home's repos"),
    ws: gauge("rudder.control.ws.connections", "{connection}", "Open WebSockets by role"),
    loopDelay: gauge("rudder.control.event_loop.delay", "s", "Event loop delay"),
    memory: gauge("rudder.control.process.memory", "By", "Control plane process memory"),
    builders: gauge("rudder.control.builders", "{builder}", "Build pool machines by state"),
  };
  const loop = monitorEventLoopDelay({ resolution: 20 });
  loop.enable();

  meter.addBatchObservableCallback((result: { observe(instrument: unknown, value: number, attributes?: Record<string, string>): void }) => {
    let snap: ControlSnapshot | null = null;
    try {
      snap = snapshot();
    } catch (error) {
      log(`[telemetry] snapshot failed: ${error instanceof Error ? error.message : String(error)}`);
    }
    if (snap) {
      for (const [state, count] of Object.entries(snap.homes)) result.observe(g.homes, count, { state });
      for (const [state, count] of Object.entries(snap.sails)) result.observe(g.sails, count, { state });
      for (const home of snap.perHome) {
        const attributes = { "rudder.home.id": home.homeId };
        // A home the control plane stopped on purpose is quiet, not lost.
        result.observe(g.homeState, 1, { ...attributes, state: home.state });
        if (home.heartbeatAgeSeconds !== null) result.observe(g.heartbeat, home.heartbeatAgeSeconds, attributes);
        result.observe(g.connected, home.controlConnected ? 1 : 0, attributes);
        result.observe(g.clients, home.clients, attributes);
      }
      result.observe(g.ws, snap.ws.worker, { role: "worker" });
      result.observe(g.ws, snap.ws.client, { role: "client" });
      result.observe(g.ws, snap.ws.homeControl, { role: "home-control" });
      for (const [state, count] of Object.entries(snap.builders ?? {})) result.observe(g.builders, count, { state });
    }
    result.observe(g.loopDelay, loop.percentile(50) / 1e9, { stat: "p50" });
    result.observe(g.loopDelay, loop.percentile(99) / 1e9, { stat: "p99" });
    result.observe(g.loopDelay, loop.max / 1e9, { stat: "max" });
    loop.reset();
    const memory = process.memoryUsage();
    result.observe(g.memory, memory.rss, { kind: "rss" });
    result.observe(g.memory, memory.heapUsed, { kind: "heap" });
  }, Object.values(g));

  // A scanner hitting random paths must not mint a series per path.
  const routes = new Set<string>();
  log(`[telemetry] pushing to ${endpoint} every ${Math.round(intervalMs / 1000)}s`);
  return {
    enabled: true,
    recordRequest(method, pathname, status, seconds) {
      let route = normalizeRoute(pathname);
      if (!routes.has(route)) {
        if (routes.size >= MAX_ROUTES) route = "/other";
        else routes.add(route);
      }
      const verb = String(method || "GET").toUpperCase();
      duration.record(seconds, {
        "http.request.method": METHODS.has(verb) ? verb : "_OTHER",
        "http.route": route,
        "http.response.status_code": status,
      });
    },
    recordHomeDisconnect(homeId) {
      disconnects.add(1, { "rudder.home.id": homeId });
    },
    recordBuilderLease(outcome, warm) {
      builderLeases.add(1, { outcome, start: outcome === "granted" ? (warm ? "warm" : "cold") : "none" });
    },
    async flush() {
      await reader.forceFlush().catch(() => undefined);
    },
    async shutdown() {
      loop.disable();
      await provider.shutdown().catch(() => undefined);
    },
  };
}

async function loadSdk(): Promise<Sdk> {
  const [metrics, exporter, resources] = await Promise.all([
    import("@opentelemetry/sdk-metrics"),
    import("@opentelemetry/exporter-metrics-otlp-http"),
    import("@opentelemetry/resources"),
  ]);
  return {
    MeterProvider: metrics.MeterProvider as unknown as Sdk["MeterProvider"],
    PeriodicExportingMetricReader: metrics.PeriodicExportingMetricReader as unknown as Sdk["PeriodicExportingMetricReader"],
    OTLPMetricExporter: exporter.OTLPMetricExporter as unknown as Sdk["OTLPMetricExporter"],
    resourceFromAttributes: resources.resourceFromAttributes as unknown as Sdk["resourceFromAttributes"],
  };
}
