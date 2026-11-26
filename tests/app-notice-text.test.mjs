import assert from "node:assert/strict";
import test from "node:test";
import { noticeParts } from "../cloud/app/notice-text.js";

test("an OpenRouter credit error is said in plain words with both ways on", () => {
  const key = "https://openrouter.ai/workspaces/default/keys/abc123";
  const parts = noticeParts(`This request requires more credits, or fewer max_tokens. You requested up to 32000 tokens, but can only afford 40. To increase, visit ${key} and adjust the key's total limit`);
  assert.deepEqual(parts, [
    "OpenRouter is out of credit. This reply could use up to 32,000 tokens and the credit left pays for 40. ",
    { href: "https://openrouter.ai/settings/credits", label: "Add credit" },
    " or ",
    { href: key, label: "raise the key's limit" },
  ]);
});

test("an address in any other notice becomes a link named for its site", () => {
  assert.deepEqual(noticeParts("Rate limited. See https://www.example.com/docs/limits?x=1. Try later"), [
    "Rate limited. See ",
    { href: "https://www.example.com/docs/limits?x=1", label: "example.com" },
    ". Try later",
  ]);
  assert.deepEqual(noticeParts("Stopped"), ["Stopped"]);
});
