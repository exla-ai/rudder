// What a reply to highlighted text sends and how the chat reads it back
// (cloud/app/quote-text.js).
import test from "node:test";
import assert from "node:assert/strict";
import { cleanQuote, parseQuoted, quoteMessage } from "../cloud/app/quote-text.js";
import { sentKey } from "../cloud/app/pending.js";

test("a reply is the quote's lines behind \"> \", a blank line, then the prompt", () => {
  assert.equal(quoteMessage("The shell sizes itself\nto the viewport.", "Why not 100vh?"),
    "> The shell sizes itself\n> to the viewport.\n\nWhy not 100vh?");
});

test("a selection's blank lines quote as a bare \">\", runs of them cut to one", () => {
  assert.equal(quoteMessage("  First paragraph.  \r\n\r\n\r\n\nSecond one.\n", " and? "),
    "> First paragraph.\n>\n> Second one.\n\nand?");
  assert.equal(cleanQuote("\n a b \n"), "a b");
});

test("no quote sends the prompt alone, and no prompt sends the quote alone", () => {
  assert.equal(quoteMessage("  \n ", "just this"), "just this");
  assert.equal(quoteMessage("only a quote", ""), "> only a quote");
});

test("a sent reply reads back as its quote and its prompt", () => {
  const sent = quoteMessage("line one\n\nline two", "Explain this.\n\nThen fix it.");
  assert.deepEqual(parseQuoted(sent), { quote: "line one\n\nline two", body: "Explain this.\n\nThen fix it." });
  assert.deepEqual(parseQuoted("> only a quote"), { quote: "only a quote", body: "" });
});

test("text that is not a leading quote ended by a blank line stays as it is", () => {
  for (const text of ["plain message", "> carries on\ninto this line", "intro\n\n> quoted later", ">\n>\n\nempty quote", ""]) {
    assert.deepEqual(parseQuoted(text), { quote: "", body: text });
  }
});

test("a reply the dashboard trimmed on the way still settles its sent bubble", () => {
  // native sanitize_steer_instruction: every line's whitespace collapsed and trimmed.
  const sent = quoteMessage("fn  main()  {\n    run();\n}", "What does run do?");
  const arrived = sent.split("\n").map((line) => line.split(/\s+/).filter(Boolean).join(" ")).join("\n").trim();
  assert.equal(sentKey(arrived), sentKey(sent));
  assert.equal(parseQuoted(arrived).body, "What does run do?");
});
