/** Local installed-protocol discovery without importing/refreshing credentials.
 * The caller supplies a dedicated Codex home and owns its private configuration.
 * No thread/turn is started. A model list is capability evidence, not execution
 * qualification or proof that an account may use any listed model. */
import { spawn } from "node:child_process";
import path from "node:path";
import { codexProtocolCapabilities, type ModelCapability } from "./identity.js";

export async function discoverInstalledCodexModels(program: string, codexHome: string,
  timeoutMs = 10_000): Promise<{ backendVersion: string; capabilities: ModelCapability[] }> {
  if (!path.isAbsolute(program) || !path.isAbsolute(codexHome)
    || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 30_000)
    throw new Error("Exact installed Codex program, private home and bounded discovery deadline are required.");
  const child = spawn(program, ["app-server"], { cwd: codexHome,
    env: { HOME: codexHome, CODEX_HOME: codexHome, PATH: "/usr/local/bin:/usr/bin:/bin" },
    stdio: ["pipe", "pipe", "ignore"] });
  let pending: { id: number; resolve: (value: Record<string, unknown>) => void; reject: (error: Error) => void } | undefined;
  let buffer = "", bytes = 0, nextId = 0, closed = false;
  let rejectRun: (error: Error) => void = () => {};
  const failure = () => new Error("Installed Codex model discovery failed; no session was started.");
  const failed = new Promise<never>((_, reject) => { rejectRun = reject; });
  // Every async failure is handled by the race below; never echo backend data,
  // errors or paths that might contain private configuration.
  const fail = () => { pending?.reject(failure()); pending = undefined; rejectRun(failure()); };
  child.stdin.on("error", fail); child.on("error", fail);
  const reaped = new Promise<void>(resolve => child.once("close", () => { closed = true; fail(); resolve(); }));
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (data: string) => {
    bytes += Buffer.byteLength(data);
    if (bytes > 4 * 1024 * 1024) { fail(); return; }
    buffer += data;
    let newline: number;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
      try {
        const message = JSON.parse(line);
        if (pending && message.id === pending.id) {
          const request = pending; pending = undefined;
          if (message.error || !message.result || typeof message.result !== "object") request.reject(failure());
          else request.resolve(message.result);
        }
      } catch { fail(); }
    }
  });
  const send = (message: object) => { child.stdin.write(`${JSON.stringify(message)}\n`); };
  const request = (method: string, params: object) => new Promise<Record<string, unknown>>((resolve, reject) => {
    if (closed || pending) { reject(failure()); return; }
    const id = ++nextId; pending = { id, resolve, reject }; send({ id, method, params });
  });
  const timer = setTimeout(fail, timeoutMs);
  const run = async () => {
    const initialized = await request("initialize", { clientInfo: { name: "rudder-model-profiles", version: "1" } });
    if (typeof initialized.userAgent !== "string" || !initialized.userAgent.trim()) throw failure();
    send({ method: "initialized" });
    const capabilities: ModelCapability[] = [];
    let cursor: string | undefined;
    for (let page = 0; page < 16; page++) {
      const result = await request("model/list", { limit: 100, includeHidden: false, ...(cursor ? { cursor } : {}) });
      capabilities.push(...codexProtocolCapabilities(result));
      if (result.nextCursor === null || result.nextCursor === undefined) return { backendVersion: initialized.userAgent, capabilities };
      if (typeof result.nextCursor !== "string" || !result.nextCursor || result.nextCursor === cursor) throw failure();
      cursor = result.nextCursor;
    }
    throw failure();
  };
  try { return await Promise.race([run(), failed]); }
  finally {
    clearTimeout(timer);
    if (!closed) child.kill("SIGKILL");
    await reaped; // Actual process close, never a timeout-as-reap claim.
  }
}
