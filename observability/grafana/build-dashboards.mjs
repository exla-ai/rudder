#!/usr/bin/env node
// The Grafana dashboards for Rudder Cloud, as code. On 2026-11-02 a home ran
// out of memory (no swap, 34 Claude agents idle for days) and every attached
// terminal showed "Cloud machine reconnecting" before anyone noticed. These
// pages exist so that is visible early: the fleet page says which home is in
// trouble, the home page says why, the control plane page says whether users
// can reach their homes at all.
//
//   node observability/grafana/build-dashboards.mjs          write dashboards/*.json
//   node observability/grafana/build-dashboards.mjs --check  exit 1 if they are stale
//
// Metric names come from observability/metrics.md and nowhere else; the test
// (tests/observability-dashboards.test.mjs) fails on any name it does not list.
// Edit this file, run it, commit both. Never hand-edit the JSON: the check
// would flag it and the next run would drop the change.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const CLI_PATH = fileURLToPath(import.meta.url);
export const DASHBOARD_DIR = path.join(path.dirname(CLI_PATH), "dashboards");

const DS = { type: "prometheus", uid: "prometheus" };
const SCHEMA_VERSION = 41;

// Categorical slots in fixed order: the dataviz reference palette at its dark
// steps. Grafana cannot switch a fixed color with the theme, and these steps
// pass every palette check against both Grafana panel surfaces (#ffffff and
// #181b1f). A series keeps its slot on every page (working is always slot 1).
const SLOT = ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"];
// Context marks (totals, the "other" bucket, the quieter half of a pair).
const MUTED = "#898781";
// Ordered series (p50 < p95 < p99): one hue, light to dark.
const RAMP = ["#86b6ef", "#3987e5", "#1c5cab"];
// Reserved for good and bad; never a series identity.
const STATUS = { good: "#0ca30c", warning: "#fab219", serious: "#ec835a", critical: "#d03b3b" };

// Healthy values stay in the text color (or no fill in tables) so only the
// numbers that need attention light up.
const CALM = "text";
const CLEAR = "transparent";

const AGENT_STATES = [
  ["working", SLOT[0]],
  ["waiting", SLOT[1]],
  ["idle", SLOT[2]],
  ["asleep", SLOT[3]],
];
// Stacking order (bottom up) is the order below: Claude usually holds most.
// Nine groups is one more than the palette has, so "other" is gray.
const PROCESS_GROUPS = [
  ["claude", SLOT[0]],
  ["codex", SLOT[1]],
  ["opencode", SLOT[2]],
  ["dashboard", SLOT[3]],
  ["supervisor", SLOT[4]],
  ["mcp", SLOT[5]],
  ["build", SLOT[6]],
  ["node", SLOT[7]],
  ["other", MUTED],
];
// When a home has more than one scaler decision, the Homes table shows the
// most urgent one (the Scaling section lists all of them).
const DECISION_PRIORITY = [
  "scale-up-memory",
  "run-cleanup",
  "add-swap",
  "extend-volume",
  "investigate-silent",
  "scale-down-memory",
];

const FLEET = 'rudder_home_id=~"$home"';
const ONE = 'rudder_home_id="$home"';
const HTTP = "http_server_request_duration_seconds";

const HOME_URL = "/d/rudder-home/rudder-home-machine?var-home=${__value.raw}&${__url_time_range}";

// ---------------------------------------------------------------------------
// PromQL. Every home series is collapsed by home id first: a supervisor
// restart or an image upgrade starts a new series (new service_version or
// instance) while the old one lingers for the 5 minute lookback, and a raw
// sum would count that home twice.
// ---------------------------------------------------------------------------

function sel(metric, ...matchers) {
  const list = matchers.filter(Boolean);
  return list.length ? `${metric}{${list.join(",")}}` : metric;
}

function perHome(metric, ...matchers) {
  return `max by (rudder_home_id) (${sel(metric, ...matchers)})`;
}

function ratio(numerator, denominator) {
  return `${numerator} / ${denominator}`;
}

// Swap used as a share of swap, or -1 when the home has no swap at all (the
// mapping prints "No swap"). Keeps "no swap" apart from "not reporting".
function swapUsed(by) {
  const total = by("rudder_home_swap_total_bytes");
  const free = by("rudder_home_swap_free_bytes");
  return `((${total} - ${free}) / (${total} > 0)) or (${total} - 1)`;
}

// Zero only while the home reports at all: a silent home reads "-", not
// "0 agents", which would look like a quiet but healthy machine.
function agentsTotal(state, home) {
  return `sum(${perHome("rudder_home_agents", `state="${state}"`, home)}) or (max(${sel("rudder_home_memory_total_bytes", home)}) * 0)`;
}

// ---------------------------------------------------------------------------
// Panel builders. Each returns a panel without id or position; layout()
// assigns both so ids stay unique and the grid stays tidy.
// ---------------------------------------------------------------------------

function target(expr, refId, { legend, instant = false, table = false, interval } = {}) {
  const t = { datasource: DS, editorMode: "code", expr, refId };
  if (table) {
    Object.assign(t, { format: "table", instant: true, range: false });
  } else {
    Object.assign(t, { instant, range: !instant, legendFormat: legend ?? "__auto" });
  }
  if (interval) t.interval = interval;
  return t;
}

const REF_IDS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/** [[expr, legend, opts?], ...] -> targets with refIds A, B, C... */
function targets(list, defaults = {}) {
  return list.map(([expr, legend, opts], i) => target(expr, REF_IDS[i], { legend, ...defaults, ...opts }));
}

function steps(...pairs) {
  return { mode: "absolute", steps: pairs.map(([value, color]) => ({ color, value })) };
}

function valueMap(entries) {
  const options = {};
  entries.forEach(([value, text, color], index) => {
    options[String(value)] = color ? { color, index, text } : { index, text };
  });
  return { options, type: "value" };
}

// A missing value (no series for that home, or 0/0) must never wear a status
// color: Grafana colors null with the base threshold step, which is red on
// every "low is bad" column. Every table column and stat tile gets this.
const MISSING = "-";

function missing(text = MISSING, color) {
  const result = color ? { color, index: 99, text } : { index: 99, text };
  return { options: { match: "null+nan", result }, type: "special" };
}

const hasMissing = (mappings) => mappings.some((m) => m.type === "special");

function fixed(color) {
  return { fixedColor: color, mode: "fixed" };
}

function override(matcher, properties) {
  return {
    matcher,
    properties: Object.entries(properties).map(([id, value]) => ({ id, value })),
  };
}

const byRef = (refId) => ({ id: "byFrameRefID", options: refId });
const byName = (name) => ({ id: "byName", options: name });

function row(title) {
  return { collapsed: false, panels: [], title, type: "row" };
}

function stat({ title, description, size = [6, 3], expr, unit = "none", decimals, thresholds, mappings = [], noValue }) {
  // The number in the text color while healthy, amber or red when a status
  // tile crosses a threshold. No sparkline: at this height it shrinks the
  // number, and the charts below carry the history.
  const defaults = {
    color: { mode: "thresholds" },
    // No data reads as noValue (or "-") in the text color, never in red.
    mappings: hasMissing(mappings) ? mappings : [...mappings, missing(noValue ?? MISSING, CALM)],
    noValue: noValue ?? MISSING,
    thresholds: thresholds ?? steps([null, CALM]),
    unit,
  };
  if (decimals !== undefined) defaults.decimals = decimals;
  return {
    datasource: DS,
    description,
    fieldConfig: { defaults, overrides: [] },
    options: {
      colorMode: "value",
      graphMode: "none",
      justifyMode: "auto",
      orientation: "auto",
      reduceOptions: { calcs: ["lastNotNull"], fields: "", values: false },
      showPercentChange: false,
      textMode: "value",
      wideLayout: true,
    },
    size,
    // One number, so one evaluation instead of a range.
    targets: targets([[expr, title, { instant: true }]]),
    title,
    type: "stat",
  };
}

