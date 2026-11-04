import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";

const built = new URL("../cloud/dist/home-control-sessions.js", import.meta.url);
const lib = fs.existsSync(built) ? await import(built.href) : null;
const test = lib ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built" }, fn);

class Clock {
  at = 0;
  timers = [];
  now = () => this.at;
  setTimeout = (callback, ms) => {
    const timer = { callback, ms, deadline: this.at + ms, cleared: false };
    this.timers.push(timer); return timer;
  };
  clearTimeout = (timer) => { timer.cleared = true; };
  fire(timer) { timer.callback(); } // Includes an already-queued callback after clearTimeout.
}
function fixture(id) {
  const clock = new Clock(); let serial = 0;
  const sessions = new lib.HomeControlSessions({
    isOpen: (socket) => socket.open,
    send: (socket, data) => {
      socket.sent.push(JSON.parse(data));
      if (socket.failure === "throw") throw new Error("private synthetic transport detail");
      return socket.failure !== "false";
    },
  }, { timing: clock, id: id ?? (() => `rpc-${++serial}`) });
  return { clock, sessions };
}
function socket() { return { open: true, sent: [] }; }
function observe(promise) {
  const view = { settled: false };
  view.done = promise.then(
    (value) => { view.settled = true; return { value }; },
    (error) => { view.settled = true; return { error }; },
  );
  return view;
}
function unknown(outcome, reason, status = 503) {
  assert.ok(outcome.error instanceof lib.HomeRpcEffectUnknownError);
  assert.equal(outcome.error.code, "HOME_RPC_EFFECT_UNKNOWN");
  assert.equal(outcome.error.effect, "unknown");
  assert.equal(outcome.error.reason, reason); assert.equal(outcome.error.status, status);
  assert.match(outcome.error.message, /effect unknown; request was not replayed/);
}

test("ordinary success keeps the worker7 RPC wire and original caller timeout", async () => {
  const { clock, sessions } = fixture(); const wire = socket(); const session = sessions.attach("home-a", wire);
  const call = observe(sessions.request("home-a", wire, "task", { text: "hello" }, 20_000));
  assert.deepEqual(wire.sent, [{ type: "rpc", id: "rpc-1", method: "task", params: { text: "hello" } }]);
  assert.equal(clock.timers[0].ms, 20_000);
  assert.equal(sessions.reply(session, "rpc-1", { accepted: true }), true);
  assert.deepEqual(await call.done, { value: { accepted: true } });
  assert.equal(clock.timers[0].cleared, true);
  assert.equal(sessions.reply(session, "rpc-1", { duplicate: true }), false);
  const failed = observe(sessions.request("home-a", wire, "models", {}, 40_000));
  assert.equal(sessions.reply(session, "rpc-2", null, "worker error"), true);
  const outcome = await failed.done;
  assert.equal(outcome.error.message, "worker error"); assert.equal(outcome.error.status, 400);
});

test("cross-home colliding IDs cannot acknowledge another authenticated home's call", async () => {
  const { sessions } = fixture(() => "same-id"); const a = socket(); const b = socket();
  const sa = sessions.attach("home-a", a); const sb = sessions.attach("home-b", b);
  const pa = observe(sessions.request("home-a", a, "task", {}, 20_000));
  assert.equal(sessions.reply(sb, "same-id", "forged A result"), false);
  const pb = observe(sessions.request("home-b", b, "task", {}, 20_000));
  assert.equal(sessions.reply(sb, "same-id", "B result"), true);
  assert.deepEqual(await pb.done, { value: "B result" });
  assert.equal(pa.settled, false);
  assert.equal(sessions.reply(sa, "same-id", "A result"), true);
  assert.deepEqual(await pa.done, { value: "A result" });
});

