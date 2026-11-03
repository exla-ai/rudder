/** Standalone trusted host helper, invoked only by the off-App native owner.
 * No public CLI route. Secret output belongs exclusively to inherited FD3. */
type Reply = { ok: false; error: { code: string; message: string } } | {
  ok: true; metadata: { formatVersion: 1; profileId: string; backend: "codex" | "claude";
    credentialKind: "api_key" | "oauth" | "token"; provider: "openai" | "openai-codex" | "anthropic" };
};
const failed = (code: string, message: string): Reply => ({ ok: false, error: { code, message } });
let reply: Reply;
let imported = false;
try {
  const args = process.argv.slice(2);
  const [operation, profileId, backend] = args;
  if (args.length === 1 && operation === "import-codex-oauth-access-snapshot") {
    const store = await import("./managed-codex-access-snapshot-store.js");
    await store.importCodexOAuthAccessSnapshotStdin(process.stdin);
    imported = true;
    // The import ACK has no profile, bearer or credential metadata.
    reply = failed("UNUSED", "Unused import reply.");
  } else if (args.length !== 3 || !profileId || (backend !== "codex" && backend !== "claude")
    || (operation !== "inspect" && operation !== "handoff-api-key"
      && operation !== "handoff-codex-oauth-access-snapshot")) {
    reply = failed("INVALID_REQUEST", "An exact standalone profile operation and selection are required.");
  } else {
    // Dynamic import keeps import/loader failures inside the static error boundary.
    const api = await import("./managed-task-profile.js");
    const selection: { profileId: string; backend: "codex" | "claude" } = { profileId, backend };
    reply = operation === "inspect" ? await api.inspectManagedTaskProfile(selection)
      : operation === "handoff-api-key" ? await api.handoffManagedTaskApiKey(selection, 3)
      : await api.handoffManagedTaskCodexOAuthAccessSnapshot(selection, 3);
  }
} catch { reply = failed("HELPER_UNAVAILABLE", "The standalone private profile helper is unavailable."); }
process.exitCode = imported || reply.ok ? 0 : 1;
process.stdout.on("error", () => { process.exitCode = 1; });
try {
  await new Promise<void>((resolve, reject) => process.stdout.write(imported
    ? "CODEX_OAUTH_ACCESS_SNAPSHOT_IMPORTED_V1\n" : `${JSON.stringify(reply)}\n`,
    error => error ? reject(error) : resolve()));
} catch { process.exitCode = 1; }
export {}; // Top-level await; no process.exit truncation or stderr/cause output.
