import { counter, exact, fail, text, uuid, type OwnerGrant } from "./types.js";

/** One process/boot only. Call fence() on restart/resume uncertainty; it is irreversible. */
export class MonotonicLeaseFence {
  #pending?: { requestId: string; firstSent: number };
  #deadline = -Infinity;
  #serial = -1n;
  #fenced = false;
  #lastObserved = -Infinity;
  constructor(readonly identity: { authorityId: string; accountId: string; projectId: string; engineInstanceId: string; machineBootId: string; ownerEpoch: string; leaseId: string }, readonly safety: { marginMs: number; engineRateError: number; databaseRateError: number; maxPositiveClockStepMs: number }) {
    exact(identity,["authorityId","accountId","projectId","engineInstanceId","machineBootId","ownerEpoch","leaseId"]);
    [identity.authorityId,identity.projectId,identity.engineInstanceId,identity.leaseId].forEach(uuid);
    text(identity.accountId);text(identity.machineBootId);counter(identity.ownerEpoch,true);
    const keys=["marginMs","engineRateError","databaseRateError","maxPositiveClockStepMs"] as const;
    exact(safety,keys);
    if (keys.some(key => typeof safety[key] !== "number" || !Number.isFinite(safety[key]) || safety[key] < 0) || safety.engineRateError >= 1 || safety.databaseRateError >= 1) fail("invalid_request");
    this.identity = Object.freeze({ ...identity }); this.safety = Object.freeze({ ...safety });
  }
  #time(now: number): boolean {
    if (!Number.isFinite(now) || now < 0 || now < this.#lastObserved) { this.fence(); return false; }
    this.#lastObserved = now; return true;
  }
  begin(requestId: string, now: number): void {
    if (!this.#time(now)) fail("stale_fence");
    if (this.#serial >= 0n && now >= this.#deadline) this.fence();
    text(requestId); if (this.#fenced) fail("stale_fence");
    if (this.#pending && this.#pending.requestId !== requestId) fail("busy");
    this.#pending ??= { requestId, firstSent: now };
  }
  observe(grant: OwnerGrant, now: number): boolean {
    if (!this.#time(now)) return false;
    if (this.#serial >= 0n && now >= this.#deadline) this.fence();
    if (this.#fenced || !this.#pending || grant.requestId !== this.#pending.requestId || !Number.isFinite(now) || now < this.#pending.firstSent) return false;
    if (Object.entries(this.identity).some(([key, value]) => grant[key as keyof OwnerGrant] !== value)) return false;
    const serial = BigInt(counter(grant.renewalSerial));
    if (grant.formatVersion !== 1 || serial <= this.#serial || !Number.isSafeInteger(grant.durationMs) || grant.durationMs <= 0 || grant.durationMs > 15000) return false;
    const duration = (grant.durationMs - this.safety.maxPositiveClockStepMs) * (1 - this.safety.engineRateError)
      / (1 + this.safety.databaseRateError) - this.safety.marginMs;
    const deadline = this.#pending.firstSent + duration;
    this.#pending = undefined;
    if (!Number.isFinite(duration) || duration <= 0 || deadline <= now) { this.fence(); return false; }
    this.#deadline = deadline; this.#serial = serial; return true;
  }
  current(now: number): boolean {
    if (!this.#time(now)) return false;
    if (this.#serial < 0n && !this.#fenced) return false;
    if (!Number.isFinite(now) || now >= this.#deadline) this.fence();
    return !this.#fenced;
  }
  fence(): void { this.#fenced = true; this.#pending = undefined; this.#deadline = -Infinity; }
}