test("replacement rejects only the old socket's calls and refuses its stale same-home reply", async () => {
  const { clock, sessions } = fixture(); const a = socket(); const b = socket();
  const old = sessions.attach("home-a", a); const other = sessions.attach("home-b", b);
  const p1 = observe(sessions.request("home-a", a, "task", {}, 20_000));
  const p2 = observe(sessions.request("home-a", a, "upload", {}, 60_000));
  const pb = observe(sessions.request("home-b", b, "task", {}, 20_000));
  clock.at = 7_000;
  const nextSocket = socket(); const next = sessions.attach("home-a", nextSocket);
  unknown(await p1.done, "replaced"); unknown(await p2.done, "replaced");
  assert.equal(pb.settled, false); assert.equal(clock.timers[2].cleared, false);
  assert.equal(clock.timers[2].deadline, 20_000, "another home keeps its original timeout");
  assert.equal(sessions.reply(old, "rpc-1", "late"), false);
  assert.equal(sessions.reply(next, "rpc-1", "adopted old result"), false);
  const current = observe(sessions.request("home-a", nextSocket, "task", {}, 20_000));
  sessions.close(old); // A delayed old close must not retire the replacement.
  assert.equal(sessions.isCurrent(next), true);
  assert.equal(sessions.reply(next, "rpc-4", "current"), true);
  assert.equal(sessions.reply(other, "rpc-3", "other"), true);
  assert.deepEqual(await current.done, { value: "current" });
  assert.deepEqual(await pb.done, { value: "other" });
  assert.equal(a.sent.length, 2, "neither old mutation was replayed");
});

test("reattaching the same socket object gets a new generation even when IDs collide", async () => {
  const { clock, sessions } = fixture(() => "same-id"); const wire = socket();
  const old = sessions.attach("home", wire);
  const prior = observe(sessions.request("home", wire, "task", {}, 20_000)); const oldTimer = clock.timers[0];
  const current = sessions.attach("home", wire); unknown(await prior.done, "replaced");
  assert.notEqual(old, current);
  const next = observe(sessions.request("home", wire, "task", {}, 20_000));
  clock.fire(oldTimer); sessions.close(old);
  assert.equal(sessions.reply(old, "same-id", "stale generation"), false);
  assert.equal(next.settled, false); assert.equal(clock.timers[1].cleared, false);
  assert.equal(sessions.reply(current, "same-id", "current generation"), true);
  assert.deepEqual(await next.done, { value: "current generation" });
});

test("close cleanup is socket-specific and closed-session admission sends nothing", async () => {
  const { clock, sessions } = fixture(); const a = socket(); const b = socket();
  const sa = sessions.attach("a", a); const sb = sessions.attach("b", b);
  const pa = observe(sessions.request("a", a, "task", {}, 20_000));
  const pb = observe(sessions.request("b", b, "task", {}, 20_000));
  a.open = false; sessions.close(sa); sessions.close(sa);
  unknown(await pa.done, "closed"); assert.equal(pb.settled, false);
  assert.equal(clock.timers[0].cleared, true); assert.equal(clock.timers[1].cleared, false);
  const refused = await observe(sessions.request("a", a, "task", {}, 20_000)).done;
  assert.equal(refused.error.status, 503); assert.equal(refused.error.effect, undefined);
  assert.equal(a.sent.length, 1);
  sessions.closeCurrent("b", a); assert.equal(sessions.isCurrent(sb), true);
  sessions.closeCurrent("b", b); unknown(await pb.done, "closed");
});

test("a reply at the original deadline is unknown even before the queued timeout runs", async () => {
  const { clock, sessions } = fixture(); const wire = socket(); const session = sessions.attach("home", wire);
  const call = observe(sessions.request("home", wire, "action", {}, 150_000));
  clock.at = 150_000;
  assert.equal(sessions.reply(session, "rpc-1", { tooLate: true }), false);
  unknown(await call.done, "timeout", 504); clock.fire(clock.timers[0]);
  assert.equal(wire.sent.length, 1, "late/timeout paths never resend mutations");
});

test("timeout/reply callback races settle once without deleting another request", async () => {
  for (const replyFirst of [false, true]) {
    const { clock, sessions } = fixture(); const wire = socket(); const session = sessions.attach("home", wire);
    const call = observe(sessions.request("home", wire, "task", {}, 20_000)); const timer = clock.timers[0];
    if (replyFirst) {
      clock.at = 19_999; assert.equal(sessions.reply(session, "rpc-1", "accepted"), true);
      clock.at = 20_000; clock.fire(timer); assert.deepEqual(await call.done, { value: "accepted" });
    } else {
      clock.at = 20_000; clock.fire(timer); unknown(await call.done, "timeout", 504);
      assert.equal(sessions.reply(session, "rpc-1", "late"), false);
    }
    const next = observe(sessions.request("home", wire, "task", {}, 20_000));
    clock.fire(timer); assert.equal(clock.timers[1].cleared, false);
    assert.equal(sessions.reply(session, "rpc-2", "next"), true);
    assert.deepEqual(await next.done, { value: "next" });
  }
});

