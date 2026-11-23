import fs from "node:fs";

// The run records the app is waiting on: a working agent's (its turn ends,
// it asks something) and one a send just reached (its turn starts). The home
// checks them every RUN_WATCH_MS and publishes the state the moment one
// changes, rather than at its next 1.5 s publish, so Working ends in the app
// as the reply lands. A working agent's record changes a few times a turn,
// so the checks are a few stats.
export const RUN_WATCH_MS = 250;
// How long a send keeps its agent's record watched for the turn to start.
const SENT_WATCH_MS = 20_000;

export function createRunWatch(publish) {
  // run.json path -> size:mtimeMs as the last published state read it.
  let watched = new Map();
  // run.json path -> until when.
  const sent = new Map();
  return {
    /** A send reached this idle agent: watch its record for the turn to start. */
    sent(file) {
      sent.set(file, Date.now() + SENT_WATCH_MS);
    },
    /** Whether the state being built should watch this agent's record. */
    wants(file, status) {
      return status === "working" || status === "waiting" || sent.get(file) > Date.now();
    },
    /** The records the state just built read, each with the signature it read. */
    watch(next) {
      watched = next;
      for (const [file, until] of sent) {
        if (until <= Date.now()) {
          sent.delete(file);
        }
      }
    },
    /** Publish once when a watched record changed since the state read it. */
    check() {
      for (const [file, seen] of watched) {
        if (signatureOf(file) !== seen) {
          watched = new Map();
          publish();
          return;
        }
      }
    },
  };
}

/** A run record's signature, as the state's own read of it is keyed. */
export function runSignature(stat) {
  return stat ? `${stat.size}:${stat.mtimeMs}` : "none";
}

function signatureOf(file) {
  try {
    return runSignature(fs.statSync(file));
  } catch (error) {
    if (error?.code === "ENOENT") {
      return "none";
    }
    throw error;
  }
}