function timeseries({
  title,
  description,
  size = [12, 9],
  unit = "none",
  series,
  color = SLOT[0],
  min = 0,
  max,
  softMax,
  decimals,
  stack = false,
  bars = false,
  step = false,
  thresholds,
  legend = true,
  calcs = ["lastNotNull", "max"],
  overrides = [],
  interval,
  legendRight = false,
}) {
  const custom = {
    axisSoftMin: min,
    drawStyle: bars ? "bars" : "line",
    // Thin lines; stacked areas and bars get a light fill so a full stack
    // never reads as one saturated block.
    fillOpacity: bars ? 70 : stack ? 30 : 0,
    gradientMode: "none",
    lineInterpolation: step ? "stepAfter" : "linear",
    lineWidth: stack || bars ? 1 : 2,
    showPoints: "never",
    spanNulls: false,
    stacking: { group: "A", mode: stack ? "normal" : "none" },
    thresholdsStyle: { mode: thresholds ? "dashed" : "off" },
  };
  if (softMax !== undefined) custom.axisSoftMax = softMax;
  const defaults = {
    color: fixed(color),
    custom,
    mappings: [],
    min,
    thresholds: thresholds ?? steps([null, CLEAR]),
    unit,
  };
  if (max !== undefined) defaults.max = max;
  if (decimals !== undefined) defaults.decimals = decimals;
  // A templated legend means one series per label value.
  const many = series.length > 1 || series.some(([, legend]) => String(legend).includes("{{"));
  return {
    datasource: DS,
    description,
    fieldConfig: { defaults, overrides },
    options: {
      legend: legend
        ? { calcs, displayMode: "table", placement: legendRight ? "right" : "bottom", showLegend: true, sortBy: "Max", sortDesc: true }
        : { calcs: [], displayMode: "list", placement: "bottom", showLegend: false },
      tooltip: { hideZeros: false, mode: many ? "multi" : "single", sort: many ? "desc" : "none" },
    },
    size,
    targets: targets(series, interval ? { interval } : {}),
    title,
    type: "timeseries",
  };
}

/** Colors for targets A, B, C... in order (which is also stack order). */
function colored(colors) {
  return colors.map((color, i) => override(byRef(REF_IDS[i]), { color: fixed(color) }));
}

function table({ title, description, size = [24, 9], queries, columns, sortBy, hidden = [], overrides = [] }) {
  // Instant queries that all return one row per key, merged into one table.
  // columns: [[sourceName, displayName], ...] in display order.
  const excludeByName = { Time: true };
  for (const name of hidden) excludeByName[name] = true;
  const indexByName = {};
  const renameByName = {};
  columns.forEach(([source, display], i) => {
    indexByName[source] = i;
    if (display !== source) renameByName[source] = display;
  });
  return {
    datasource: DS,
    description,
    fieldConfig: {
      defaults: {
        // Grafana's default 150px minimum pushes a 14 column table off screen.
        custom: { align: "auto", cellOptions: { type: "auto" }, filterable: false, inspect: false, minWidth: 80 },
        mappings: [missing()],
        thresholds: steps([null, CLEAR]),
        unit: "none",
      },
      overrides,
    },
    options: {
      cellHeight: "sm",
      showHeader: true,
      sortBy: sortBy ? [{ desc: sortBy[1] === "desc", displayName: sortBy[0] }] : [],
    },
    size,
    targets: queries.map((expr, i) => target(expr, REF_IDS[i], { table: true })),
    title,
    transformations: [
      { id: "merge", options: {} },
      { id: "organize", options: { excludeByName, includeByName: {}, indexByName, renameByName } },
    ],
    type: "table",
  };
}

/** A table column: unit, thresholds and colored cells when it carries status. */
function column(name, { description, unit, decimals, thresholds, mappings, links, width } = {}) {
  const props = {};
  if (description) props.description = description;
  if (unit) props.unit = unit;
  if (decimals !== undefined) props.decimals = decimals;
  if (thresholds) props.thresholds = thresholds;
  const colored = thresholds || mappings?.some((m) => JSON.stringify(m).includes('"color"'));
  if (colored) {
    props["custom.cellOptions"] = { mode: "basic", type: "color-background" };
  }
  // An override replaces the default mappings, so a colored column restates
  // the missing value mapping with an explicit clear fill.
  if (mappings || colored) {
    const list = mappings ?? [];
    props.mappings = hasMissing(list) ? list : [...list, missing(MISSING, colored ? CLEAR : undefined)];
  }
  if (links) props.links = links;
  if (width) props["custom.width"] = width;
  return override(byName(name), props);
}

const homeLink = [{ targetBlank: false, title: "Open this home", url: HOME_URL }];

function barGauge({ title, description, size, expr, legend }) {
  return {
    datasource: DS,
    description,
    fieldConfig: { defaults: { color: fixed(SLOT[0]), mappings: [], min: 0, thresholds: steps([null, CALM]), unit: "none", decimals: 0 }, overrides: [] },
    options: {
      displayMode: "basic",
      // Fixed bar height and text: with two or three states, auto sizing
      // grows the text until the state names no longer fit.
      maxVizHeight: 44,
      minVizHeight: 40,
      minVizWidth: 8,
      // Above the bar: beside it Grafana gives names a quarter of the width
      // and cuts "stopped" to "stop...".
      namePlacement: "top",
      orientation: "horizontal",
      reduceOptions: { calcs: ["lastNotNull"], fields: "", values: false },
      showUnfilled: true,
      sizing: "manual",
      text: { titleSize: 14, valueSize: 16 },
      valueMode: "text",
    },
    size,
    targets: targets([[expr, legend, { instant: true }]]),
    title,
    type: "bargauge",
  };
}

const CONNECTED = [valueMap([[0, "Disconnected", STATUS.critical], [1, "Connected", STATUS.good]])];

// The control plane's view of a home as one number, so one table column: the
// socket while the home is running, otherwise the home's state. A stopped
// home is quiet on purpose and must read "stopped", not "Disconnected".
const HOME_STATE_CODES = [["queued", 2], ["paused", 3], ["stopped", 4], ["failed", 5], ["running", 6]];

function controlView(home) {
  const inState = (state) => `max by (rudder_home_id) (${sel("rudder_control_home_state", `state="${state}"`, home)} == 1)`;
  const socket = perHome("rudder_control_home_control_connected_ratio", home);
  return [
    `(${socket} and on (rudder_home_id) ${inState("running")})`,
    // Running without a socket series reads "running" (code 6).
    ...HOME_STATE_CODES.map(([state, code]) => `${inState(state)} * ${code}`),
    // A control plane without the state gauge still shows the socket.
    socket,
  ].join(" or ");
}

