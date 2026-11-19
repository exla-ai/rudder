import { TerminalRecovery } from "/app/terminal-recovery.js";

/** The agent's current terminal, read only. Each screen is a complete ANSI
 * repaint of its PTY (vt100-ansi-v1: geometry, cursor, modes and plain-text
 * scrollback). `draw(screen, done)` paints it (terminal-panel.js, xterm.js)
 * and calls `done` once it is on screen, which acknowledges the frame. */
export class CanonicalTerminalView {
  constructor(draw, { workspaceId, runId, onUnavailable = () => {}, onCapability = () => {},
    socket = (url) => new WebSocket(url), afterPaint = (fn) => requestAnimationFrame(() => requestAnimationFrame(fn)),
    origin = location.origin } = {}) {
    this.selection = {workspaceId,runId}; this.socket = socket;
    this.afterPaint = afterPaint; this.origin = origin; this.onUnavailable = onUnavailable; this.onCapability = onCapability;
    this.active = false; this.serial = 0; this.timer = null; this.ws = null;
    this.recovery = new TerminalRecovery({
      restore: (screen, paint) => {
        if (!paint.isCurrent()) return;
        // A whole screen from the authoritative emulator, never a replay suffix.
        draw(screen, () => this.afterPaint(() => { if (this.active && paint.isCurrent()) paint.painted(); }));
      },
      requestSnapshot: cursor => this.send({version:1,type:"terminal-snapshot-request",cursor}),
      acknowledge: cursor => this.send({version:1,type:"terminal-painted",...cursor}),
      sendInput: () => false,
      maxScreenBytes: 256 * 1024,
    });
  }
  send(value) { if(this.active && this.ws?.readyState === 1) this.ws.send(JSON.stringify(value)); }
  start() {
    if (this.active) return; this.active = true; this.connect();
  }
  connect() {
    if (!this.active) return;
    const serial = ++this.serial;
    const url = new URL("/api/app/terminal-screen", this.origin); url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    for (const [key,value] of Object.entries({workspace:this.selection.workspaceId,run:this.selection.runId,
      capability:"canonical-screen-text-v1",client:crypto.randomUUID()})) url.searchParams.set(key,value);
    const ws = this.socket(url.href); this.ws = ws; let connection;
    const current = () => this.active && this.serial === serial && this.ws === ws;
    ws.onmessage = event => {
      if (!current() || typeof event.data !== "string" || event.data.length > 1024 * 1024) return;
      let frame;try{frame=JSON.parse(event.data);}catch{return;}
      if (frame?.version === 1 && frame.type === "terminal-capability" && frame.capability === "canonical-screen-text-v1"
        && frame.access === "read-only" && typeof frame.generation === "string" && frame.generation.length <= 2048) {
        if (connection !== undefined) return;
        connection = this.recovery.connect(frame.generation,false); this.connection = connection; this.onCapability("read-only"); return;
      }
      if (connection === undefined) return;
      if (frame.type === "terminal-screen") this.recovery.accept(connection,frame);
      if (frame.type === "terminal-exit") this.recovery.acceptExit(connection,frame);
    };
    ws.onclose = () => {
      if (!current()) return;
      if (connection !== undefined) this.recovery.disconnect(connection);
      this.ws = null;
      // History and legacy owners retain the existing authenticated HTTP view.
      // No byte channel is ever passed off as this negotiated capability.
      if (connection === undefined) this.onUnavailable();
      else this.timer = setTimeout(() => this.connect(),1000);
    };
    ws.onerror = () => { if (current()) ws.close(); };
  }
  stop() {
    this.active = false; this.serial++; clearTimeout(this.timer);
    if (this.connection !== undefined) this.recovery.disconnect(this.connection); this.connection = undefined;
    this.ws?.close();this.ws = null;
  }
}
