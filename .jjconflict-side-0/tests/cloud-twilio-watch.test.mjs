// Watching the phone number from Twilio's side (cloud/src/twilio-watch.ts):
// parsing Twilio's call list and alerts, joining each finished call with
// Rudder's record of it, and what reaches the log on each poll.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built = fs.existsSync(path.join(cloudDir, "dist", "twilio-watch.js"));
const test = built ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const watch = built ? await import(path.join(cloudDir, "dist", "twilio-watch.js")) : null;

const T0 = Date.parse("2026-10-19T23:25:30Z");

function twilioCall(overrides = {}) {
  return {
    sid: "CAde42a49ec0e163a948813ce85214fda7",
    status: "completed",
    from: "+13042164370",
    to: "sip:proj_x@sip.api.openai.com;transport=tls",
    direction: "trunking-originating",
    start_time: "Mon, 19 Oct 2026 23:25:30 +0000",
    date_created: "Mon, 19 Oct 2026 23:25:29 +0000",
    duration: "59",
    trunk_sid: "TK1",
    ...overrides,
  };
}

test("Twilio's call list and alerts parse, junk dropped", () => {
  const [call] = watch.parseTwilioCalls({ calls: [twilioCall(), { nope: true }] });
  assert.equal(call.sid, "CAde42a49ec0e163a948813ce85214fda7");
  assert.equal(call.startedAt, T0);
  assert.equal(call.durationSeconds, 59);
  assert.equal(watch.parseTwilioCalls({ calls: [twilioCall({ duration: null, start_time: null })] })[0].durationSeconds, null);
  assert.deepEqual(watch.parseTwilioCalls(null), []);

  const [alert] = watch.parseTwilioAlerts({ alerts: [{ sid: "NO1", error_code: "32011", log_level: "error", date_created: "2026-10-19T23:25:31Z", resource_sid: "CA1", alert_text: "Msg=Error+from+SIP+endpoint&sipResponseCode=503" }] });
  assert.equal(alert.text, "Error from SIP endpoint");
  assert.equal(alert.errorCode, "32011");
});

test("numbers are masked to their last four digits", () => {
  assert.equal(watch.maskNumber("+13042164370"), "...4370");
  assert.equal(watch.maskNumber("sip:proj@sip.api.openai.com"), "sip");
});

test("a finished call joins the nearest Rudder call and gets a verdict", () => {
  const [call] = watch.parseTwilioCalls({ calls: [twilioCall()] });
  const answered = { tag: "GPlXIm0G", at: T0 + 800, outcome: "ended" };
  const far = { tag: "older", at: T0 - 120_000, outcome: "ended" };
  assert.equal(watch.matchRudderCall(call, [far, answered]), answered);
  assert.equal(watch.matchRudderCall(call, [far]), null);

  assert.equal(watch.callVerdict(call, answered), "ok");
  assert.match(watch.callVerdict(call, null), /never reached Rudder/);
  assert.match(watch.callVerdict({ ...call, status: "failed" }, null), /failed before reaching Rudder/);
  assert.match(watch.callVerdict(call, { ...answered, outcome: "refused", detail: "unknown caller ...0100" }), /refused by Rudder \(unknown caller/);
  assert.match(watch.callVerdict({ ...call, durationSeconds: 2 }, answered), /under 5s/);
  assert.equal(watch.callVerdict({ ...call, status: "in-progress" }, null), "in progress");
  // A call the previous server process answered, cut off by a deploy.
  assert.match(watch.callVerdict(call, null, T0 + 60_000), /began before the server restarted/);
  assert.match(watch.callVerdict(call, null, T0 - 60_000), /never reached Rudder/);
});

function fakeTwilio() {
  const state = { calls: [], alerts: [], fail: false, seen: [] };
  const fetch = async (url, init) => {
    state.seen.push({ url, auth: init.headers.authorization });
    if (state.fail) {
      return new Response("down", { status: 503 });
    }
    const body = url.includes("/Alerts") ? { alerts: state.alerts } : { calls: state.calls };
    return new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
  };
  return { state, fetch };
}

test("polling: old calls are baseline, new ones are logged once, joined", async () => {
  const twilio = fakeTwilio();
  const records = [];
  const logs = [];
  const warns = [];
  const w = new watch.TwilioWatch({
    accountSid: "AC1",
    username: "AC1",
    password: "secret",
    rudderCalls: () => records,
    log: (line) => logs.push(line),
    warn: (line) => warns.push(line),
    fetch: twilio.fetch,
    apiBase: "http://twilio.test",
    monitorBase: "http://monitor.test",
  });
  twilio.state.calls = [twilioCall({ sid: "CAold0000000000000000000000000old1" })];
  twilio.state.alerts = [{ sid: "NOold", error_code: "11200", log_level: "error", alert_text: "old" }];
  await w.poll();
  assert.equal(twilio.state.seen[0].auth, `Basic ${Buffer.from("AC1:secret").toString("base64")}`);
  assert.equal(twilio.state.seen[0].url, "http://twilio.test/2010-04-01/Accounts/AC1/Calls.json?PageSize=50");
  assert.deepEqual(warns, []);
  assert.match(logs.at(-1), /watching \(1 recent calls, 1 recent alerts\)/);

  // A call rings, Rudder answers, it completes.
  const start = new Date().toUTCString();
  twilio.state.calls.unshift(twilioCall({ sid: "CAnew000000000000000000000000new1", status: "in-progress", start_time: start }));
  await w.poll();
  assert.match(logs.at(-1), /twilio call \w*new1: in-progress from \.\.\.4370/);
  records.push({ tag: "GPlXIm0G", at: Date.parse(start) + 500, outcome: "ended" });
  twilio.state.calls[0] = twilioCall({ sid: "CAnew000000000000000000000000new1", status: "completed", start_time: start, duration: "42" });
  await w.poll();
  assert.match(logs.at(-1), /twilio call \w*new1: completed from \.\.\.4370, 42s, rudder GPlXIm0G: ok/);
  const before = logs.length;
  await w.poll();
  assert.equal(logs.length, before, "a finished call is reported once");

  // A call OpenAI never handed us, and a new alert, are warnings.
  twilio.state.calls.unshift(twilioCall({ sid: "CAlost00000000000000000000000lost", start_time: new Date().toUTCString() }));
  twilio.state.alerts.unshift({ sid: "NOnew", error_code: "32011", log_level: "error", resource_sid: "CAlost", alert_text: "Msg=Error+from+SIP+endpoint" });
  await w.poll();
  assert.equal(warns.length, 2);
  assert.ok(warns.some((line) => /lost: completed .* rudder -: never reached Rudder/.test(line)));
  assert.ok(warns.some((line) => /twilio alert 32011 \(error\) on CAlost: Error from SIP endpoint/.test(line)));

  // An outage warns once, and says so when it is over.
  twilio.state.fail = true;
  await w.poll();
  await w.poll();
  assert.equal(warns.filter((line) => line.includes("poll failed")).length, 1);
  twilio.state.fail = false;
  await w.poll();
  assert.match(logs.at(-1), /polling again/);

  const snapshot = w.snapshot();
  assert.equal(snapshot.calls[0].from, "...4370", "the report masks numbers too");
  assert.equal(snapshot.calls.find((call) => call.sid.endsWith("new1")).rudder.tag, "GPlXIm0G");
  assert.equal(snapshot.lastError, null);
  assert.match(snapshot.calls.find((call) => call.sid.endsWith("old1")).verdict, /before this server started/);
});
