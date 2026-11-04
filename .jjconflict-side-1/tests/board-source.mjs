// Load current TS for focused board checks without compiling or touching dist.
import fs from "node:fs";
import { registerHooks, stripTypeScriptTypes } from "node:module";

const root = new URL("../src/", import.meta.url).href;
registerHooks({
  resolve(specifier, context, next) {
    if (context.parentURL?.startsWith(root) && specifier.startsWith(".")) {
      const candidate = new URL(specifier.endsWith(".js") ? specifier.slice(0, -3) + ".ts" : specifier + ".ts", context.parentURL);
      if (candidate.href.startsWith(root) && fs.existsSync(candidate)) return { url: candidate.href, shortCircuit: true };
    }
    return next(specifier, context);
  },
  load(url, context, next) {
    if (url.startsWith(root) && url.endsWith(".ts")) return { format: "module", shortCircuit: true,
      source: stripTypeScriptTypes(fs.readFileSync(new URL(url), "utf8"), { mode: "transform", sourceUrl: url }) };
    return next(url, context);
  },
});
