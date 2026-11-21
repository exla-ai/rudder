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