const CONTROL_VIEW = {
  description: "The control plane's view: Connected or Disconnected while the home is running, otherwise the home's state (a stopped home is quiet on purpose)",
  mappings: [
    valueMap([
      [0, "Disconnected", STATUS.critical],
      [1, "Connected"],
      ...HOME_STATE_CODES.map(([state, code]) => (state === "failed" ? [code, state, STATUS.critical] : [code, state])),
    ]),
  ],
  width: 115,
};

// ---------------------------------------------------------------------------
// Layout: panels flow left to right on Grafana's 24 column grid and wrap; a
// row always starts a new line. Ids are assigned in reading order.
// ---------------------------------------------------------------------------

function layout(items) {
  const panels = [];
  let x = 0;
  let y = 0;
  let lineHeight = 0;
  let id = 1;
  const newLine = () => {
    y += lineHeight;
    x = 0;
    lineHeight = 0;
  };
  for (const item of items) {
    if (item.type === "row") {
      if (x > 0) newLine();
      panels.push({ ...item, gridPos: { h: 1, w: 24, x: 0, y }, id: id++ });
      y += 1;
      continue;
    }
    const { size, ...panel } = item;
    const [w, h] = size;
    if (x + w > 24) newLine();
    panels.push({ ...panel, gridPos: { h, w, x, y }, id: id++ });
    x += w;
    lineHeight = Math.max(lineHeight, h);
  }
  return panels;
}

function homeVariable({ multi }) {
  const query = "label_values(rudder_home_memory_total_bytes,rudder_home_id)";
  const variable = {
    current: multi ? { text: ["All"], value: ["$__all"] } : {},
    datasource: DS,
    definition: query,
    includeAll: multi,
    label: "Home",
    multi,
    name: "home",
    options: [],
    query: { qryType: 1, query, refId: "PrometheusVariableQueryEditor-VariableQuery" },
    refresh: 2,
    regex: "",
    sort: 1,
    type: "query",
  };
  if (multi) variable.allValue = ".*";
  return variable;
}

function dashboard({ uid, title, description, variables = [], items }) {
  return {
    annotations: {
      list: [
        {
          builtIn: 1,
          datasource: { type: "grafana", uid: "-- Grafana --" },
          enable: true,
          hide: true,
          iconColor: "rgba(0, 211, 255, 1)",
          name: "Annotations & Alerts",
          type: "dashboard",
        },
      ],
    },
    description,
    editable: true,
    fiscalYearStartMonth: 0,
    // Shared crosshair: memory, pressure and agents line up under one cursor.
    graphTooltip: 1,
    id: null,
    // Tag links resolve through Grafana's search, so they keep working when
    // Grafana is served under /dashboard/ (serve_from_sub_path).
    links: [
      {
        asDropdown: false,
        icon: "external link",
        includeVars: false,
        keepTime: true,
        tags: ["rudder"],
        targetBlank: false,
        title: "Rudder dashboards",
        tooltip: "",
        type: "dashboards",
        url: "",
      },
    ],
    panels: layout(items),
    refresh: "30s",
    schemaVersion: SCHEMA_VERSION,
    tags: ["rudder"],
    templating: { list: variables },
    time: { from: "now-6h", to: "now" },
    timepicker: {},
    timezone: "browser",
    title,
    uid,
    version: 1,
    weekStart: "",
  };
}

// ---------------------------------------------------------------------------
// Rudder fleet: which home needs attention right now.
// ---------------------------------------------------------------------------

