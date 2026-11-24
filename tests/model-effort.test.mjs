import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import { groupAntigravityEfforts, matchesModel, modelRequest, variantEffort } from "../cloud/app/model-effort.js";

test("the Antigravity picker groups advertised efforts and submits their actual CLI IDs", () => {
  const fixture = JSON.parse(fs.readFileSync(new URL("./fixtures/agy-1.5.0-models.json", import.meta.url), "utf8"));
  const catalog = fixture.command.data.models.map(({ id, label }) => ({ value: id, label, efforts: [] }));
  const neon = { value: "gemini-4-neon-xhigh", label: "Gemini 4 Neon (Extra)", efforts: [] };
  const rows = groupAntigravityEfforts([neon, ...catalog]);
  assert.deepEqual(rows.map(({ label }) => label), [
    "Gemini 4 Neon", "Gemini 3.8 Flash", "Claude Sonnet 4.6 (Thinking)", "Claude Opus 4.6 (Thinking)", "GPT-OSS 120B",
  ]);
  const [neonRow, flash, sonnet] = rows;
  assert.deepEqual(neonRow.efforts, ["xhigh"]);
  assert.deepEqual(modelRequest(neonRow, "xhigh"), { model: neon.value, effort: null, name: neon.label });
  assert.deepEqual(flash.efforts, ["high", "medium", "low"]);
  assert.ok(matchesModel(flash, "gemini-3.8-flash-low"));
  assert.equal(variantEffort(flash, "gemini-3.8-flash-low"), "low");
  assert.equal(variantEffort(flash, "gemini-3.8-flash-max"), undefined);
  assert.deepEqual(modelRequest(flash, "medium"), {
    model: "gemini-3.8-flash-medium", effort: null, name: "Gemini 3.8 Flash (Medium)",
  });
  assert.deepEqual(modelRequest(sonnet, "auto"), { model: "claude-sonnet-4-6", effort: "auto", name: sonnet.label });
});

test("Gemini 4 Neon exposes the CLI's four selectable thinking levels", () => {
  const rows = groupAntigravityEfforts(["low", "medium", "high", "xhigh"].map((effort) => ({
    value: `gemini-4-neon-${effort}`,
    label: `Gemini 4 Neon (${effort === "xhigh" ? "Extra" : effort[0].toUpperCase() + effort.slice(1)})`,
    efforts: [],
  })));
  assert.equal(rows.length, 1);
  assert.equal(rows[0].label, "Gemini 4 Neon");
  // The picker shows this id: the effort is chosen apart from the model.
  assert.equal(rows[0].family, "gemini-4-neon");
  assert.deepEqual(rows[0].efforts, ["low", "medium", "high", "xhigh"]);
  assert.deepEqual(modelRequest(rows[0], "xhigh"), {
    model: "gemini-4-neon-xhigh", effort: null, name: "Gemini 4 Neon (Extra)",
  });
});
