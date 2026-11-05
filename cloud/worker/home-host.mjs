// A home on an external host (cloud/host/) runs this same image under Docker
// instead of on a Fly machine. Nothing in the container can restart itself onto
// a new image, so an image update from the control plane is written to the
// volume as desired.json, and the host's systemd path unit picks it up, pulls
// the image, swaps the worker token in its env file and recreates the container.

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

/** Writes desired.json atomically, readable only by its owner. */
export function writeHostUpdate({ volume, image, workerToken }) {
  if (!validHostImage(image)) throw new Error("update: image is not a valid image reference");
  if (!validWorkerToken(workerToken)) throw new Error("update: workerToken is not a worker token");
  const file = hostUpdatePath(volume);
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify({ image, workerToken, requestedAt: new Date().toISOString() })}\n`, { mode: 0o600 });
  fs.chmodSync(tmp, 0o600);
  fs.renameSync(tmp, file);
  return file;
}

/** What this home runs on, for the control plane's idle update decision. */
export function hostInfo(env = process.env) {
  return {
    kind: env.RUDDER_HOST_KIND === "external" ? "external" : "fly",
    image: env.RUDDER_HOST_IMAGE || env.FLY_IMAGE_REF || null,
  };
}
