// Carrying out the scaler's decisions on Fly: resize a home machine's memory
// or extend its /workspace volume, through the Fly Machines API.
//
// The labels in Prometheus are reported by the machine itself, so a machine
// could claim any home. Before touching anything this reads the machine from
// Fly and refuses unless Fly's own config says it is that home
// (config.env.RUDDER_HOME is "1" and config.env.RUDDER_WORKSPACE_ID is the
// home id). Fly's config is the authority, not the metrics.
//
// A memory resize restarts the machine. It runs only for a decision the policy
// marked "recommended", which for a resize means no agent worked or waited for
// 15 minutes. After the identity check it takes the machine's Fly lease, reads
// the config again under it and sends it back with current_version, so a
// concurrent image update by the control plane is not overwritten with a
// stale config.
//
// A volume extend never restarts anything. When Fly says the machine needs a
// restart to see the new size, that is reported and left to the home's next
// idle restart.
//
// With apply false (the default) this only reads: GETs with a token, nothing
// without one. It returns { dryRun: true, wouldDo } instead of changing anything.

const DEFAULT_BASE = "https://api.machines.dev/v1";
const WORKSPACE_MOUNT = "/workspace";

export function createFlyExecutor({
  token = null,
  app,
  fetch: fetchImpl = globalThis.fetch,
  apply = false,
  baseUrl = DEFAULT_BASE,
  timeoutMs = 30_000,
  leaseTtlSeconds = 120,
} = {}) {
  if (!app) throw new Error("createFlyExecutor needs the Fly app name");
  const live = Boolean(apply && token);
  const machinePath = (id) => `/apps/${encodeURIComponent(app)}/machines/${encodeURIComponent(id)}`;
  const volumePath = (id) => `/apps/${encodeURIComponent(app)}/volumes/${encodeURIComponent(id)}`;

  async function call(method, pathname, { body, nonce } = {}) {
    if (!live && method !== "GET") throw new Error(`dry run must not send ${method} ${pathname}`);
    const headers = { Authorization: authorization(token), Accept: "application/json" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (nonce) headers["fly-machine-lease-nonce"] = nonce;
    let response;
    try {
      response = await fetchImpl(`${baseUrl}${pathname}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (error) {
      // A request that timed out may still have been carried out by Fly.
      const why = error?.name === "TimeoutError" ? `no answer in ${timeoutMs} ms, outcome unknown` : (error?.cause?.code || error?.cause?.message || error?.message || String(error));
      throw new Error(`Fly ${method} ${pathname}: ${why}`);
    }
    const text = await response.text();
    let json = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      // not JSON; the text goes into the error below
    }
    return { status: response.status, ok: response.ok, json, text };
  }

  function failure(method, pathname, reply) {
    const detail = reply.json?.error || reply.text.trim().slice(0, 200) || "no detail";
    return new Error(`Fly ${method} ${pathname}: HTTP ${reply.status}: ${detail}`);
  }

  async function getMachine(id) {
    const reply = await call("GET", machinePath(id));
    if (reply.status === 404) return null;
    if (!reply.ok) throw failure("GET", machinePath(id), reply);
    return reply.json;
  }

  async function resize(decision) {
    const targetMb = decision.target?.memoryMb;
    if (!Number.isInteger(targetMb) || targetMb <= 0) return refuse("the decision has no memory target");
    const request = `POST ${machinePath(decision.machineId)}`;
    if (!token) {
      return { ok: true, dryRun: true, verified: false, wouldDo: { request, change: `guest.memory_mb to ${targetMb}`, restartsMachine: true } };
    }
    // Check first without a lease, so a machine that is not this home is
    // never leased, let alone changed.
    const first = planResize(await getMachine(decision.machineId), decision, targetMb);
    if (first.refused) return refuse(first.refused);
    if (!live) {
      if (first.done) return { ok: true, dryRun: true, verified: true, alreadyDone: true, memoryMb: first.currentMb };
      return { ok: true, dryRun: true, verified: true, wouldDo: { request, change: `guest.memory_mb ${first.currentMb} -> ${targetMb}`, restartsMachine: true } };
    }

    const leasePath = `${machinePath(decision.machineId)}/lease`;
    const lease = await call("POST", leasePath, { body: { ttl: leaseTtlSeconds, description: "rudder-scaler memory resize" } });
    if (lease.status === 409) return refuse("another change holds the machine's lease; trying again later");
    if (!lease.ok) throw failure("POST", leasePath, lease);
    const nonce = lease.json?.data?.nonce;
    if (typeof nonce !== "string" || !nonce) throw new Error("Fly granted the lease without a nonce");
    try {
      // Read the config under the lease so nothing else can change it before
      // the update below, and send it back whole with only the memory changed.
      const machine = await getMachine(decision.machineId);
      const plan = planResize(machine, decision, targetMb);
      if (plan.refused) return refuse(plan.refused);
      if (plan.done) return { ok: true, applied: true, reconciled: true, memoryMb: { from: plan.currentMb, to: targetMb } };
      const config = structuredClone(machine.config);
      config.guest = { ...config.guest, memory_mb: targetMb };
      const body = { config };
      if (machine.instance_id) body.current_version = machine.instance_id;
      const update = await call("POST", machinePath(decision.machineId), { body, nonce });
      if (!update.ok) throw failure("POST", machinePath(decision.machineId), update);
      return {
        ok: true,
        applied: true,
        memoryMb: { from: plan.currentMb, to: targetMb },
        instanceId: update.json?.instance_id ?? null,
        machineState: update.json?.state ?? null,
      };
    } finally {
      try {
        await call("DELETE", leasePath, { nonce });
      } catch {
        // The lease expires on its own after leaseTtlSeconds.
      }
    }
  }

  async function extend(decision) {
    const targetGb = decision.target?.sizeGb;
    if (!Number.isInteger(targetGb) || targetGb <= 0) return refuse("the decision has no volume size target");
    if (!token) {
      return {
        ok: true,
        dryRun: true,
        verified: false,
        wouldDo: { request: `PUT /apps/${app}/volumes/<the ${WORKSPACE_MOUNT} volume of ${decision.machineId}>/extend`, change: `size_gb to ${targetGb}`, restartsMachine: false },
      };
    }
    const machine = await getMachine(decision.machineId);
    const refused = identityRefusal(machine, decision);
    if (refused) return refuse(refused);
    const mount = (machine.config?.mounts ?? []).find((entry) => entry?.path === WORKSPACE_MOUNT || entry?.path === `${WORKSPACE_MOUNT}/`);
    if (!mount?.volume) return refuse(`the machine has no volume mounted at ${WORKSPACE_MOUNT}`);
    const volume = await call("GET", volumePath(mount.volume));
    if (!volume.ok) throw failure("GET", volumePath(mount.volume), volume);
    const attached = volume.json?.attached_machine_id;
    if (attached && attached !== machine.id) return refuse(`volume ${mount.volume} is attached to another machine`);
    const currentGb = volume.json?.size_gb;
    if (Number.isFinite(currentGb) && currentGb >= targetGb) {
      return live
        ? { ok: true, applied: true, reconciled: true, volumeId: mount.volume, sizeGb: { from: currentGb, to: currentGb } }
        : { ok: true, dryRun: true, verified: true, alreadyDone: true, volumeId: mount.volume, sizeGb: currentGb };
    }
    const extendPath = `${volumePath(mount.volume)}/extend`;
    if (!live) {
      return { ok: true, dryRun: true, verified: true, wouldDo: { request: `PUT ${extendPath}`, change: `size_gb ${currentGb ?? "?"} -> ${targetGb}`, restartsMachine: false } };
    }
    const reply = await call("PUT", extendPath, { body: { size_gb: targetGb } });
    if (!reply.ok) throw failure("PUT", extendPath, reply);
    return {
      ok: true,
      applied: true,
      volumeId: mount.volume,
      sizeGb: { from: currentGb ?? null, to: reply.json?.volume?.size_gb ?? targetGb },
      needsRestart: Boolean(reply.json?.needs_restart),
    };
  }

  function refuse(reason) {
    return live ? { ok: false, refused: reason } : { ok: false, dryRun: true, refused: reason };
  }

  /**
   * Carry out one decision, or in a dry run say what would happen. Never
   * throws: returns { ok, applied?, dryRun?, wouldDo?, refused?, error? }.
   */
  async function execute(decision) {
    try {
      if (!decision?.homeId || !decision.machineId) return refuse("the decision names no home or no Fly machine");
      if (decision.state !== "recommended") return refuse(`the decision is ${decision.state}, not recommended`);
      if (decision.action === "scale-up-memory" || decision.action === "scale-down-memory") return await resize(decision);
      if (decision.action === "extend-volume") return await extend(decision);
      return refuse(`there is nothing to execute for ${decision.action}`);
    } catch (error) {
      return live ? { ok: false, error: error.message } : { ok: false, dryRun: true, error: error.message };
    }
  }

  return { execute, apply: live };
}

/** Why Fly's view of the machine rules out acting for this home, or null. */
export function identityRefusal(machine, decision) {
  if (!machine) return `Fly has no machine ${decision.machineId}`;
  if (machine.id && machine.id !== decision.machineId) return `Fly returned machine ${machine.id} for ${decision.machineId}`;
  const env = machine.config?.env ?? {};
  if (env.RUDDER_HOME !== "1") return "Fly's config for the machine does not mark it as a home (RUDDER_HOME is not 1)";
  if (env.RUDDER_WORKSPACE_ID !== decision.homeId) return `Fly's config for the machine names a different home than ${decision.homeId}`;
  return null;
}

// What a resize would change, or why it must not happen.
function planResize(machine, decision, targetMb) {
  const refused = identityRefusal(machine, decision);
  if (refused) return { refused };
  if (machine.state !== "started") return { refused: `the machine is ${machine.state || "in an unknown state"}, not started` };
  const currentMb = machine.config?.guest?.memory_mb;
  if (!Number.isInteger(currentMb)) return { refused: "Fly's config for the machine has no guest.memory_mb" };
  if (currentMb === targetMb) return { done: true, currentMb };
  const up = decision.action === "scale-up-memory";
  if (up ? currentMb > targetMb : currentMb < targetMb) {
    return { refused: `the machine already has ${currentMb} MiB, so ${targetMb} MiB would not be a scale ${up ? "up" : "down"}` };
  }
  return { currentMb };
}

// Fly accepts both personal tokens (Bearer) and macaroon tokens ("FlyV1 ...").
function authorization(token) {
  if (!token) return "";
  return /^FlyV1 /.test(token) ? token : `Bearer ${token}`;
}