test("failed transport delivery is effect-unknown and never retries or exposes its error", async () => {
  for (const failure of ["false", "throw"]) {
    const { clock, sessions } = fixture(); const wire = socket(); wire.failure = failure;
    sessions.attach("home", wire);
    const call = await observe(sessions.request("home", wire, "task", {}, 20_000)).done;
    unknown(call, "send_failed"); assert.equal(wire.sent.length, 1); assert.equal(clock.timers[0].cleared, true);
    assert.doesNotMatch(call.error.message, /private synthetic transport detail/);
  }
});

test("a duplicate active ID is refused without overwriting the admitted request", async () => {
  const { clock, sessions } = fixture(() => "same-id"); const wire = socket(); const session = sessions.attach("home", wire);
  const original = observe(sessions.request("home", wire, "task", {}, 20_000));
  const duplicate = await observe(sessions.request("home", wire, "task", {}, 20_000)).done;
  assert.equal(duplicate.error.status, 503); assert.equal(clock.timers.length, 1); assert.equal(wire.sent.length, 1);
  assert.equal(original.settled, false); assert.equal(sessions.reply(session, "same-id", "original"), true);
  assert.deepEqual(await original.done, { value: "original" });
});

test("serialization cannot dispatch after consuming the original timeout budget", async () => {
  const { clock, sessions } = fixture(); const wire = socket(); const otherWire = socket();
  sessions.attach("home", wire); const otherSession = sessions.attach("other", otherWire);
  const other = observe(sessions.request("other", otherWire, "task", {}, 60_000));
  let serializations = 0;
  const call = observe(sessions.request("home", wire, "task", {
    toJSON() { serializations++; clock.at = 20_000; return { text: "late mutation" }; },
  }, 20_000));
  unknown(await call.done, "timeout", 504);
  assert.equal(serializations, 1); assert.equal(wire.sent.length, 0);
  assert.equal(clock.timers.length, 2, "serialization must not start a replacement timeout");
  assert.equal(clock.timers[1].deadline, 20_000); assert.equal(clock.timers[1].cleared, true);
  clock.fire(clock.timers[1]);
  assert.equal(other.settled, false); assert.equal(clock.timers[0].cleared, false);
  assert.equal(clock.timers[0].deadline, 60_000);
  assert.equal(sessions.reply(otherSession, "rpc-1", "other remains owned"), true);
  assert.deepEqual(await other.done, { value: "other remains owned" });
});

test("serialization-time replacement or close cannot dispatch or settle another session's call", async () => {
  for (const reason of ["replaced", "closed"]) {
    const { clock, sessions } = fixture(() => "same-id");
    const wire = socket(); const nextWire = socket(); const otherWire = socket();
    const old = sessions.attach("home", wire); const otherSession = sessions.attach("other", otherWire);
    const other = observe(sessions.request("other", otherWire, "task", {}, 20_000));
    let nextSession; let next;
    const call = observe(sessions.request("home", wire, "task", {
      toJSON() {
        if (reason === "replaced") {
          nextSession = sessions.attach("home", nextWire);
          next = observe(sessions.request("home", nextWire, "task", {}, 20_000));
        } else sessions.close(old);
        return { text: "retired mutation" };
      },
    }, 20_000));
    unknown(await call.done, reason);
    assert.equal(wire.sent.length, 0, "the serialized old mutation must never reach transport.send");
    assert.equal(clock.timers[1].cleared, true); assert.equal(clock.timers[1].deadline, 20_000);
    clock.fire(clock.timers[1]); sessions.close(old);
    assert.equal(other.settled, false); assert.equal(clock.timers[0].cleared, false);
    assert.equal(sessions.reply(old, "same-id", "stale"), false);
    if (next) {
      assert.equal(nextWire.sent.length, 1); assert.equal(next.settled, false);
      assert.equal(clock.timers[2].cleared, false);
      assert.equal(sessions.reply(nextSession, "same-id", "replacement remains owned"), true);
      assert.deepEqual(await next.done, { value: "replacement remains owned" });
    } else assert.equal(nextWire.sent.length, 0);
    assert.equal(sessions.reply(otherSession, "same-id", "other remains owned"), true);
    assert.deepEqual(await other.done, { value: "other remains owned" });
  }
});
