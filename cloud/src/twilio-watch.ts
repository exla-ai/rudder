// Watching the phone number from Twilio's side (cloud/src/voice-call.ts is
// Rudder's side). Rudder only hears about a call once OpenAI posts its
// incoming-call webhook, so a call Twilio failed to route, or one OpenAI
// never handed us, left no trace anywhere we look. This polls Twilio's call
// list and debugger alerts and puts both in the same log stream as the
// `voice call` lines, joined: every finished Twilio call says which Rudder
// call it was, or that it never reached Rudder.
//
// Off unless TWILIO_ACCOUNT_SID and TWILIO_AUTH_TOKEN (or TWILIO_API_KEY +
// TWILIO_API_SECRET) are set.

export type TwilioCall = {
  sid: string;
  status: string;
  from: string;
  to: string;
  direction: string;
  startedAt: number | null;
  durationSeconds: number | null;
  trunkSid: string | null;
};

export type TwilioAlert = {
  sid: string;
  errorCode: string;
  level: string;
  createdAt: number | null;
  resourceSid: string;
  text: string;
};

/** Rudder's own record of a call it heard about, kept by server.ts. */
export type RudderCallRecord = {
  tag: string;
  at: number;
  outcome: "answering" | "refused" | "failed" | "ended";
  detail?: string;
};

export type JoinedCall = TwilioCall & { rudder: RudderCallRecord | null; verdict: string };

// A finished call the first poll found: Rudder's record of it (if any) died
// with the previous server process.
const BEFORE_START = "";

const FINISHED = new Set(["completed", "busy", "failed", "no-answer", "canceled"]);

/** How far apart Twilio's start time and Rudder's webhook may be and still be one call. */
const JOIN_WINDOW_MS = 45_000;

function time(value: unknown): number | null {
  if (typeof value !== "string" || !value) {
    return null;
  }
  const at = Date.parse(value);
  return Number.isFinite(at) ? at : null;
}

function str(value: unknown): string {
  return typeof value === "string" ? value : value == null ? "" : String(value);
}

export function parseTwilioCalls(json: unknown): TwilioCall[] {
  const calls = (json as { calls?: unknown })?.calls;
  if (!Array.isArray(calls)) {
    return [];
  }
  return calls
    .filter((call) => typeof call?.sid === "string")
    .map((call) => {
      const duration = Number(call.duration);
      return {
        sid: call.sid,
        status: str(call.status),
        from: str(call.from),
        to: str(call.to),
        direction: str(call.direction),
        startedAt: time(call.start_time) ?? time(call.date_created),
        durationSeconds: call.duration != null && Number.isFinite(duration) ? duration : null,
        trunkSid: typeof call.trunk_sid === "string" ? call.trunk_sid : null,
      };
    });
}

export function parseTwilioAlerts(json: unknown): TwilioAlert[] {
  const alerts = (json as { alerts?: unknown })?.alerts;
  if (!Array.isArray(alerts)) {
    return [];
  }
  return alerts
    .filter((alert) => typeof alert?.sid === "string")
    .map((alert) => ({
      sid: alert.sid,
      errorCode: str(alert.error_code),
      level: str(alert.log_level),
      createdAt: time(alert.date_created),
      resourceSid: str(alert.resource_sid),
      text: alertText(str(alert.alert_text)),
    }));
}

/** "+13042164370" -> "...4370". A phone number is personal; logs keep the tail. */
export function maskNumber(value: string): string {
  if (value.startsWith("sip:")) {
    return "sip";
  }
  const digits = value.replace(/[^\d]/g, "");
  return digits.length >= 4 ? `...${digits.slice(-4)}` : value || "?";
}

/**
 * The Rudder call nearest in time to a Twilio call, inside the join window.
 * `taken` holds tags already joined to another Twilio call: two calls a few
 * seconds apart must not both claim the one Rudder heard about.
 */
export function matchRudderCall(call: TwilioCall, records: RudderCallRecord[], taken: ReadonlySet<string> = new Set()): RudderCallRecord | null {
  if (call.startedAt == null) {
    return null;
  }
  let best: RudderCallRecord | null = null;
  for (const record of records) {
    if (taken.has(record.tag)) {
      continue;
    }
    const gap = Math.abs(record.at - call.startedAt);
    if (gap <= JOIN_WINDOW_MS && (!best || gap < Math.abs(best.at - call.startedAt))) {
      best = record;
    }
  }
  return best;
}

/**
 * What a finished call says about the line, in words. "ok" is the only
 * verdict that is not worth a look.
 */
export function callVerdict(call: TwilioCall, rudder: RudderCallRecord | null): string {
  if (!FINISHED.has(call.status)) {
    return "in progress";
  }
  if (!rudder) {
    return call.status === "completed"
      ? "never reached Rudder (no webhook from OpenAI)"
      : `${call.status} before reaching Rudder`;
  }
  if (rudder.outcome === "refused") {
    return `refused by Rudder (${rudder.detail ?? "no reason"})`;
  }
  if (rudder.outcome === "failed") {
    return `failed in Rudder (${rudder.detail ?? "no reason"})`;
  }
  if (call.status !== "completed") {
    return `${call.status} after Rudder answered`;
  }
  if (call.durationSeconds != null && call.durationSeconds < 5) {
    return "ok, but under 5s (did the caller hear anything?)";
  }
  return "ok";
}

export type TwilioWatchOptions = {
  accountSid: string;
  /** Basic-auth user: an API key (SK...) or the account sid. */
  username: string;
  password: string;
  rudderCalls: () => RudderCallRecord[];
  log: (line: string) => void;
  warn: (line: string) => void;
  fetch?: typeof fetch;
  apiBase?: string;
  monitorBase?: string;
};

