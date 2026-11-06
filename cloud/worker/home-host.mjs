// A home on an external host (cloud/host/) runs this same image under Docker
// instead of on a Fly machine. Nothing in the container can restart itself onto
// a new image, so an image update from the control plane is written to the
// volume as desired.json, and the host's systemd path unit picks it up, pulls
// the image, swaps the worker token in its env file and recreates the container.

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

// The host's apply script checks the same shapes before using either value.
const IMAGE_RE = /^[A-Za-z0-9][A-Za-z0-9._/:@-]{0,255}$/;
const TOKEN_RE = /^rdrw_[A-Za-z0-9_-]{16,200}$/;

export function validHostImage(value) {
  return typeof value === "string" && IMAGE_RE.test(value);
}

export function validWorkerToken(value) {
  return typeof value === "string" && TOKEN_RE.test(value);
}

export function hostUpdatePath(volume) {
  return path.join(volume, ".rudder-host", "desired.json");
}

// Where the host sets aside a request it could not apply, and its one-line
// reason (cloud/host/rudder-home-apply-update).
export function hostUpdateFailedPath(volume) {
  return path.join(volume, ".rudder-host", "desired.failed.json");
}

export function hostUpdateReasonPath(volume) {
  return path.join(volume, ".rudder-host", "desired.failed.reason");
}

/** Writes desired.json atomically, readable only by its owner. */
export function writeHostUpdate({ volume, image, workerToken }) {
  if (!validHostImage(image)) throw new Error("update: image is not a valid image reference");
  if (!validWorkerToken(workerToken)) throw new Error("update: workerToken is not a worker token");
  const file = hostUpdatePath(volume);
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  // A new request replaces whatever failed before it.
  for (const stale of [hostUpdateFailedPath(volume), hostUpdateReasonPath(volume)]) {
    try {
      fs.rmSync(stale, { force: true });
    } catch {
      // the host overwrites both if this request fails too
    }
  }
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify({ image, workerToken, requestedAt: new Date().toISOString() })}\n`, { mode: 0o600 });
  fs.chmodSync(tmp, 0o600);
  fs.renameSync(tmp, file);
  return file;
}

/** The hash the control plane keeps for a worker token (server.ts tokenHash). */
export function workerTokenHash(token) {
  return createHash("sha256").update(token).digest("hex").slice(0, 32);
}

export const HOST_UPDATE_FAILED = "the host could not apply the update; see journalctl -u rudder-home-update on the host";

/**
 * The request the host set aside, or null: its image, why it failed, and the
 * hash of its token so the control plane can match it to the replacement it
 * is waiting on. The token itself is never returned. `key` changes with each
 * new failed request.
 */
export function readHostUpdateFailure(volume) {
  let raw;
  let mtimeMs;
  try {
    const file = hostUpdateFailedPath(volume);
    mtimeMs = fs.statSync(file).mtimeMs;
    raw = fs.readFileSync(file, "utf8");
  } catch {
    return null;
  }
  let desired = {};
  try {
    desired = JSON.parse(raw) ?? {};
  } catch {
    // a request the host could not read either
  }
  let reason = "";
  try {
    reason = fs.readFileSync(hostUpdateReasonPath(volume), "utf8").split("\n")[0];
  } catch {
    // a host from before 4.2.9 records no reason
  }
  reason = reason.replace(/[\u0000-\u001f\u007f]+/g, " ").trim().slice(0, 300);
  const tokenHash = validWorkerToken(desired.workerToken) ? workerTokenHash(desired.workerToken) : null;
  return {
    key: `${tokenHash ?? ""}:${mtimeMs}`,
    image: validHostImage(desired.image) ? desired.image : null,
    tokenHash,
    reason: reason || HOST_UPDATE_FAILED,
  };
}

/** What this home runs on, for the control plane's idle update decision. */
export function hostInfo(env = process.env) {
  return {
    kind: env.RUDDER_HOST_KIND === "external" ? "external" : "fly",
    image: env.RUDDER_HOST_IMAGE || env.FLY_IMAGE_REF || null,
  };
}

// "Update now" from a dashboard on this home. home.mjs keeps status.json (what
// Cloud would move this machine to, and how a request went) for the dashboards
// to read, and takes the `request` file a dashboard writes when the user asks.
// The update itself is the same one as above: Cloud picks the image.

export function homeUpdateStatusPath(dir) {
  return path.join(dir, "status.json");
}

export function homeUpdateRequestPath(dir) {
  return path.join(dir, "request");
}

/** The status after Cloud says what this home runs and what it would run. */
export function offeredHomeUpdate(previous, { current, target }) {
  const cur = typeof current === "string" && current ? current : null;
  const tgt = typeof target === "string" && target ? target : null;
  const newer = Boolean(cur && tgt && cur !== tgt);
  // A request on its way, or one that failed, stays so for the same target.
  const kept = previous?.target === tgt && (previous.state === "requested" || previous.state === "failed");
  const state = !newer ? "current" : kept ? previous.state : "available";
  return { current: cur, target: tgt, state, error: state === "failed" ? previous.error ?? null : null };
}

export function writeHomeUpdateStatus(dir, status) {
  fs.mkdirSync(dir, { recursive: true });
  const file = homeUpdateStatusPath(dir);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify({ ...status, at: new Date().toISOString() })}\n`);
  fs.renameSync(tmp, file);
}

/** True once per request file a dashboard wrote; the file is gone afterwards. */
export function takeHomeUpdateRequest(dir) {
  try {
    fs.unlinkSync(homeUpdateRequestPath(dir));
    return true;
  } catch {
    return false;
  }
}
