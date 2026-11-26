import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test from "node:test";
import { setImmediate as tick } from "node:timers/promises";

const { ShellRelay } = await import("../cloud/src/shell-relay.ts");

function socket() {
  const ws = new EventEmitter();
  ws.readyState = 1;
  ws.sent = [];
  ws.send = (text) => ws.sent.push(JSON.parse(text));
  ws.close = (code) => { ws.readyState = 3; ws.closed = code; ws.emit("close"); };
  return ws;
}

test("a ticket opens one socket, for its account, for half a minute", () => {
  const relay = new ShellRelay({});
  const ticket = relay.ticket("a", 1000);
  assert.equal(relay.redeem(ticket, 2000), "a");
  assert.equal(relay.redeem(ticket, 2000), null);
  assert.equal(relay.redeem(relay.ticket("a", 1000), 31_001), null);
  assert.equal(relay.redeem("", 0), null);
});

test("typing reaches the account's home only once its shell is open, and output comes back", async () => {
  const posted = [];
  const relay = new ShellRelay({
    home: async (account) => `home-of-${account}`,
    rpc: async (home, method, params) => { posted.push([home, method, params]); return { replay: "$ " }; },
    post: (home, message) => posted.push([home, message]),
  });
  const ws = socket();
  relay.attach("a", ws);
  ws.emit("message", JSON.stringify({ type: "input", data: "early" }));
  ws.emit("message", JSON.stringify({ type: "open", cols: 80, rows: 24 }));
  await tick();
  assert.deepEqual(posted, [["home-of-a", "shell-open", { cols: 80, rows: 24 }]]);
  assert.deepEqual(ws.sent, [{ type: "ready", replay: "$ " }]);
  ws.emit("message", JSON.stringify({ type: "input", data: "ls\r" }));
  assert.deepEqual(posted[1], ["home-of-a", { type: "shell-input", data: "ls\r" }]);
  assert.equal(relay.fromHome("home-of-a", { type: "shell-output", data: "file" }), true);
  assert.equal(relay.fromHome("home-of-b", { type: "shell-output", data: "other" }), true);
  assert.equal(relay.fromHome("home-of-a", { type: "state" }), false);
  assert.deepEqual(ws.sent[1], { type: "output", data: "file" });
  assert.equal(ws.sent.length, 2);
  relay.homeGone("home-of-a");
  assert.equal(ws.closed, 4001);
});

test("a home that cannot open a shell says why and the socket closes", async () => {
  const relay = new ShellRelay({ home: async () => "h", rpc: async () => { throw new Error("Your cloud machine runs 5.0.43, which does not have this yet."); }, post: () => {} });
  const ws = socket();
  relay.attach("a", ws);
  ws.emit("message", JSON.stringify({ type: "open", cols: 80, rows: 24 }));
  await tick();
  assert.deepEqual(ws.sent, [{ type: "error", message: "Your cloud machine runs 5.0.43, which does not have this yet." }]);
  assert.equal(ws.closed, 4500);
});