function fleetDashboard() {
  const memAvailable = ratio(perHome("rudder_home_memory_available_bytes", FLEET), perHome("rudder_home_memory_total_bytes", FLEET));
  const memFull60 = perHome("rudder_home_pressure_ratio", 'resource="memory"', 'kind="full"', 'window="avg60"', FLEET);
  const ioFull60 = perHome("rudder_home_pressure_ratio", 'resource="io"', 'kind="full"', 'window="avg60"', FLEET);
  const diskFree = ratio(
    perHome("rudder_home_disk_free_bytes", 'mount="/workspace"', FLEET),
    perHome("rudder_home_disk_total_bytes", 'mount="/workspace"', FLEET),
  );
  const agents = (state) => perHome("rudder_home_agents", `state="${state}"`, FLEET);
  const decision = (...matchers) => `max by (rudder_home_id, action, state) (${sel("rudder_scaler_decision", ...matchers, FLEET)} == 1)`;
  const mostUrgent = `topk by (rudder_home_id) (1, ${DECISION_PRIORITY.map(
    (action, i) => `${decision(`action="${action}"`)} * ${DECISION_PRIORITY.length - i}`,
  ).join(" or ")})`;
  const withDecision = (expr, ...matchers) => `${decision(...matchers)} * on (rudder_home_id) group_left () ${expr}`;

  const items = [
    stat({
      title: "Homes reporting",
      description: "Home machines that sent metrics in the last 5 minutes. If this is lower than the running homes on the control plane page, a home went silent: the scaler raises investigate-silent for it.",
      expr: `count(${perHome("rudder_home_memory_total_bytes", FLEET)}) or vector(0)`,
      decimals: 0,
    }),
    stat({
      title: "Under memory pressure",
      description: "Homes where every task was stalled on memory for at least 10% of the last minute. Red means a home is thrashing and its terminals are about to drop: open it from the Homes table, see what holds the memory, and let the cleanup or the scaler act.",
      expr: `count(${memFull60} >= 0.1) or vector(0)`,
      decimals: 0,
      thresholds: steps([null, CALM], [1, STATUS.critical]),
    }),
    stat({
      title: "Low on disk",
      description: "Homes with less than 10% free on /workspace. Red means checkouts and builds will start failing: extend the volume (the scaler recommends extend-volume).",
      expr: `count(${diskFree} < 0.1) or vector(0)`,
      decimals: 0,
      thresholds: steps([null, CALM], [1, STATUS.critical]),
    }),
    stat({
      title: "Disconnects, last hour",
      description: "Times the control plane saw a home's control socket close in the last hour. Each one shows \"Cloud machine reconnecting\" in every attached terminal. Amber or red: find the home in the Homes table and check its memory stall and supervisor event loop.",
      expr: `sum(increase(${sel("rudder_control_home_disconnects_total", FLEET)}[1h])) or vector(0)`,
      decimals: 0,
      thresholds: steps([null, CALM], [1, STATUS.warning], [5, STATUS.critical]),
    }),
    stat({
      title: "Agents working",
      description: "Agents in a live turn across the selected homes.",
      expr: agentsTotal("working", FLEET),
      decimals: 0,
    }),
    stat({
      title: "Agents idle",
      description: "Agents with a live process and no turn. They hold memory; the hourly cleanup puts the ones nobody has touched to sleep. A climbing count with no asleep agents means the cleanup is not running.",
      expr: agentsTotal("idle", FLEET),
      decimals: 0,
    }),
    stat({
      title: "Agents asleep",
      description: "Agents the cleanup parked. They keep their conversation and workspace and wake on the next message.",
      expr: agentsTotal("asleep", FLEET),
      decimals: 0,
    }),
    stat({
      title: "Scaler mode",
      description: "Dry run means the scaler only recommends. Applying means it may resize machines and extend volumes (RUDDER_SCALER_APPLY=1). Not reporting means Prometheus cannot scrape the scaler.",
      expr: "max(rudder_scaler_apply_enabled)",
      mappings: [valueMap([[0, "Dry run"], [1, "Applying"]])],
      noValue: "Not reporting",
    }),

    row("Homes"),
    table({
      title: "Homes",
      description: "One row per home, worst memory first. A - means the home sent no value (stopped reporting, or no such disk). Amber and red cells need attention: low memory or high stall means the home is close to dropping its terminals, low disk means builds will fail, a slow event loop means the supervisor is starving. Click a home to open its page.",
      size: [24, 10],
      queries: [
        memAvailable,
        swapUsed((metric) => perHome(metric, FLEET)),
        memFull60,
        ioFull60,
        diskFree,
        agents("working"),
        agents("idle"),
        agents("asleep"),
        perHome("rudder_home_process_memory_bytes", 'group="claude"', FLEET),
        perHome("rudder_home_supervisor_event_loop_delay_seconds", 'stat="p99"', FLEET),
        controlView(FLEET),
        // "scale-up-memory (waiting-for-idle)" in one label, so one column.
        `label_replace(label_join(${mostUrgent}, "decision", ",", "action", "state"), "decision", "$1 ($2)", "decision", "([^,]+),(.+)")`,
      ],
      columns: [
        ["rudder_home_id", "Home"],
        ["Value #A", "Available"],
        ["Value #B", "Swap used"],
        ["Value #C", "Mem stall"],
        ["Value #D", "IO stall"],
        ["Value #E", "Disk free"],
        ["Value #F", "Working"],
        ["Value #G", "Idle"],
        ["Value #H", "Asleep"],
        ["Value #I", "Claude"],
        ["Value #J", "Loop p99"],
        ["Value #K", "Control"],
        ["decision", "Scaler"],
      ],
      hidden: ["Value #L", "action", "state"],
      sortBy: ["Available", "asc"],
      overrides: [
        column("Home", { links: homeLink, width: 140 }),
        column("Available", { description: "Memory available, as a share of the machine", unit: "percentunit", decimals: 0, thresholds: steps([null, STATUS.critical], [0.1, STATUS.warning], [0.2, CLEAR]) }),
        column("Swap used", {
          description: "Swap in use, as a share of swap",
          unit: "percentunit",
          decimals: 0,
          thresholds: steps([null, CLEAR], [0.5, STATUS.warning], [0.8, STATUS.critical]),
          mappings: [valueMap([[-1, "No swap", STATUS.warning]])],
        }),
        column("Mem stall", { description: "Share of the last minute every task waited for memory (PSI full avg60)", unit: "percentunit", decimals: 1, thresholds: steps([null, CLEAR], [0.05, STATUS.warning], [0.2, STATUS.critical]) }),
        column("IO stall", { description: "Share of the last minute every task waited for disk (PSI full avg60)", unit: "percentunit", decimals: 1, thresholds: steps([null, CLEAR], [0.05, STATUS.warning], [0.2, STATUS.critical]) }),
        column("Disk free", { description: "Free space on /workspace", unit: "percentunit", decimals: 0, thresholds: steps([null, STATUS.critical], [0.1, STATUS.warning], [0.2, CLEAR]) }),
        column("Working", { description: "Agents in a live turn", decimals: 0 }),
        column("Idle", { description: "Agents with a live process and no turn", decimals: 0 }),
        column("Asleep", { description: "Agents the cleanup parked", decimals: 0 }),
        column("Claude", { description: "Resident memory of all Claude processes", unit: "bytes", decimals: 1 }),
        column("Loop p99", { description: "Supervisor event loop delay, p99", unit: "s", thresholds: steps([null, CLEAR], [0.1, STATUS.warning], [1, STATUS.critical]) }),
        column("Control", CONTROL_VIEW),
        column("Scaler", {
          description: "The most urgent scaler decision and its state; the Scaling section lists all of them",
          mappings: [{ options: { pattern: ".*failed.*", result: { color: STATUS.critical, index: 0 } }, type: "regex" }, missing("None", CLEAR)],
          width: 250,
        }),
      ],
    }),

    row("Trends"),
    timeseries({
      title: "Memory stall by home",
      description: "Share of the last minute every task on the home waited for memory (PSI full avg60). Above the amber line the home counts as under pressure; above the red line the cleanup runs early. A home that stays above red needs more memory: see the Scaling section.",
      unit: "percentunit",
      softMax: 0.25,
      series: [[memFull60, "{{rudder_home_id}}"]],
      thresholds: steps([null, CLEAR], [0.1, STATUS.warning], [0.2, STATUS.critical]),
    }),
    timeseries({
      title: "Memory available by home",
      description: "Memory the kernel can hand out without swapping, as a share of the machine. Below the red line the home is close to the OOM state of 2026-11-02; the cleanup runs early below 12%.",
      unit: "percentunit",
      max: 1,
      series: [[memAvailable, "{{rudder_home_id}}"]],
      thresholds: steps([null, CLEAR], [0.1, STATUS.critical], [0.2, STATUS.warning]),
      calcs: ["lastNotNull", "min"],
    }),
    timeseries({
      title: "Agents by state",
      description: "Live agents across the selected homes. Working and waiting hold a turn; idle agents hold memory for nothing until the cleanup puts them to sleep. A tall idle band with no asleep band means the cleanup is not keeping up.",
      decimals: 0,
      stack: true,
      legendRight: true,
      series: AGENT_STATES.map(([state]) => [agentsTotal(state, FLEET), state]),
      overrides: colored(AGENT_STATES.map(([, color]) => color)),
    }),
    timeseries({
      title: "Disconnects by home, per 5 minutes",
      description: "Control socket closes the control plane saw, in 5 minute buckets. Each bar is a \"Cloud machine reconnecting\" banner for that home's users. Repeated bars on one home usually follow memory stall on the chart above.",
      decimals: 0,
      bars: true,
      stack: true,
      interval: "5m",
      series: [[`sum by (rudder_home_id) (increase(${sel("rudder_control_home_disconnects_total", FLEET)}[5m]))`, "{{rudder_home_id}}"]],
      calcs: ["sum", "max"],
    }),

    row("Scaling"),
    stat({
      title: "Last scaler evaluation",
      description: "Seconds since the scaler last evaluated the fleet. It runs every minute; red means it stopped and nothing is watching the homes. Check the rudder-scaler process and its logs.",
      expr: "time() - max(max_over_time(rudder_scaler_last_evaluation_timestamp_seconds[1d]))",
      unit: "s",
      decimals: 0,
      thresholds: steps([null, CALM], [120, STATUS.warning], [300, STATUS.critical]),
      noValue: "Never",
    }),
    stat({
      title: "Scaler errors, last hour",
      description: "Evaluations that failed in the last hour, usually because Prometheus did not answer. Red means the scaler is blind: check that Prometheus is up.",
      expr: 'sum(increase(rudder_scaler_evaluations_total{outcome="error"}[1h])) or vector(0)',
      decimals: 0,
      thresholds: steps([null, CALM], [1, STATUS.warning], [5, STATUS.critical]),
    }),
    stat({
      title: "Homes with a decision",
      description: "Homes the scaler wants something done for. The table below says what and why it has not happened yet.",
      expr: `count(count by (rudder_home_id) (${sel("rudder_scaler_decision", FLEET)} == 1)) or vector(0)`,
      decimals: 0,
    }),
    stat({
      title: "Resizes waiting for idle",
      description: "Memory resizes the scaler holds back because agents are mid turn on that home. A resize restarts the machine, so it waits until nothing is working or waiting.",
      expr: `count(${sel("rudder_scaler_decision", 'state="waiting-for-idle"', FLEET)} == 1) or vector(0)`,
      decimals: 0,
    }),
    table({
      title: "Scaler decisions",
      description: "Every current decision. Recommended means a person (or the scaler in apply mode) should act; waiting-for-idle means the home is busy; cooldown means it acted recently; failed means the Fly API refused, so check the scaler logs. Empty means the scaler sees nothing to do.",
      size: [24, 8],
      queries: [
        decision(),
        withDecision(perHome("rudder_home_memory_total_bytes", FLEET)),
        withDecision(perHome("rudder_scaler_target_memory_bytes", FLEET), 'action=~"scale-up-memory|scale-down-memory"'),
        withDecision(`sum by (rudder_home_id) (max by (rudder_home_id, state) (${sel("rudder_home_agents", 'state=~"working|waiting"', FLEET)}))`),
      ],
      columns: [
        ["rudder_home_id", "Home"],
        ["action", "Action"],
        ["state", "State"],
        ["Value #B", "Memory now"],
        ["Value #C", "Target memory"],
        ["Value #D", "Busy agents"],
      ],
      hidden: ["Value #A"],
      sortBy: ["Home", "asc"],
      overrides: [
        column("Home", { links: homeLink, width: 140 }),
        column("State", { mappings: [valueMap([["failed", "failed", STATUS.critical]])] }),
        column("Memory now", { unit: "bytes", decimals: 0 }),
        column("Target memory", { unit: "bytes", decimals: 0 }),
        column("Busy agents", { decimals: 0 }),
      ],
    }),
  ];

  return dashboard({
    uid: "rudder-fleet",
    title: "Rudder fleet",
    description: "Every Rudder Cloud home at a glance: memory, disk, agents, disconnects and what the scaler wants to do.",
    variables: [homeVariable({ multi: true })],
    items,
  });
}

