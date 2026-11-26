// What a chat notice shows (app.js paintNotice): the text a backend recorded,
// cut into plain strings and links. A web address becomes a short link named
// for its site, so a long key or query string never fills the note. An error
// Rudder knows is said in plain words with the one thing to do about it.

const URL_RE = /https:\/\/[^\s<>"')\]]+/g;
// OpenRouter, when the key's limit or the account's credit cannot cover the reply.
const OPENROUTER_CREDIT = /requires more credits, or fewer max_tokens\. You requested up to (\d+) tokens, but can only afford (\d+)\./;

const count = (digits) => Number(digits).toLocaleString("en-US");

// -> [string | { href, label }]
export function noticeParts(text) {
  const said = String(text ?? "");
  const credit = OPENROUTER_CREDIT.exec(said);
  if (credit) {
    // OpenRouter names the key's limit whenever the key has one, even when it
    // is the account that has run out, so both ways on are offered.
    const key = said.match(URL_RE)?.find((url) => /^https:\/\/openrouter\.ai\/.*\/keys\//.test(url));
    const parts = [`OpenRouter is out of credit. This reply could use up to ${count(credit[1])} tokens and the credit left pays for ${count(credit[2])}. `, { href: "https://openrouter.ai/settings/credits", label: "Add credit" }];
    if (key) parts.push(" or ", { href: key.replace(/[.,;:!?]+$/, ""), label: "raise the key's limit" });
    return parts;
  }
  const parts = [];
  let at = 0;
  for (const found of said.matchAll(URL_RE)) {
    // A full stop or comma after an address ends the sentence, not the address.
    const href = found[0].replace(/[.,;:!?]+$/, "");
    if (found.index > at) parts.push(said.slice(at, found.index));
    parts.push({ href, label: new URL(href).hostname.replace(/^www\./, "") });
    at = found.index + href.length;
  }
  if (at < said.length) parts.push(said.slice(at));
  return parts;
}
