// Calling Rudder: a phone number that reaches the same voice assistant as the
// app (cloud/src/voice.ts). A phone call works in any car through CarPlay
// today, while the CarPlay voice template waits on Apple's entitlement.
//
// The path: the user dials a Twilio number, Twilio's Elastic SIP trunk sends
// the call to OpenAI's SIP endpoint, and OpenAI posts a signed
// `realtime.call.incoming` webhook here. We check the signature and the
// caller, accept the call with the session config, and then drive the
// conversation over a server-side WebSocket the way the phone does over its
// own: run the tool calls, deliver live updates in quiet moments, hang up on
// end_conversation or when the line has been idle too long. The audio never
// touches this server.
import { createHmac, timingSafeEqual } from "node:crypto";
import { WebSocket } from "ws";
import { runVoiceTool, voiceUpdateMessage, voiceUpdates, type VoiceContext, type VoiceSeen } from "./voice.js";

// ---------------------------------------------------------------------------
// The webhook.
// ---------------------------------------------------------------------------

/** OpenAI's webhooks follow Standard Webhooks: HMAC-SHA256 over "<id>.<timestamp>.<body>". */
export function verifyOpenAiWebhook(
  secret: string,
  headers: { id?: string; timestamp?: string; signature?: string },
  rawBody: string,
  now = Date.now(),
  toleranceSeconds = 300,
): boolean {
  const { id, timestamp, signature } = headers;
  if (!secret || !id || !timestamp || !signature) {
    return false;
  }
  const sent = Number(timestamp);
  if (!Number.isFinite(sent) || Math.abs(now / 1000 - sent) > toleranceSeconds) {
    return false;
  }
  const key = Buffer.from(secret.replace(/^whsec_/, ""), "base64");
  if (key.length === 0) {
    return false;
  }
  const expected = createHmac("sha256", key).update(`${id}.${timestamp}.${rawBody}`).digest();
  // Several "v1,<base64>" entries, space separated, while a secret rotates.
  return signature.split(" ").some((entry) => {
    const [version, value] = entry.split(",", 2);
    if (version !== "v1" || !value) {
      return false;
    }
    const given = Buffer.from(value, "base64");
    return given.length === expected.length && timingSafeEqual(given, expected);
  });
}

export type SipHeader = { name: string; value: string };

export type IncomingCall = { callId: string; headers: SipHeader[] };

/** The `realtime.call.incoming` event, or null for any other event. */
export function incomingCall(event: unknown): IncomingCall | null {
  const object = event as { type?: unknown; data?: { call_id?: unknown; sip_headers?: unknown } } | null;
  if (object?.type !== "realtime.call.incoming" || typeof object.data?.call_id !== "string") {
    return null;
  }
  const headers = Array.isArray(object.data.sip_headers)
    ? object.data.sip_headers.filter((header): header is SipHeader =>
        typeof header?.name === "string" && typeof header?.value === "string")
    : [];
  return { callId: object.data.call_id, headers };
}

function header(headers: SipHeader[], name: string): string | undefined {
  return headers.find((entry) => entry.name.toLowerCase() === name.toLowerCase())?.value;
}

/** The caller as E.164 ("+14255550100") from the From header, or null. */
export function callerNumber(headers: SipHeader[]): string | null {
  const from = header(headers, "From");
  const user = from?.match(/(?:sips?|tel):\s*(\+?[\d().\- ]{7,20})(?=[@;>]|$)/i)?.[1];
  if (!user) {
    return null;
  }
  const digits = user.replace(/[^\d]/g, "");
  if (digits.length < 7 || digits.length > 15) {
    return null;
  }
  // A US number without the country code.
  return digits.length === 10 && !user.startsWith("+") ? `+1${digits}` : `+${digits}`;
}

/**
 * Twilio stamps a STIR/SHAKEN verdict on the call. A caller id can be faked,
 * so a call that carries a verdict must carry the best one; a call without one
 * (the carrier did not sign it) is let through on the number alone.
 */
export function callerIdTrusted(headers: SipHeader[]): boolean {
  const verdict = header(headers, "X-Twilio-VerStat");
  return verdict === undefined || verdict === "TN-Validation-Passed-A";
}

/** "+14255550100=github:123, +1..." -> number to account id. */
export function parseVoicePhones(value: string | undefined): Map<string, string> {
  const phones = new Map<string, string>();
  for (const entry of (value ?? "").split(",")) {
    const at = entry.indexOf("=");
    if (at <= 0) {
      continue;
    }
    const number = callerNumber([{ name: "From", value: `sip:${entry.slice(0, at).trim()}@x` }]);
    const account = entry.slice(at + 1).trim();
    if (number && account) {
      phones.set(number, account);
    }
  }
  return phones;
}

