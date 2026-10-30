import assert from "node:assert/strict";
import test from "node:test";
import { fabricStoreTemplate, validateFabricCacheMissUpdate } from "../infra/fabric-store.mjs";

test("Fabric storage keeps writer access bounded and uses exact Fly workload trust", () => {
  const template = fabricStoreTemplate({ accountId: "123456789012" });
  const { ContentBucket, FabricRole, ContentPolicy, WriterBoundary } = template.Resources;
  assert.equal(ContentBucket.DeletionPolicy, "Retain");
  assert.equal(ContentBucket.Properties.VersioningConfiguration.Status, "Enabled");
  assert.ok(Object.values(ContentBucket.Properties.PublicAccessBlockConfiguration).every(Boolean));
  assert.equal(ContentBucket.Properties.LifecycleConfiguration, undefined);
  const trust = FabricRole.Properties.AssumeRolePolicyDocument.Statement[0];
  assert.equal(trust.Condition.StringEquals["oidc.fly.io/exla:aud"], "sts.amazonaws.com");
  assert.equal(trust.Condition.StringLike["oidc.fly.io/exla:sub"], "exla:rudder-cloud-control:*");
  assert.ok(!JSON.stringify(template).includes("AWS::IAM::AccessKey"), "no long-lived writer key is provisioned");
  const deletion = WriterBoundary.Properties.PolicyDocument.Statement.filter((rule) => [].concat(rule.Action).some((action) => action.includes("Delete")));
  assert.equal(deletion.length, 1);
  assert.match(deletion[0].Resource["Fn::Sub"], /\/staging\/\*$/);
  assert.ok(ContentPolicy.Properties.PolicyDocument.Statement.some((rule) => rule.Sid === "RequireImmutableConditionalPut" && rule.Condition.Null["s3:if-none-match"] === "true"));
  assert.deepEqual(FabricRole.Properties.PermissionsBoundary, { Ref: "WriterBoundary" });
  for (const policy of [WriterBoundary.Properties.PolicyDocument, FabricRole.Properties.Policies[0].PolicyDocument]) {
    const listing = policy.Statement.filter((rule) => [].concat(rule.Action).includes("s3:ListBucket"));
    assert.equal(listing.length, 1, "missing keys must be distinguishable from access denial under the workload role");
    assert.deepEqual(listing[0].Resource, { "Fn::GetAtt": ["ContentBucket", "Arn"] });
    assert.equal(listing[0].Condition, undefined, "GetObject does not carry a ListObjects prefix condition");
  }
});

test("the cache-miss IAM migration cannot change trust, retention or other permission scope", () => {
  const next = JSON.parse(JSON.stringify(fabricStoreTemplate({ accountId: "123456789012" })));
  const previous = JSON.parse(JSON.stringify(next));
  for (const policy of [previous.Resources.WriterBoundary.Properties.PolicyDocument, previous.Resources.FabricRole.Properties.Policies[0].PolicyDocument]) policy.Statement = policy.Statement.filter((rule) => rule.Sid !== "DistinguishCacheMisses");
  assert.doesNotThrow(() => validateFabricCacheMissUpdate(previous, next));
  for (const change of [
    (template) => { template.Resources.ContentBucket.Properties.VersioningConfiguration.Status = "Suspended"; },
    (template) => { template.Resources.FabricRole.Properties.AssumeRolePolicyDocument.Statement[0].Condition.StringLike["oidc.fly.io/exla:sub"] = "*"; },
    (template) => { template.Resources.WriterBoundary.Properties.PolicyDocument.Statement.find((rule) => rule.Sid === "DistinguishCacheMisses").Resource = "*"; },
  ]) {
    const altered = JSON.parse(JSON.stringify(next)); change(altered);
    assert.throws(() => validateFabricCacheMissUpdate(previous, altered));
  }
});

test("existing shared OIDC providers are referenced without transferring ownership", () => {
  const arn = "arn:aws:iam::123456789012:oidc-provider/oidc.fly.io/exla";
  const template = fabricStoreTemplate({ accountId: "123456789012", existingProviderArn: arn });
  assert.equal(template.Resources.FlyIdentityProvider, undefined);
  assert.equal(template.Resources.FabricRole.Properties.AssumeRolePolicyDocument.Statement[0].Principal.Federated, arn);
  assert.throws(() => fabricStoreTemplate({ accountId: "123456789012", existingProviderArn: arn.replace("123456789012", "222222222222") }), /different issuer/);
  assert.throws(() => fabricStoreTemplate({ accountId: "123456789012", flyOrg: "exla:*" }), /Invalid/);
});
