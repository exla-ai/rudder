// Keys `rudder cloud` sent to the relay but does not yet know arrived.
//
// A WebSocket send is not a receipt: when the link dies (the laptop slept, the
// Wi-Fi changed) the socket keeps saying open until the liveness check gives
// up, and every key typed into it meanwhile used to vanish. A pong answers a
// ping that left after those keys on the same ordered stream, so it confirms
// them. Whatever is unconfirmed when the connection ends is handed to the next
// one (AttachSession.keepUnsent).
export class InputReceipts {
  private sentCount = 0;
  private confirmedCount = 0;
  private chunks: Buffer[] = [];
  private seq = 0;
  private readonly out = new Map<number, { at: number; upTo: number }>();
  private readonly now: () => number;

  constructor(now: () => number = Date.now) {
    this.now = now;
  }

  /** Record a chunk just sent. True when a receipt ping should go out now. */
  sent(chunk: Buffer): boolean {
    this.chunks.push(chunk);
    this.sentCount += 1;
    // One receipt in flight at a time; keys typed meanwhile ride the next.
    return this.out.size === 0;
  }

  /** The payload of a ping about to leave; it covers everything sent so far. */
  nextPing(): Buffer {
    this.seq += 1;
    this.out.set(this.seq, { at: this.now(), upTo: this.sentCount });
    return Buffer.from(String(this.seq));
  }

  /**
   * A pong arrived. `rtt` is its ping's round trip (null for a pong this did
   * not ask for); `again` asks for another ping because keys sent after that
   * one are still unconfirmed.
   */
  pong(payload: Buffer): { rtt: number | null; again: boolean } {
    const id = Number(payload.toString());
    const ping = this.out.get(id);
    if (!ping) {
      return { rtt: null, again: false };
    }
    // The stream is ordered: this answers every earlier ping too.
    for (const earlier of [...this.out.keys()]) {
      if (earlier <= id) this.out.delete(earlier);
    }
    if (ping.upTo > this.confirmedCount) {
      this.chunks.splice(0, ping.upTo - this.confirmedCount);
      this.confirmedCount = ping.upTo;
    }
    return { rtt: this.now() - ping.at, again: this.sentCount > this.confirmedCount && this.out.size === 0 };
  }

  /** How long the oldest unanswered ping has been out (0 when none is). */
  oldestPingAge(): number {
    let oldest = Infinity;
    for (const ping of this.out.values()) oldest = Math.min(oldest, ping.at);
    return this.out.size ? this.now() - oldest : 0;
  }

  /** Keys sent and not confirmed. */
  get unconfirmed(): number {
    return this.chunks.length;
  }

  /** Hand over the unconfirmed keys, oldest first; this connection is over. */
  takeUnconfirmed(): Buffer[] {
    const chunks = this.chunks;
    this.chunks = [];
    return chunks;
  }
}