// ---------------------------------------------------------------------------
// The call.
// ---------------------------------------------------------------------------

export type VoiceCallOptions = {
  callId: string;
  apiKey: string;
  /** https://api.openai.com, or a test double. */
  baseUrl: string;
  /** The session config to accept with (voiceSessionConfig with sip: true). */
  session: Record<string, unknown>;
  /** Accepted when the full session is refused (an option the model does not take). */
  fallbackSession?: Record<string, unknown>;
  /** What the assistant opens with (voiceGreeting). */
  greeting: string;
  ctx: VoiceContext;
  safetyIdentifier?: string;
  log?: (message: string) => void;
  /** Live updates are checked this often. */
  updateMs?: number;
  /** Hang up after this long with nobody saying anything. */
  idleMs?: number;
  /** Hang up after this long no matter what. */
  maxMs?: number;
  /** How long the goodbye may play before hanging up. */
  goodbyeMs?: number;
};

type ToolCall = { name: string; arguments: string; callId: string };

/** Accepts the call and runs the conversation until either side hangs up. */
export async function runVoiceCall(options: VoiceCallOptions): Promise<void> {
  const log = options.log ?? (() => {});
  const api = async (action: string, body?: unknown) => fetch(`${options.baseUrl}/v1/realtime/calls/${encodeURIComponent(options.callId)}/${action}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${options.apiKey}`,
      "content-type": "application/json",
      ...(options.safetyIdentifier ? { "openai-safety-identifier": options.safetyIdentifier } : {}),
    },
    body: JSON.stringify(body ?? {}),
    signal: AbortSignal.timeout(15_000),
  });

  let accepted = await api("accept", options.session);
  if (accepted.status === 400 && options.fallbackSession) {
    log(`accept refused (${(await accepted.text()).slice(0, 300)}); retrying without extras`);
    accepted = await api("accept", options.fallbackSession);
  }
  if (!accepted.ok) {
    throw new Error(`accept failed: ${accepted.status} ${(await accepted.text()).slice(0, 300)}`);
  }

  const socket = new WebSocket(`${options.baseUrl.replace(/^http/, "ws")}/v1/realtime?call_id=${encodeURIComponent(options.callId)}`, {
    headers: { authorization: `Bearer ${options.apiKey}` },
  });
  const send = (event: Record<string, unknown>) => {
    if (socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(event));
    }
  };
  const systemMessage = (text: string) => send({
    type: "conversation.item.create",
    item: { type: "message", role: "system", content: [{ type: "input_text", text }] },
  });

  // The same gates as the phone (ios/App/VoiceAssistant.swift): one response
  // at a time, never over the user, tool batches in order.
  let responseInFlight = false;
  let createPending = false;
  let awaitingResponse = false;
  let responseWanted = false;
  let userSpeaking = false;
  let playing = false;
  let pendingTools = 0;
  const toolBatches: ToolCall[][] = [];
  let toolsRunning = false;
  let ending = false;
  let finished = false;
  let lastActivity = Date.now();
  // What is already true when the call starts is not news.
  let seen: VoiceSeen | null = null;
  try {
    seen = voiceUpdates(options.ctx.projects(), null).seen;
  } catch {
    // Watching starts on the first poll instead.
  }
  const queuedUpdates: string[] = [];
  const startedAt = Date.now();

  const requestResponse = () => {
    if (userSpeaking || awaitingResponse) {
      return;
    }
    if (responseInFlight || createPending) {
      responseWanted = true;
      return;
    }
    send({ type: "response.create" });
    createPending = true;
    lastActivity = Date.now();
  };

  const deliverUpdates = () => {
    if (!queuedUpdates.length || ending || playing || userSpeaking || responseInFlight || createPending || awaitingResponse || pendingTools > 0) {
      return;
    }
    systemMessage(queuedUpdates.splice(0).join("\n"));
    requestResponse();
  };

  const hangup = async (why: string) => {
    if (finished) {
      return;
    }
    finished = true;
    log(`hanging up: ${why}`);
    await api("hangup").catch((error) => log(`hangup failed: ${error instanceof Error ? error.message : String(error)}`));
    socket.close();
  };

  // The goodbye was generated with the end_conversation call; let it play.
  let goodbyeTimer: NodeJS.Timeout | undefined;
  const endAfterGoodbye = () => {
    ending = true;
    // output_audio_buffer.stopped hangs up sooner; this caps a goodbye whose
    // buffer events never came.
    goodbyeTimer = setTimeout(() => void hangup("goodbye"), options.goodbyeMs ?? 6_000);
  };

  const drainToolBatches = async () => {
    toolsRunning = true;
    try {
      while (toolBatches.length && !finished) {
        const calls = toolBatches.shift()!;
        let end = false;
        for (const call of calls) {
          const reply = call.name === "end_conversation"
            ? { output: "Ending.", end: true }
            : await runVoiceTool(options.ctx, call.name, call.arguments).catch((error) => ({
                output: `That did not work: ${error instanceof Error ? error.message : String(error)}`,
                end: false,
              }));
          pendingTools = Math.max(0, pendingTools - 1);
          lastActivity = Date.now();
          send({ type: "conversation.item.create", item: { type: "function_call_output", call_id: call.callId, output: reply.output } });
          end ||= Boolean(reply.end);
        }
        if (end) {
          endAfterGoodbye();
          return;
        }
        requestResponse();
      }
    } finally {
      toolsRunning = false;
    }
  };

  const responseDone = (response: Record<string, unknown> | undefined) => {
    responseInFlight = false;
    lastActivity = Date.now();
    const status = typeof response?.status === "string" ? response.status : "";
    if (status === "failed") {
      log(`response failed: ${JSON.stringify(response?.status_details ?? {}).slice(0, 300)}`);
    }
    // Only finished calls of a finished response run: a cancelled call can
    // carry half its arguments.
    const output = status === "completed" && Array.isArray(response?.output) ? response.output as Record<string, unknown>[] : [];
    const calls: ToolCall[] = output
      .filter((item) => item.type === "function_call" && item.status === "completed" && typeof item.name === "string" && typeof item.call_id === "string")
      .map((item) => ({ name: item.name as string, arguments: typeof item.arguments === "string" ? item.arguments : "{}", callId: item.call_id as string }));
    if (calls.length) {
      responseWanted = false;
      pendingTools += calls.length;
      toolBatches.push(calls);
      if (!toolsRunning) {
        void drainToolBatches();
      }
    } else if (responseWanted && !createPending && !userSpeaking && !awaitingResponse) {
      responseWanted = false;
      requestResponse();
    }
    deliverUpdates();
  };

  socket.on("open", () => {
    log("connected");
    systemMessage(options.greeting);
    requestResponse();
  });

  socket.on("message", (data) => {
    let event: { type?: string; response?: Record<string, unknown>; error?: { message?: string } };
    try {
      event = JSON.parse(data.toString());
    } catch {
      return;
    }
    switch (event.type) {
      case "input_audio_buffer.speech_started":
        userSpeaking = true;
        lastActivity = Date.now();
        break;
      case "input_audio_buffer.speech_stopped":
        userSpeaking = false;
        awaitingResponse = true;
        lastActivity = Date.now();
        break;
      case "response.created":
        responseInFlight = true;
        createPending = false;
        awaitingResponse = false;
        break;
      case "response.done":
        responseDone(event.response);
        break;
      case "output_audio_buffer.started":
        playing = true;
        lastActivity = Date.now();
        break;
      case "output_audio_buffer.stopped":
      case "output_audio_buffer.cleared":
        playing = false;
        lastActivity = Date.now();
        if (ending) {
          void hangup("goodbye played");
        } else {
          deliverUpdates();
        }
        break;
      case "error":
        log(`error: ${event.error?.message ?? "unknown"}`);
        // A refused response.create sends neither created nor done.
        createPending = false;
        awaitingResponse = false;
        break;
    }
  });

  const watch = setInterval(() => {
    if (finished) {
      return;
    }
    const now = Date.now();
    if (now - startedAt > (options.maxMs ?? 30 * 60_000)) {
      void hangup("call too long");
      return;
    }
    // A turn that drew no response must not hold the line "thinking".
    if ((awaitingResponse || createPending) && !responseInFlight && now - lastActivity > 10_000) {
      awaitingResponse = false;
      createPending = false;
    }
    const quiet = !playing && !userSpeaking && pendingTools === 0 && !responseInFlight;
    if (quiet && now - lastActivity > (options.idleMs ?? 3 * 60_000)) {
      void hangup("idle");
      return;
    }
    try {
      const next = voiceUpdates(options.ctx.projects(), seen);
      seen = next.seen;
      if (next.updates.length) {
        queuedUpdates.push(voiceUpdateMessage(next.updates));
        deliverUpdates();
      }
    } catch (error) {
      log(`updates: ${error instanceof Error ? error.message : String(error)}`);
    }
  }, options.updateMs ?? 5_000);

  await new Promise<void>((resolve) => {
    socket.on("close", () => resolve());
    socket.on("error", (error) => {
      log(`socket: ${error.message}`);
      resolve();
    });
  });
  finished = true;
  clearInterval(watch);
  clearTimeout(goodbyeTimer);
  log("ended");
}
