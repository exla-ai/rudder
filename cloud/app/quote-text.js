// What a reply to highlighted text sends, and reading it back for the chat
// (quote-reply.js). No imports: tests load it in node.
//
// A reply is Markdown: each line of the quote behind "> " (a blank line is a
// bare ">"), a blank line, then the prompt. It goes to the agent through the
// chat's one send path, so Claude Code and Codex read an ordinary blockquote.
// On the way the dashboard trims every line and collapses runs of spaces
// (native sanitize_steer_instruction); this format comes through that as is.

/** Selected text as a quote: its line breaks kept, runs of blank lines cut to one, ends trimmed. */
export function cleanQuote(text) {
  return String(text ?? "")
    .replace(/\r\n?/g, "\n")
    .replace(/ /g, " ")
    .split("\n").map((line) => line.trimEnd()).join("\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

/** The message a reply sends: the quote as a Markdown blockquote, a blank line, the prompt. */
export function quoteMessage(quote, prompt) {
  const q = cleanQuote(quote);
  const p = String(prompt ?? "").trim();
  if (!q) return p;
  const block = q.split("\n").map((line) => (line.trim() ? `> ${line}` : ">")).join("\n");
  return p ? `${block}\n\n${p}` : block;
}

/**
 * A message read back: `{ quote, body }` when it starts with a blockquote
 * that a blank line ends (or that is the whole message), else
 * `{ quote: "", body: text }`. Quote lines lose their "> ".
 */
export function parseQuoted(text) {
  const src = String(text ?? "").replace(/\r\n?/g, "\n");
  const none = { quote: "", body: src };
  const lines = src.split("\n");
  let i = 0;
  while (i < lines.length && lines[i].startsWith(">")) i += 1;
  // Markdown carries a quote on into the next line unless a blank line ends it.
  if (i === 0 || (i < lines.length && lines[i].trim() !== "")) return none;
  const quote = lines.slice(0, i).map((line) => line.replace(/^> ?/, "")).join("\n").trim();
  if (!quote) return none;
  return { quote, body: lines.slice(i).join("\n").trim() };
}
