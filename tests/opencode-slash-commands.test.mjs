import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { OPENCODE_VERSION } from "../cloud/worker/opencode-commands.mjs";
import { createSlashCommands } from "../cloud/worker/slash-commands.mjs";
import { RudderAgent } from "../native/src/opencode_plugin.mjs";

test("OpenCode's picker separates session actions from project and MCP prompts", async (t) => {
  // The fake server compares against process.cwd(), which resolves macOS's temporary directory symlink.
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-opencode-commands-")));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dockerfile = fs.readFileSync(new URL("../cloud/worker/Dockerfile", import.meta.url), "utf8");
  assert.equal(/^ARG OPENCODE_VERSION=(\S+)$/m.exec(dockerfile)?.[1], OPENCODE_VERSION);
  const bin = path.join(root, "opencode");
  fs.writeFileSync(bin, `#!${process.execPath}
const http = require("node:http");
if (process.argv[2] === "--version") {
  console.log(${JSON.stringify(OPENCODE_VERSION)});
} else {
  const server = http.createServer((req, res) => {
    const token = Buffer.from("opencode:" + process.env.OPENCODE_SERVER_PASSWORD).toString("base64");
    const url = new URL(req.url, "http://127.0.0.1");
    if (!process.env.OPENCODE_SERVER_PASSWORD || req.headers.authorization !== "Basic " + token) {
      res.writeHead(401).end();
    } else if (url.pathname !== "/command" || url.searchParams.get("directory") !== process.cwd()) {
      res.writeHead(404).end();
    } else {
      res.end(JSON.stringify([
        { name: "greet", description: "Greet a teammate", hints: ["$1"], source: "command" },
        { name: "report", description: "Write a report", hints: [], source: "command" },
        { name: "compact", description: "Would shadow the session action", hints: [], source: "command" },
        { name: "docs_lookup", description: "Read a doc", hints: [], source: "mcp" },
        { name: "tidy", description: "A skill", hints: [], source: "skill" },
      ]));
    }
  });
  server.listen(0, "127.0.0.1", () => {
    console.log("opencode server listening on http://127.0.0.1:" + server.address().port);
  });
}
`, { mode: 0o700 });
  const cwd = path.join(root, "project with spaces");
  const runDir = path.join(cwd, ".rudder", "runs", "run-1");
  fs.mkdirSync(runDir, { recursive: true });
  fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify({
    backend: "opencode", workspace: { path: cwd }, session: { nativeSessionId: "ses_x" },
  }));
  const source = createSlashCommands({
    envFor: (backend, account) => {
      assert.deepEqual([backend, account], ["opencode", "main"]);
      return { ...process.env, RUDDER_OPENCODE_BIN: bin };
    },
    log: () => {},
    projectDirs: () => [],
    file: path.join(root, "commands.json"),
  });
  const { backend, version, commands } = await source.forRun(cwd, "run-1");
  assert.equal(backend, "opencode");
  assert.equal(version, OPENCODE_VERSION);
  const byName = new Map(commands.map((item) => [item.name, item]));
  assert.equal(byName.get("compact").opens, null);
  assert.deepEqual(byName.get("compact").aliases, ["summarize"]);
  assert.equal(byName.get("compact").prompt, undefined);
  assert.equal(byName.get("models").opens, "model");
  assert.equal(byName.get("greet").hint, "<arg 1>");
  assert.equal(byName.get("greet").args, "required");
  assert.equal(byName.get("report").prompt, true);
  assert.equal(byName.get("docs_lookup").label, "docs_lookup:mcp");
  assert.equal(byName.has("tidy"), false);
});

test("a resumed OpenCode session dispatches compaction and project commands through the CLI API", async (t) => {
  const keys = ["RUDDER_OPENCODE_PROMPT", "RUDDER_OPENCODE_MODEL", "RUDDER_OPENCODE_SESSION"];
  const saved = new Map(keys.map((key) => [key, process.env[key]]));
  t.after(() => {
    for (const [key, value] of saved) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    delete globalThis.rudderAgentLoaded;
  });
  process.env.RUDDER_OPENCODE_SESSION = "ses_x";
  process.env.RUDDER_OPENCODE_MODEL = "openai/model-x";
  async function dispatched(prompt) {
    process.env.RUDDER_OPENCODE_PROMPT = prompt;
    delete globalThis.rudderAgentLoaded;
    let posted;
    const request = new Promise((resolve) => { posted = resolve; });
    const plugin = await RudderAgent({
      client: { _client: {
        get: async () => ({ data: [{ name: "greet" }] }),
        post: async ({ url, body }) => { posted({ url, body }); },
      } },
    });
    return { plugin, request: await request };
  }
  const compact = await dispatched("/summarize");
  assert.deepEqual(compact.request, {
    url: "/session/ses_x/summarize",
    body: { providerID: "openai", modelID: "model-x" },
  });
  const greet = await dispatched("/greet teammate");
  assert.equal(greet.request.url, "/session/ses_x/command");
  assert.equal(greet.request.body.command, "greet");
  assert.equal(greet.request.body.arguments, "teammate");
  const output = { parts: [{ type: "text", text: "Greet teammate", metadata: { existing: true } }] };
  await greet.plugin["command.execute.before"]({ command: "greet" }, output);
  assert.deepEqual(output.parts[0].metadata, { existing: true, rudderSlashCommand: "greet" });
});
