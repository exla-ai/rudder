// Where a home runs. "fly" is the original shape: the control plane owns a Fly
// machine and volume and starts, stops, updates and moves them. "external" is
// any Linux host with Docker (a bare-metal box, a cloud VM) running the same
// worker image under cloud/host/: the control plane never calls Fly for it,
// sends image updates over the home's control socket, and the host applies
// them itself.

export type HomeHostKind = "fly" | "external";

export function homeHostKind(value: unknown): HomeHostKind {
  return value === "external" ? "external" : "fly";
}

export function parseHomeHostKind(value: unknown): HomeHostKind | null {
  return value === "external" || value === "fly" ? value : null;
}

export type HomeHostEnvInput = {
  homeId: string;
  accountId: string;
  workerToken: string;
  cloudUrl: string;
};

/** The env file an external host passes to the worker container. */
export function homeHostEnv(input: HomeHostEnvInput): Record<string, string> {
  return {
    RUDDER_WORKSPACE_ID: input.homeId,
    RUDDER_WORKER_TOKEN: input.workerToken,
    RUDDER_ACCOUNT_ID: input.accountId,
    RUDDER_CLOUD_URL: input.cloudUrl,
    RUDDER_HOME: "1",
    RUDDER_REPO_NAME: "home",
    RUDDER_HOST_KIND: "external",
  };
}

export type ExternalHomeAction = "region" | "stop" | "resize";

const REFUSALS: Record<ExternalHomeAction, string> = {
  region: "this home runs on an external host, so it has no Fly region to move; run `rudder cloud host <target>` to move it to another host",
  stop: "this home runs on an external host; stop it on that host (`sudo systemctl stop rudder-home`)",
  resize: "this home runs on an external host; its size is the host's size",
};

export function externalHomeRefusal(action: ExternalHomeAction): string {
  return REFUSALS[action];
}

export const HOME_HOST_OFFLINE = "the home's external host is offline (its control socket is not connected); check `sudo systemctl status rudder-home` on the host";

export type ExternalUpdateInput = {
  /** Image the home last reported running (its state's host.image). */
  reportedImage: string | null | undefined;
  targetImage: string;
  /** A replacement token is already waiting for the home to use it. */
  pendingNextToken: boolean;
};

/** Whether an idle external home should be sent an image update now. */
export function externalHomeNeedsUpdate(input: ExternalUpdateInput): boolean {
  if (input.pendingNextToken || !input.targetImage) {
    return false;
  }
  // A home that never said what it runs (an image older than 4.2) is left
  // alone rather than restarted on a guess.
  if (!input.reportedImage) {
    return false;
  }
  return input.reportedImage !== input.targetImage;
}