// ---------------------------------------------------------------------------
// Rudder home machine: why one home is struggling.
// ---------------------------------------------------------------------------

function homeDashboard() {
  const one = (metric, ...matchers) => `max(${sel(metric, ...matchers, ONE)})`;
  const total = one("rudder_home_memory_total_bytes");
  const available = one("rudder_home_memory_available_bytes");
  const pressure = (resource, kind, window = "avg10") => one("rudder_home_pressure_ratio", `resource="${resource}"`, `kind="${kind}"`, `window="${window}"`);
  const pressurePanel = (resource, title, description, extra = {}) =>
    timeseries({
      title,
      description,
      size: [8, 8],
      unit: "percentunit",
      softMax: 0.25,
      series: [
        [pressure(resource, "full"), "full"],
        [pressure(resource, "some"), "some"],
      ],
      overrides: colored([SLOT[0], MUTED]),
      ...extra,
    });

  const groupSeries = PROCESS_GROUPS.map(([group]) => [one("rudder_home_process_memory_bytes", `group="${group}"`), group]);
  const memoryOverlays = [
    [`${total} - ${available}`, "used"],
    [total, "total"],
  ];
  const groupCount = PROCESS_GROUPS.length;

  const items = [
    stat({
      title: "Memory available",
      description: "Memory the kernel can hand out without swapping. Below 12% the cleanup runs early; red means the home is close to the OOM state of 2026-11-02. Check Memory by process group below.",
      expr: ratio(available, total),
      unit: "percentunit",
      decimals: 0,
      thresholds: steps([null, STATUS.critical], [0.1, STATUS.warning], [0.2, CALM]),
    }),
    stat({
      title: "Swap used",
      description: "Share of swap in use. No swap means the machine booted from an image without the swap file; the scaler recommends add-swap. Red means swap is nearly full and the next spike will stall the machine.",
      expr: swapUsed((metric) => one(metric)),
      unit: "percentunit",
      decimals: 0,
      thresholds: steps([null, CALM], [0.5, STATUS.warning], [0.8, STATUS.critical]),
      mappings: [valueMap([[-1, "No swap", STATUS.warning]])],
    }),
    stat({
      title: "Memory stall",
      description: "Share of the last 10 seconds every task waited for memory (PSI full avg10). Red means the machine is thrashing right now: terminals lag, then drop.",
      expr: pressure("memory", "full"),
      unit: "percentunit",
      decimals: 1,
      thresholds: steps([null, CALM], [0.05, STATUS.warning], [0.2, STATUS.critical]),
    }),
    stat({
      title: "Disk free",
      description: "Free space on /workspace, where checkouts, builds and caches live. Red means builds will start failing: extend the volume.",
      expr: ratio(one("rudder_home_disk_free_bytes", 'mount="/workspace"'), one("rudder_home_disk_total_bytes", 'mount="/workspace"')),
      unit: "percentunit",
      decimals: 0,
      thresholds: steps([null, STATUS.critical], [0.1, STATUS.warning], [0.2, CALM]),
    }),
    ...[0, 2, 3].map((i) => {
      const [state] = AGENT_STATES[i];
      return stat({
        title: `Agents ${state}`,
        description: {
          working: "Agents in a live turn on this home.",
          idle: "Agents with a live process and no turn. They hold memory until the cleanup puts them to sleep.",
          asleep: "Agents the cleanup parked. One message wakes them.",
        }[state],
        expr: agentsTotal(state, ONE),
        decimals: 0,
      });
    }),
    stat({
      title: "Control socket",
      description: "Whether the supervisor's control socket to the control plane is open, as the home sees it. Disconnected means attached terminals show \"Cloud machine reconnecting\"; check memory stall and the supervisor event loop.",
      expr: one("rudder_home_control_connected_ratio"),
      thresholds: steps([null, CALM]),
      mappings: CONNECTED,
      noValue: "Not reporting",
    }),

    row("Memory"),
    timeseries({
      title: "Memory by process group",
      description: "Resident memory summed per process group, stacked, with memory in use (total minus available) and the machine's total as lines. Claude growing with many idle agents is what the cleanup fixes; used close to total with no swap is the 2026-11-02 failure.",
      size: [16, 10],
      unit: "bytes",
      stack: true,
      legendRight: true,
      series: [...groupSeries, ...memoryOverlays],
      calcs: ["lastNotNull", "max"],
      overrides: [
        ...colored(PROCESS_GROUPS.map(([, color]) => color)),
        override(byRef(REF_IDS[groupCount]), {
          color: fixed(MUTED),
          "custom.fillOpacity": 0,
          "custom.lineWidth": 2,
          "custom.stacking": { group: "A", mode: "none" },
        }),
        override(byRef(REF_IDS[groupCount + 1]), {
          color: fixed(MUTED),
          "custom.fillOpacity": 0,
          "custom.lineStyle": { dash: [10, 10], fill: "dash" },
          "custom.lineWidth": 2,
          "custom.stacking": { group: "A", mode: "none" },
        }),
      ],
    }),
    timeseries({
      title: "Swap",
      description: "Swap in use against the swap size (dashed). A flat zero total means the home has no swap and nothing cushions a memory spike.",
      size: [8, 10],
      unit: "bytes",
      series: [
        [`${one("rudder_home_swap_total_bytes")} - ${one("rudder_home_swap_free_bytes")}`, "used"],
        [one("rudder_home_swap_total_bytes"), "total"],
      ],
      overrides: [
        override(byRef("A"), { color: fixed(SLOT[0]) }),
        override(byRef("B"), { color: fixed(MUTED), "custom.lineStyle": { dash: [10, 10], fill: "dash" } }),
      ],
    }),

    row("Pressure stall"),
    pressurePanel(
      "memory",
      "Memory stall",
      "Share of the last 10 seconds tasks waited for memory: full means every task, some means at least one. The cleanup runs early once full (over a minute) reaches the red line; staying above it means the home needs more memory.",
      { thresholds: steps([null, CLEAR], [0.2, STATUS.critical]) },
    ),
    pressurePanel("io", "IO stall", "Share of the last 10 seconds tasks waited for disk. High IO stall together with memory stall usually means the kernel is evicting and rereading files because memory is short."),
    pressurePanel("cpu", "CPU stall", "Share of the last 10 seconds tasks waited for a CPU. High CPU stall with low memory stall means too many builds or agents are running at once for 4 CPUs."),

    row("Agents and load"),
    timeseries({
      title: "Agents by state",
      description: "Live agents on this home. Idle agents hold memory for nothing; the hourly cleanup moves the untouched ones to asleep.",
      decimals: 0,
      stack: true,
      legendRight: true,
      series: AGENT_STATES.map(([state]) => [agentsTotal(state, ONE), state]),
      overrides: colored(AGENT_STATES.map(([, color]) => color)),
    }),
    timeseries({
      title: "Load and CPUs",
      description: "One minute load average against the CPU count (dashed). Load far above the CPU count with high memory stall means tasks are blocked on memory, not computing (the 2026-11-02 home sat at 235 on 4 CPUs).",
      series: [
        [one("rudder_home_load", 'window="1m"'), "load 1m"],
        [one("rudder_home_cpu_count"), "CPUs"],
      ],
      overrides: [
        override(byRef("A"), { color: fixed(SLOT[0]) }),
        override(byRef("B"), { color: fixed(MUTED), "custom.lineStyle": { dash: [10, 10], fill: "dash" } }),
      ],
    }),
    timeseries({
      title: "Disk free by mount",
      description: "Free space as a share of each disk. Below the red line on /workspace builds start failing: extend the volume. The root disk filling usually means logs or temp files outside /workspace.",
      unit: "percentunit",
      max: 1,
      series: [
        [ratio(one("rudder_home_disk_free_bytes", 'mount="/workspace"'), one("rudder_home_disk_total_bytes", 'mount="/workspace"')), "/workspace"],
        [ratio(one("rudder_home_disk_free_bytes", 'mount="/"'), one("rudder_home_disk_total_bytes", 'mount="/"')), "/"],
      ],
      thresholds: steps([null, CLEAR], [0.1, STATUS.critical], [0.2, STATUS.warning]),
      calcs: ["lastNotNull", "min"],
      overrides: colored([SLOT[0], SLOT[1]]),
    }),
    timeseries({
      title: "Processes",
      description: "Processes on the machine. A steady climb with flat agent counts means something is leaking processes (watchers, dev servers); the cleanup stops leftovers older than an hour.",
      decimals: 0,
      legend: false,
      series: [[one("rudder_home_processes"), "processes"]],
    }),

    row("Supervisor"),
    timeseries({
      title: "Supervisor event loop delay",
      description: "How late the supervisor's event loop runs. It relays every terminal, so above the amber line typing lags and above the red line sockets time out. Usually caused by memory stall on the machine.",
      size: [8, 8],
      unit: "s",
      series: [
        [one("rudder_home_supervisor_event_loop_delay_seconds", 'stat="p50"'), "p50"],
        [one("rudder_home_supervisor_event_loop_delay_seconds", 'stat="p99"'), "p99"],
        [one("rudder_home_supervisor_event_loop_delay_seconds", 'stat="max"'), "max"],
      ],
      thresholds: steps([null, CLEAR], [0.1, STATUS.warning], [1, STATUS.critical]),
      overrides: colored(RAMP),
    }),
    {
      datasource: DS,
      description: "The control socket over time, as the home sees it and as the control plane sees it. Red stretches are when attached terminals showed \"Cloud machine reconnecting\". If only the control plane shows red, the home thinks it is connected while the control plane does not.",
      fieldConfig: {
        defaults: {
          // Fixed, not thresholds: in thresholds mode Grafana 13 paints the
          // timeline with the threshold color and ignores the mapping colors.
          color: fixed(MUTED),
          custom: { fillOpacity: 60, hideFrom: { legend: false, tooltip: false, viz: false }, lineWidth: 0 },
          mappings: CONNECTED,
        },
        overrides: [],
      },
      options: {
        alignValue: "left",
        legend: { displayMode: "list", placement: "bottom", showLegend: true },
        mergeValues: true,
        rowHeight: 0.8,
        showValue: "never",
        tooltip: { hideZeros: false, mode: "single", sort: "none" },
      },
      size: [8, 8],
      targets: targets([
        [one("rudder_home_control_connected_ratio"), "home"],
        [one("rudder_control_home_control_connected_ratio"), "control plane"],
      ]),
      title: "Control socket",
      type: "state-timeline",
    },
    timeseries({
      title: "Control reconnects, per 5 minutes",
      description: "Times the supervisor reopened its control socket, in 5 minute buckets. Each one dropped every attached terminal briefly. Repeated reconnects follow memory stall or a slow event loop.",
      size: [8, 8],
      decimals: 0,
      bars: true,
      legend: false,
      interval: "5m",
      series: [[`sum(increase(${sel("rudder_home_control_reconnects_total", ONE)}[5m]))`, "reconnects"]],
    }),

    row("Cleanup"),
    timeseries({
      title: "Cleanup runs per hour by trigger",
      description: "Cleanup runs in hourly buckets. Hourly is the schedule, memory means the home ran short and the cleanup ran early, boot is the first run after a start. Memory runs every hour mean the cleanup cannot free enough: the home needs more memory.",
      size: [8, 8],
      decimals: 0,
      bars: true,
      stack: true,
      interval: "1h",
      series: ["hourly", "memory", "boot"].map((trigger) => [
        `sum(increase(${sel("rudder_home_cleanup_runs_total", `trigger="${trigger}"`, ONE)}[1h]))`,
        trigger,
      ]),
      calcs: ["sum"],
      overrides: colored([SLOT[0], SLOT[1], SLOT[2]]),
    }),
    timeseries({
      title: "Cleanup runs that failed or timed out",
      description: "Cleanup runs per hour that did not finish. Any bar here means idle agents were not put to sleep that hour: check the cleanup log on the home (~/.rudder/cleanup).",
      size: [8, 8],
      decimals: 0,
      bars: true,
      stack: true,
      interval: "1h",
      series: [
        [`sum(increase(${sel("rudder_home_cleanup_runs_total", 'outcome="failed"', ONE)}[1h]))`, "failed"],
        [`sum(increase(${sel("rudder_home_cleanup_runs_total", 'outcome="timeout"', ONE)}[1h]))`, "timeout"],
      ],
      calcs: ["sum"],
      overrides: colored([STATUS.critical, STATUS.serious]),
    }),
    timeseries({
      title: "Cleanup actions per hour",
      description: "What the cleanup did: park puts an idle agent to sleep, stop ends a leftover process (a dev server, watcher or test runner an agent forgot).",
      size: [8, 8],
      decimals: 0,
      bars: true,
      stack: true,
      interval: "1h",
      series: ["park", "stop"].map((action) => [
        `sum(increase(${sel("rudder_home_cleanup_actions_total", `action="${action}"`, ONE)}[1h]))`,
        action,
      ]),
      calcs: ["sum"],
      overrides: colored([SLOT[0], SLOT[1]]),
    }),

    row("Seen from the control plane"),
    timeseries({
      title: "Heartbeat age",
      description: "Seconds since the control plane last heard this home's heartbeat (sent every 30 seconds while the home is running). Past the red line the control plane treats the home as gone; if the home's own metrics still arrive, its supervisor is stuck.",
      size: [8, 8],
      unit: "s",
      softMax: 360,
      legend: false,
      series: [[one("rudder_control_home_heartbeat_age_seconds"), "heartbeat age"]],
      thresholds: steps([null, CLEAR], [120, STATUS.warning], [300, STATUS.critical]),
    }),
    timeseries({
      title: "Attached clients",
      description: "Terminals, phones and browsers attached to this home through the control plane. These are the people who see \"reconnecting\" when the control socket drops.",
      size: [8, 8],
      decimals: 0,
      legend: false,
      series: [[one("rudder_control_home_clients"), "clients"]],
    }),
    timeseries({
      title: "Disconnects, per 5 minutes",
      description: "Control socket closes for this home as the control plane counted them. Compare with Control reconnects above: closes the home never noticed point at the network or the control plane.",
      size: [8, 8],
      decimals: 0,
      bars: true,
      legend: false,
      interval: "5m",
      series: [[`sum(increase(${sel("rudder_control_home_disconnects_total", ONE)}[5m]))`, "disconnects"]],
    }),
  ];

  return dashboard({
    uid: "rudder-home",
    title: "Rudder home machine",
    description: "One Rudder Cloud home: what holds its memory, how stalled it is, what its agents are doing, and whether its terminals stay connected.",
    variables: [homeVariable({ multi: false })],
    items,
  });
}

