import { S3FabricObjectStorage } from "../fabric-cas.js";
import { validateConfig, type PostgresAuthorityConfig } from "../project-authority/config.js";
import { validatePolicy, type ManagedHomePolicy } from "./types.js";
import type { ManagedHomeConfig } from "./index.js";
import type { StorageServiceCredential } from "./storage.js";

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)
    || Object.getPrototypeOf(value) !== Object.prototype) throw new Error();
  return value as Record<string, unknown>;
}
function fields(value: Record<string, unknown>, names: readonly string[]): void {
  if (Object.keys(value).length !== names.length || names.some(name => !Object.hasOwn(value, name))) throw new Error();
}
function text(value: unknown, maximum = 256): string {
  if (typeof value !== "string" || !value || value.length > maximum || /[\u0000-\u001f\u007f]/.test(value)) throw new Error();
  return value;
}

/** Trusted process-start configuration only. No HTTP caller, inherited worker
 * environment or readiness observation can select these credentials/policies.
 * Construction never migrates, activates a frozen authority or provisions a VM.
 */
export function readManagedHomeStartupConfig(raw: string | undefined, dependencies: {
  storage: S3FabricObjectStorage | null;
  flyToken: string;
}): ManagedHomeConfig {
  if (raw === undefined) return { mode: "inactive" };
  try {
    if (!raw || Buffer.byteLength(raw, "utf8") > 256 * 1024) throw new Error();
    const value = object(JSON.parse(raw));
    fields(value, ["formatVersion", "authority", "registrationAuthority", "policy", "tokenEncryptionKeyHex", "storageReceiptSha256", "serviceCredentials"]);
    if (value.formatVersion !== 1 || !(dependencies.storage instanceof S3FabricObjectStorage)) throw new Error();
    const flyToken = text(dependencies.flyToken, 65536);
    const authority = object(value.authority) as unknown as PostgresAuthorityConfig;
    const registrationAuthority = object(value.registrationAuthority) as unknown as PostgresAuthorityConfig;
    validateConfig(authority);
    validateConfig(registrationAuthority);
    const a = authority.connection, r = registrationAuthority.connection;
    if (authority.authorityId !== registrationAuthority.authorityId || a.user === r.user
      || a.host !== r.host || a.port !== r.port || a.database !== r.database || a.ca !== r.ca) throw new Error();
    const policy = validatePolicy(object(value.policy) as unknown as ManagedHomePolicy);
    const encryption = text(value.tokenEncryptionKeyHex, 64);
    const storageReceiptSha256 = text(value.storageReceiptSha256, 64);
    if (!/^[a-f0-9]{64}$/.test(encryption) || !/^[a-f0-9]{64}$/.test(storageReceiptSha256)) throw new Error();
    const tokenEncryptionKey = Buffer.from(encryption, "hex");
    if (!tokenEncryptionKey.some(byte => byte !== 0)) throw new Error();
    if (!Array.isArray(value.serviceCredentials) || !value.serviceCredentials.length || value.serviceCredentials.length > 64) throw new Error();
    const seen = new Set<string>();
    const serviceCredentials: StorageServiceCredential[] = value.serviceCredentials.map(input => {
      const credential = object(input);
      fields(credential, ["accountId", "principalId", "credentialId", "tokenBase64url"]);
      const accountId = text(credential.accountId), principalId = text(credential.principalId), credentialId = text(credential.credentialId);
      const encoded = text(credential.tokenBase64url, 128), token = Buffer.from(encoded, "base64url");
      if (token.length !== 32 || token.toString("base64url") !== encoded || !token.some(byte => byte !== 0)) throw new Error();
      const key = accountId + "\0" + credentialId;
      if (seen.has(key)) throw new Error();
      seen.add(key);
      return { accountId, principalId, credentialId, token };
    });
    return { mode: "postgres", authority, registrationAuthority, policy, flyToken, tokenEncryptionKey,
      freshStorage: { authority, storage: dependencies.storage, storageReceiptSha256, serviceCredentials } };
  } catch {
    // Parse/role/policy errors can carry raw passwords, tokens or private JSON.
    // Explicit invalid configuration must fail startup, never become inactive.
    throw new Error("Invalid private managed-home startup configuration");
  }
}
