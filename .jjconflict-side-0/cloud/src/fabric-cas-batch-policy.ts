/** A deliberately narrow AWS S3 policy profile. This parser is DATA validation,
 * never an admission constructor. Only actual readbacks by the retained adapter
 * may issue its private qualification. Unknown policy forms fail closed. */
export interface BatchRetentionPolicyReadback {
  Status?: string;
  Rules?: readonly { Status?: string }[];
  Policy?: string;
}

export function validateBatchRetentionPolicy(bucket: string, workloadRoleArn: string, versioning: Pick<BatchRetentionPolicyReadback, "Status">,
  lifecycle: Pick<BatchRetentionPolicyReadback, "Rules">, policy: Pick<BatchRetentionPolicyReadback, "Policy">): void {
  function reject(): never { throw new Error("Immutable batch retention policy is unqualified"); }
  if (!/^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/.test(bucket) || !/^arn:aws:iam::[0-9]{12}:role\/[A-Za-z0-9+=,.@_\/-]{1,512}$/.test(workloadRoleArn) || versioning.Status !== "Enabled"
    || !Array.isArray(lifecycle.Rules) || lifecycle.Rules.length !== 0
    || typeof policy.Policy !== "string" || Buffer.byteLength(policy.Policy) > 128 * 1024) reject();
  let raw: unknown;
  try { raw = JSON.parse(policy.Policy!); } catch { reject(); }
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) reject();
  const statements = (raw as Record<string, unknown>).Statement;
  if (!Array.isArray(statements) || statements.length > 128) reject();
  const root = `arn:aws:s3:::${bucket}`;
  const resources = ["objects/*", "roots/*", "batch-holds-v1/*"].map(s => `${root}/fabric/v1/accounts/*/${s}`);
  const list = (v: unknown): string[] => typeof v === "string" ? [v]
    : Array.isArray(v) && v.every(x => typeof x === "string") ? v : [];
  const shape = (value: unknown, keys: readonly string[]): Record<string, unknown> | undefined => {
    if (!value || typeof value !== "object" || Array.isArray(value)
      || Object.keys(value).sort().join() !== [...keys].sort().join()) return undefined;
    return value as Record<string, unknown>;
  };
  const exactRoleCondition = (condition: unknown): boolean => {
    const c = shape(condition, ["ArnEquals"]), arn = c && shape(c.ArnEquals, ["aws:PrincipalArn"]);
    return !!arn && arn["aws:PrincipalArn"] === workloadRoleArn;
  };
  const conditionalPut = (condition: unknown): boolean => {
    const c = shape(condition, ["Null"]) ?? shape(condition, ["Null", "Bool"]);
    if (!c) return false;
    const n = shape(c.Null, ["s3:if-none-match"]);
    if (!n || n["s3:if-none-match"] !== "true") return false;
    if (c.Bool !== undefined) { const b = shape(c.Bool, ["s3:ObjectCreationOperation"]); if (!b || b["s3:ObjectCreationOperation"] !== "true") return false; }
    return true;
  };
  const covers = (action: string, resource: string, mode: "put" | "workload-delete" | "workload-config"): boolean => statements.some((v: unknown) => {
    if (!v || typeof v !== "object" || Array.isArray(v)) return false;
    const s = v as Record<string, unknown>;
    // No NotAction/NotResource/NotPrincipal or unknown condition exceptions.
    if (Object.keys(s).some(k => !["Sid", "Effect", "Principal", "Action", "Resource", "Condition"].includes(k))
      || s.Effect !== "Deny" || !list(s.Action).includes(action) || !list(s.Resource).includes(resource)) return false;
    if (mode === "put") return s.Principal === "*" && conditionalPut(s.Condition);
    const principal = shape(s.Principal, ["AWS"]);
    if (principal?.AWS === workloadRoleArn && s.Condition === undefined) return true;
    if (s.Principal === "*" && exactRoleCondition(s.Condition)) return true;
    // A broader preexisting data deny is safe but never required. Operator
    // configuration is deliberately NOT required to be globally locked.
    return mode === "workload-delete" && s.Principal === "*" && s.Condition === undefined;
  });
  for (const resource of resources) {
    for (const action of ["s3:DeleteObject", "s3:DeleteObjectVersion"]) if (!covers(action, resource, "workload-delete")) reject();
    if (!covers("s3:PutObject", resource, "put")) reject();
  }
  // Workload-only scope: independent administrators retain recovery access.
  // No external collector/admin may delete or overwrite retained live data.
  // A workload role cannot reconfigure the invariants it was qualified under.
  // Principal is the actual factory-bound workload ROLE, never all admins.
  // Separate operator recovery remains possible; it must close service admission
  // and preserve all original holds/data/receipts before policy changes.
  for (const action of ["s3:PutLifecycleConfiguration", "s3:PutBucketVersioning", "s3:PutBucketPolicy", "s3:DeleteBucketPolicy", "s3:DeleteBucket"])
    if (!covers(action, root, "workload-config")) reject();
}