// ---------------------------------------------------------------------------
// Rudder control plane: whether users can reach their homes.
// ---------------------------------------------------------------------------

function controlDashboard() {
  const rate = (matchers = "") => `rate(${HTTP}_count${matchers}[5m])`;
  const quantile = (q, by = "le") => `histogram_quantile(${q}, sum by (${by}) (rate(${HTTP}_bucket[5m])))`;
  const errors = `{http_response_status_code=~"5.."}`;
  const loop = (stat) => `max(rudder_control_event_loop_delay_seconds{stat="${stat}"})`;

  const items = [
    stat({
      title: "Requests per second",
      description: "HTTP requests the control plane served, all routes.",
      expr: `sum(${rate()})`,
      unit: "reqps",
      decimals: 1,
    }),
    stat({
      title: "Latency p95",
      description: "95th percentile request time over the last 5 minutes. Red usually means the event loop is blocked (see Event loop delay) or the database is slow.",
      expr: quantile(0.95),
      unit: "s",
      thresholds: steps([null, CALM], [0.5, STATUS.warning], [2, STATUS.critical]),
    }),
    stat({
      title: "5xx share",
      description: "Share of requests that failed with a server error over the last 5 minutes. Red means users are seeing failures: find the route in the Routes table and check the control plane logs.",
      expr: `(sum(${rate(errors)}) or vector(0)) / sum(${rate()})`,
      unit: "percentunit",
      decimals: 2,
      thresholds: steps([null, CALM], [0.01, STATUS.warning], [0.05, STATUS.critical]),
    }),
    stat({
      title: "Stale heartbeats",
      description: "Homes whose last heartbeat is more than 2 minutes old (they send one every 30 seconds). Amber means a supervisor is stuck or a home died without the control plane noticing: open it from the Homes table below.",
      expr: "count(max by (rudder_home_id) (rudder_control_home_heartbeat_age_seconds) > 120) or vector(0)",
      decimals: 0,
      thresholds: steps([null, CALM], [1, STATUS.warning], [5, STATUS.critical]),
    }),
    stat({
      title: "Home sockets",
      description: "Home supervisors with an open control socket. Lower than the running homes means some homes cannot be reached and their terminals show \"reconnecting\".",
      expr: 'sum(rudder_control_ws_connections{role="home-control"})',
      decimals: 0,
    }),
    stat({
      title: "Client sockets",
      description: "Terminals, phones and browsers connected to the control plane.",
      expr: 'sum(rudder_control_ws_connections{role="client"})',
      decimals: 0,
    }),
    stat({
      title: "Event loop p99",
      description: "How late the control plane's event loop runs at the 99th percentile. It relays every terminal; red means everyone's typing lags. Check process memory and recent deploys.",
      expr: loop("p99"),
      unit: "s",
      thresholds: steps([null, CALM], [0.1, STATUS.warning], [0.5, STATUS.critical]),
    }),
    stat({
      title: "Memory RSS",
      description: "Resident memory of the control plane process. A steady climb across days is a leak; restart during a quiet window and report it.",
      expr: 'max(rudder_control_process_memory_bytes{kind="rss"})',
      unit: "bytes",
      decimals: 0,
    }),

    row("HTTP"),
    timeseries({
      title: "Requests per second by route",
      description: "Request rate per route (ids in paths are replaced by :id). A route that jumps on its own is usually a client retrying in a loop.",
      size: [12, 10],
      unit: "reqps",
      series: [[`sum by (http_route) (${rate()})`, "{{http_route}}"]],
      calcs: ["mean", "max"],
    }),
    timeseries({
      title: "Latency",
      description: "Request time percentiles over all routes. p99 far above p95 means a few slow routes; find them in the Routes table.",
      size: [12, 10],
      unit: "s",
      series: [
        [quantile(0.5), "p50"],
        [quantile(0.95), "p95"],
        [quantile(0.99), "p99"],
      ],
      thresholds: steps([null, CLEAR], [0.5, STATUS.warning], [2, STATUS.critical]),
      overrides: colored(RAMP),
    }),
    timeseries({
      title: "5xx responses per second",
      description: "Server errors per second by route. Any steady rate means a route is broken for someone: check the control plane logs for that route.",
      size: [12, 9],
      unit: "reqps",
      color: STATUS.critical,
      series: [[`sum by (http_route) (${rate(errors)})`, "{{http_route}}"]],
      calcs: ["mean", "max"],
    }),
    timeseries({
      title: "5xx share of requests",
      description: "Server errors as a share of all requests. Above the red line users are seeing failures on a meaningful share of requests.",
      size: [12, 9],
      unit: "percentunit",
      softMax: 0.06,
      color: STATUS.critical,
      legend: false,
      series: [[`(sum(${rate(errors)}) or vector(0)) / sum(${rate()})`, "5xx share"]],
      thresholds: steps([null, CLEAR], [0.01, STATUS.warning], [0.05, STATUS.critical]),
    }),
    table({
      title: "Routes",
      description: "Every route over the last 5 minutes, busiest first. Amber and red cells are slow or failing routes: check the control plane logs for that route.",
      size: [24, 8],
      queries: [
        `sum by (http_route) (${rate()})`,
        quantile(0.95, "http_route, le"),
        `(sum by (http_route) (${rate(errors)}) or (sum by (http_route) (${rate()}) * 0)) / sum by (http_route) (${rate()})`,
      ],
      columns: [
        ["http_route", "Route"],
        ["Value #A", "Requests per second"],
        ["Value #B", "Latency p95"],
        ["Value #C", "5xx share"],
      ],
      sortBy: ["Requests per second", "desc"],
      overrides: [
        column("Requests per second", { unit: "reqps", decimals: 2 }),
        column("Latency p95", { unit: "s", thresholds: steps([null, CLEAR], [0.5, STATUS.warning], [2, STATUS.critical]) }),
        column("5xx share", { unit: "percentunit", decimals: 2, thresholds: steps([null, CLEAR], [0.01, STATUS.warning], [0.05, STATUS.critical]) }),
      ],
    }),

    row("Connections and homes"),
    timeseries({
      title: "WebSocket connections by role",
      description: "Open sockets: home-control is one per connected home supervisor, client is attached terminals, phones and browsers, worker is task workers. A sudden drop in home-control is many homes reconnecting at once, usually a control plane restart.",
      decimals: 0,
      series: [
        ['sum(rudder_control_ws_connections{role="home-control"})', "home-control"],
        ['sum(rudder_control_ws_connections{role="client"})', "client"],
        ['sum(rudder_control_ws_connections{role="worker"})', "worker"],
      ],
      overrides: colored([SLOT[0], SLOT[1], SLOT[2]]),
    }),
    barGauge({
      title: "Homes by state",
      description: "Homes the control plane knows, by state, largest first. Failed homes need someone to look at the Fly machine.",
      size: [6, 8],
      expr: "sort_desc(sum by (state) (rudder_control_homes))",
      legend: "{{state}}",
    }),
    barGauge({
      title: "Sails by state",
      description: "Sails the control plane knows, by state, largest first. Many queued sails with few running means workers are not starting.",
      size: [6, 8],
      expr: "sort_desc(sum by (state) (rudder_control_sails))",
      legend: "{{state}}",
    }),

    row("Process"),
    timeseries({
      title: "Event loop delay",
      description: "How late the control plane's event loop runs. Above the amber line terminal input lags for everyone; above the red line sockets start timing out.",
      unit: "s",
      series: [
        [loop("p50"), "p50"],
        [loop("p99"), "p99"],
        [loop("max"), "max"],
      ],
      thresholds: steps([null, CLEAR], [0.1, STATUS.warning], [0.5, STATUS.critical]),
      overrides: colored(RAMP),
    }),
    timeseries({
      title: "Process memory",
      description: "Resident memory (rss) and JavaScript heap of the control plane process. Heap climbing with rss is a leak in our code; rss climbing alone is native buffers (sockets, SQLite).",
      unit: "bytes",
      series: [
        ['max(rudder_control_process_memory_bytes{kind="rss"})', "rss"],
        ['max(rudder_control_process_memory_bytes{kind="heap"})', "heap"],
      ],
      overrides: colored([SLOT[0], SLOT[1]]),
    }),

    row("Homes"),
    table({
      title: "Home heartbeats",
      description: "Every home the control plane tracks, stalest heartbeat first. Red heartbeat age or Disconnected means a running home's users cannot reach it. Stopped and paused homes do not heartbeat; Control shows their state and the age reads -. Click a home to open its page.",
      size: [12, 10],
      queries: [
        "max by (rudder_home_id) (rudder_control_home_heartbeat_age_seconds)",
        controlView(),
        "max by (rudder_home_id) (rudder_control_home_clients)",
        "sum by (rudder_home_id) (increase(rudder_control_home_disconnects_total[1h]))",
      ],
      columns: [
        ["rudder_home_id", "Home"],
        ["Value #A", "Heartbeat age"],
        ["Value #B", "Control"],
        ["Value #C", "Clients"],
        ["Value #D", "Disconnects, 1h"],
      ],
      sortBy: ["Heartbeat age", "desc"],
      overrides: [
        column("Home", { links: homeLink, width: 140 }),
        column("Heartbeat age", { unit: "s", decimals: 0, thresholds: steps([null, CLEAR], [120, STATUS.warning], [300, STATUS.critical]) }),
        column("Control", CONTROL_VIEW),
        column("Clients", { decimals: 0 }),
        column("Disconnects, 1h", { decimals: 0, thresholds: steps([null, CLEAR], [1, STATUS.warning], [5, STATUS.critical]) }),
      ],
    }),
    timeseries({
      title: "Home disconnects, per 5 minutes",
      description: "Control socket closes per home in 5 minute buckets. Many homes at the same moment is the control plane (a deploy or a stall); one home again and again is that home (open it from the table).",
      size: [12, 10],
      decimals: 0,
      bars: true,
      stack: true,
      interval: "5m",
      series: [["sum by (rudder_home_id) (increase(rudder_control_home_disconnects_total[5m]))", "{{rudder_home_id}}"]],
      calcs: ["sum", "max"],
    }),

    row("Build pool"),
    timeseries({
      title: "Builders by state",
      description: "Burst build pool machines across all homes. Leased builders are running someone's build; idle ones keep their caches and stop themselves after a few quiet minutes. Failed or destroying builders that stay for more than a few minutes mean Fly calls are failing. Empty while RUDDER_BUILDERS is off.",
      decimals: 0,
      stack: true,
      step: true,
      series: [["sum by (state) (rudder_control_builders)", "{{state}}"]],
    }),
    timeseries({
      title: "Builder lease requests, per 5 minutes",
      description: "rudder-burst lease requests by outcome. Cold starts wait for a machine to start or be created; busy means every builder of that home was leased (the client waits and retries), so steady busy bars mean the per-home maximum is too low.",
      decimals: 0,
      bars: true,
      stack: true,
      interval: "5m",
      series: [["sum by (outcome, start) (increase(rudder_control_builder_leases_total[5m]))", "{{outcome}} {{start}}"]],
      calcs: ["sum", "max"],
    }),
  ];

  return dashboard({
    uid: "rudder-control",
    title: "Rudder control plane",
    description: "The Rudder Cloud control plane: HTTP traffic and errors, sockets to homes and clients, its own event loop and memory, and each home's heartbeat.",
    items,
  });
}

