// The Grafana dashboards (observability/grafana/build-dashboards.mjs): the
// committed JSON is exactly what the generator writes, every query goes to the
// one Prometheus datasource, and every metric name is one that
// observability/metrics.md defines, so a renamed metric fails here instead of
// showing up later as an empty panel.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { CLI_PATH, DASHBOARD_DIR, buildDashboards, render } from "../observability/grafana/build-dashboards.mjs";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const CATALOG = fs.readFileSync(path.join(ROOT, "observability", "metrics.md"), "utf8");
const DS = { type: "prometheus", uid: "prometheus" };
const UIDS = {
  "rudder-fleet.json": "rudder-fleet",
  "rudder-home.json": "rudder-home",
  "rudder-control.json": "rudder-control",
};
const METRIC_TOKEN = /\b(rudder_[a-z0-9_]+|http_server_request_duration_seconds(_bucket|_sum|_count)?)\b/g;
// Built from its code point so this file has none either.
const EM_DASH = String.fromCharCode(0x2014);

const boards = buildDashboards();

/** Backticked names in the catalog's "Prometheus name" and "Prometheus label" columns. */
function catalogNames(markdown) {
  const names = new Set();
  let columns = null;
  for (const line of markdown.split("\n")) {
    if (!line.startsWith("|")) {
      columns = null;
      continue;
    }
    // Cells may hold escaped pipes (memory\|io\|cpu).
    const cells = line.split(/(?<!\\)\|/).slice(1, -1).map((cell) => cell.trim());
    if (!columns) {
      columns = cells.flatMap((cell, i) => (/^Prometheus (name|label)$/.test(cell) ? [i] : []));
      continue;
    }
    for (const i of columns) {
      for (const [, token] of (cells[i] ?? "").matchAll(/`([^`]+)`/g)) {
        // `http_server_request_duration_seconds_{bucket,sum,count}`
        const brace = /^(.*)_\{([a-z,]+)\}$/.exec(token);
        if (!brace) {
          names.add(token);
          continue;
        }
        names.add(brace[1]);
        for (const suffix of brace[2].split(",")) names.add(`${brace[1]}_${suffix}`);
      }
    }
  }
  return names;
}

function allPanels(board) {
  return board.panels.flatMap((panel) => [panel, ...(panel.panels ?? [])]);
}

function expressions(board) {
  const exprs = allPanels(board).flatMap((panel) => (panel.targets ?? []).map((t) => t.expr));
  for (const variable of board.templating.list) {
    if (variable.definition) exprs.push(variable.definition);
    if (variable.query?.query) exprs.push(variable.query.query);
  }
  return exprs;
}

test("the committed dashboards are what the generator writes", () => {
  assert.deepEqual(fs.readdirSync(DASHBOARD_DIR).filter((f) => f.endsWith(".json")).sort(), Object.keys(UIDS).sort());
  for (const [file, board] of Object.entries(boards)) {
    const committed = fs.readFileSync(path.join(DASHBOARD_DIR, file), "utf8");
    // ok() rather than equal(): a diff of two 2000 line files helps nobody.
    assert.ok(committed === render(board), `${file} is stale: run node observability/grafana/build-dashboards.mjs`);
  }
});

test("dashboard uids are stable", () => {
  for (const [file, board] of Object.entries(boards)) {
    assert.equal(board.uid, UIDS[file]);
    assert.deepEqual(board.tags, ["rudder"]);
    assert.equal(board.refresh, "30s");
    assert.deepEqual(board.time, { from: "now-6h", to: "now" });
  }
});

test("every panel queries the Prometheus datasource", () => {
  for (const [file, board] of Object.entries(boards)) {
    for (const panel of allPanels(board)) {
      // Rows only group panels; they hold no queries.
      if (panel.type === "row") continue;
      assert.deepEqual(panel.datasource, DS, `${file}: ${panel.title}`);
      assert.ok(panel.targets?.length, `${file}: ${panel.title} has no queries`);
      for (const t of panel.targets) {
        assert.deepEqual(t.datasource, DS, `${file}: ${panel.title} ${t.refId}`);
        assert.ok(t.expr, `${file}: ${panel.title} ${t.refId} has no expr`);
      }
    }
    for (const variable of board.templating.list) assert.deepEqual(variable.datasource, DS, `${file}: $${variable.name}`);
  }
});

test("panel ids are unique and panels fit the grid without overlapping", () => {
  for (const [file, board] of Object.entries(boards)) {
    const panels = allPanels(board);
    const ids = panels.map((p) => p.id);
    assert.equal(new Set(ids).size, ids.length, `${file}: duplicate panel id`);
    const taken = new Set();
    for (const { gridPos: g, title } of panels) {
      assert.ok(g.x >= 0 && g.w > 0 && g.x + g.w <= 24, `${file}: ${title} leaves the 24 column grid`);
      for (let x = g.x; x < g.x + g.w; x++) {
        for (let y = g.y; y < g.y + g.h; y++) {
          assert.ok(!taken.has(`${x},${y}`), `${file}: ${title} overlaps another panel`);
          taken.add(`${x},${y}`);
        }
      }
    }
  }
});

test("the catalog parser finds the metric and label names", () => {
  const names = catalogNames(CATALOG);
  for (const name of [
    "rudder_home_memory_total_bytes",
    "rudder_home_pressure_ratio",
    "rudder_control_home_disconnects_total",
    "rudder_scaler_decision",
    "http_server_request_duration_seconds_bucket",
    "rudder_home_id",
  ]) {
    assert.ok(names.has(name), name);
  }
  assert.ok(!names.has("rudder_home_memory_used_bytes"));
  assert.ok(!names.has("rudder.home.memory.total"), "OTel names are not Prometheus names");
});

test("every metric a dashboard queries is defined in observability/metrics.md", () => {
  const names = catalogNames(CATALOG);
  const unknown = new Set();
  let seen = 0;
  for (const [file, board] of Object.entries(boards)) {
    for (const expr of expressions(board)) {
      for (const [token] of expr.matchAll(METRIC_TOKEN)) {
        seen++;
        if (!names.has(token) && !names.has(token.replace(/_(bucket|sum|count)$/, ""))) unknown.add(`${file}: ${token}`);
      }
    }
  }
  assert.ok(seen > 50, "the dashboards should query catalog metrics");
  assert.deepEqual([...unknown], []);
});

test("table overrides name real columns", () => {
  for (const [file, board] of Object.entries(boards)) {
    for (const panel of allPanels(board).filter((p) => p.type === "table")) {
      const organize = panel.transformations.find((t) => t.id === "organize").options;
      const columns = Object.keys(organize.indexByName).map((source) => organize.renameByName[source] ?? source);
      for (const { matcher } of panel.fieldConfig.overrides) {
        assert.ok(columns.includes(matcher.options), `${file}: ${panel.title} overrides unknown column ${matcher.options}`);
      }
      for (const { displayName } of panel.options.sortBy) assert.ok(columns.includes(displayName), `${file}: ${panel.title} sorts by ${displayName}`);
    }
  }
});

// Grafana colors null with the base threshold step, which is red on every
// "low is bad" column: a home with no /workspace read as an empty red cell.
test("a missing value never wears a status color", () => {
  const missingMapping = (mappings) => mappings?.find((m) => m.type === "special" && m.options.match === "null+nan");
  for (const [file, board] of Object.entries(boards)) {
    for (const panel of allPanels(board)) {
      if (panel.type === "stat") {
        const mapping = missingMapping(panel.fieldConfig.defaults.mappings);
        assert.ok(mapping, `${file}: ${panel.title} has no missing value mapping`);
        assert.equal(mapping.options.result.color, "text", `${file}: ${panel.title}`);
      }
      if (panel.type !== "table") continue;
      assert.ok(missingMapping(panel.fieldConfig.defaults.mappings), `${file}: ${panel.title} defaults`);
      for (const { matcher, properties } of panel.fieldConfig.overrides) {
        const props = Object.fromEntries(properties.map((p) => [p.id, p.value]));
        if (props["custom.cellOptions"]?.type !== "color-background") continue;
        const mapping = missingMapping(props.mappings);
        assert.ok(mapping, `${file}: ${panel.title} column ${matcher.options} has no missing value mapping`);
        assert.equal(mapping.options.result.color, "transparent", `${file}: ${panel.title} column ${matcher.options}`);
      }
    }
  }
});

test("no em dashes in the dashboards or their generator", () => {
  for (const [file, board] of Object.entries(boards)) assert.ok(!render(board).includes(EM_DASH), file);
  assert.ok(!fs.readFileSync(CLI_PATH, "utf8").includes(EM_DASH), "build-dashboards.mjs");
});