export class TwilioWatch {
  private readonly options: TwilioWatchOptions;
  private readonly calls = new Map<string, TwilioCall>();
  /** Finished calls already logged, and the Rudder tag each was joined to. */
  private readonly reported = new Map<string, string | null>();
  private readonly alerts = new Map<string, TwilioAlert>();
  private baseline = false;
  private failing = false;
  lastPollAt: number | null = null;
  lastError: string | null = null;

  constructor(options: TwilioWatchOptions) {
    this.options = options;
  }

  private async get(url: string): Promise<unknown> {
    const auth = Buffer.from(`${this.options.username}:${this.options.password}`).toString("base64");
    const response = await (this.options.fetch ?? fetch)(url, {
      headers: { authorization: `Basic ${auth}` },
      signal: AbortSignal.timeout(20_000),
    });
    if (!response.ok) {
      throw new Error(`${response.status} ${(await response.text()).slice(0, 200)}`);
    }
    return response.json();
  }

  async poll(): Promise<void> {
    const api = this.options.apiBase ?? "https://api.twilio.com";
    const monitor = this.options.monitorBase ?? "https://monitor.twilio.com";
    let calls: TwilioCall[];
    let alerts: TwilioAlert[];
    try {
      [calls, alerts] = await Promise.all([
        this.get(`${api}/2010-04-01/Accounts/${encodeURIComponent(this.options.accountSid)}/Calls.json?PageSize=50`).then(parseTwilioCalls),
        this.get(`${monitor}/v1/Alerts?PageSize=50`).then(parseTwilioAlerts),
      ]);
    } catch (error) {
      this.lastError = error instanceof Error ? error.message : String(error);
      // Once per outage, not once per poll.
      if (!this.failing) {
        this.options.warn(`twilio watch: poll failed: ${this.lastError}`);
      }
      this.failing = true;
      return;
    }
    if (this.failing) {
      this.options.log("twilio watch: polling again");
    }
    this.failing = false;
    this.lastError = null;
    this.lastPollAt = Date.now();
    const records = this.options.rudderCalls();

    for (const call of calls) {
      const before = this.calls.get(call.sid);
      this.calls.set(call.sid, call);
      if (!this.baseline) {
        // What happened before the server started is not news.
        if (FINISHED.has(call.status)) {
          this.reported.set(call.sid, BEFORE_START);
        }
        continue;
      }
      if (FINISHED.has(call.status)) {
        if (this.reported.has(call.sid)) {
          continue;
        }
        const rudder = matchRudderCall(call, records, this.takenTags());
        this.reported.set(call.sid, rudder?.tag ?? null);
        const verdict = callVerdict(call, rudder);
        const line = `twilio call ${call.sid.slice(-8)}: ${call.status} from ${maskNumber(call.from)}, ${call.durationSeconds ?? "?"}s, rudder ${rudder ? rudder.tag : "-"}: ${verdict}`;
        (verdict === "ok" ? this.options.log : this.options.warn)(line);
      } else if (before?.status !== call.status) {
        this.options.log(`twilio call ${call.sid.slice(-8)}: ${call.status} from ${maskNumber(call.from)}`);
      }
    }

    for (const alert of alerts) {
      if (this.alerts.has(alert.sid)) {
        continue;
      }
      this.alerts.set(alert.sid, alert);
      if (this.baseline) {
        this.options.warn(`twilio alert ${alert.errorCode} (${alert.level}) on ${alert.resourceSid || "account"}: ${alert.text}`);
      }
    }

    if (!this.baseline) {
      this.baseline = true;
      this.options.log(`twilio watch: watching (${calls.length} recent calls, ${alerts.length} recent alerts)`);
    }
    trim(this.calls, 200);
    trim(this.alerts, 200);
    trim(this.reported, 400);
  }

  private takenTags(): Set<string> {
    return new Set([...this.reported.values()].filter((tag): tag is string => tag !== null && tag !== BEFORE_START));
  }

  /** Recent calls joined with Rudder's records, newest first, plus recent alerts. */
  snapshot(): { calls: JoinedCall[]; alerts: TwilioAlert[]; lastPollAt: string | null; lastError: string | null } {
    const records = this.options.rudderCalls();
    const calls = [...this.calls.values()]
      .sort((a, b) => (b.startedAt ?? 0) - (a.startedAt ?? 0))
      .map((call) => {
        const tag = this.reported.get(call.sid);
        if (tag === BEFORE_START) {
          return { ...call, from: maskNumber(call.from), rudder: null, verdict: "before this server started (see the Fly log)" };
        }
        const rudder = tag ? records.find((record) => record.tag === tag) ?? null
          : tag === null ? null
          : matchRudderCall(call, records, this.takenTags());
        return { ...call, from: maskNumber(call.from), rudder, verdict: callVerdict(call, rudder) };
      });
    const alerts = [...this.alerts.values()].sort((a, b) => (b.createdAt ?? 0) - (a.createdAt ?? 0));
    return {
      calls,
      alerts,
      lastPollAt: this.lastPollAt ? new Date(this.lastPollAt).toISOString() : null,
      lastError: this.lastError,
    };
  }
}

function trim<V>(map: Map<string, V>, max: number): void {
  for (const key of map.keys()) {
    if (map.size <= max) {
      return;
    }
    map.delete(key);
  }
}

// Twilio's alert_text is a query string ("Msg=...&sourceComponent=...").
function alertText(raw: string): string {
  let text = raw;
  if (/^[\w.]+=/.test(raw)) {
    const params = new URLSearchParams(raw);
    text = params.get("Msg") || params.get("msg") || [...params.entries()].map(([key, value]) => `${key}=${value}`).join(" ");
  }
  return oneLine(text).slice(0, 300);
}

function oneLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}