// ---------------------------------------------------------------------------
// Output.
// ---------------------------------------------------------------------------

/** filename -> dashboard object. */
export function buildDashboards() {
  return {
    "rudder-fleet.json": fleetDashboard(),
    "rudder-home.json": homeDashboard(),
    "rudder-control.json": controlDashboard(),
  };
}

// Sorted keys so a reordered builder call never shows up as a diff.
function sortKeys(value) {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, sortKeys(value[key])]));
  }
  return value;
}

export function render(dashboard) {
  return `${JSON.stringify(sortKeys(dashboard), null, 2)}\n`;
}

function main(argv) {
  const check = argv.includes("--check");
  const stale = [];
  for (const [file, board] of Object.entries(buildDashboards())) {
    const target = path.join(DASHBOARD_DIR, file);
    const text = render(board);
    let current = null;
    try {
      current = fs.readFileSync(target, "utf8");
    } catch {}
    if (current === text) continue;
    if (check) {
      stale.push(file);
    } else {
      fs.mkdirSync(DASHBOARD_DIR, { recursive: true });
      fs.writeFileSync(target, text);
      process.stdout.write(`wrote ${path.relative(process.cwd(), target)}\n`);
    }
  }
  if (stale.length) {
    process.stderr.write(`stale: ${stale.join(", ")}; run node observability/grafana/build-dashboards.mjs\n`);
    process.exit(1);
  }
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fs.realpathSync(CLI_PATH)) {
  main(process.argv.slice(2));
}
