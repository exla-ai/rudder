import assert from "node:assert/strict";
import test from "node:test";
import { fabricStoreTemplate, validateRemoteBuildCacheUpdate } from "../infra/fabric-store.mjs";

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
  assert.deepEqual(deletion.map((rule) => rule.Resource["Fn::Sub"]).sort(), [
    "${ContentBucket.Arn}/cache/v1/accounts/*",
    "${ContentBucket.Arn}/fabric/v1/accounts/*/staging/*",
  ]);
  const cacheGrant = {
    Sid: "RemoteBuildCacheObjects", Effect: "Allow",
    Action: ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
    Resource: { "Fn::Sub": "${ContentBucket.Arn}/cache/v1/accounts/*" },
  };
  assert.deepEqual(WriterBoundary.Properties.PolicyDocument.Statement.find((rule) => rule.Sid === cacheGrant.Sid), cacheGrant);
  assert.deepEqual(FabricRole.Properties.Policies.find((policy) => policy.PolicyName === "RudderRemoteBuildCache").PolicyDocument.Statement, [cacheGrant]);
  assert.ok(ContentPolicy.Properties.PolicyDocument.Statement.some((rule) => rule.Sid === "RequireImmutableConditionalPut" && rule.Condition.Null["s3:if-none-match"] === "true"));
  assert.deepEqual(FabricRole.Properties.PermissionsBoundary, { Ref: "WriterBoundary" });
  for (const policy of [WriterBoundary.Properties.PolicyDocument, FabricRole.Properties.Policies[0].PolicyDocument]) {
    const listing = policy.Statement.filter((rule) => [].concat(rule.Action).includes("s3:ListBucket"));
    assert.equal(listing.length, 1, "missing keys must be distinguishable from access denial under the workload role");
    assert.deepEqual(listing[0].Resource, { "Fn::GetAtt": ["ContentBucket", "Arn"] });
    assert.equal(listing[0].Condition, undefined, "GetObject does not carry a ListObjects prefix condition");
  }
});

test("the remote build cache IAM update cannot change trust, retention or other permission scope", () => {
  const next = JSON.parse(JSON.stringify(fabricStoreTemplate({ accountId: "123456789012" })));
  const previous = JSON.parse(JSON.stringify(next));
  const boundary = previous.Resources.WriterBoundary.Properties.PolicyDocument;
  boundary.Statement = boundary.Statement.filter((rule) => rule.Sid !== "RemoteBuildCacheObjects");
  const role = previous.Resources.FabricRole.Properties;
  role.Policies = role.Policies.filter((policy) => policy.PolicyName !== "RudderRemoteBuildCache");
  assert.doesNotThrow(() => validateRemoteBuildCacheUpdate(previous, next));
  for (const change of [
    (template) => { template.Resources.ContentBucket.Properties.VersioningConfiguration.Status = "Suspended"; },
    (template) => { template.Resources.FabricRole.Properties.AssumeRolePolicyDocument.Statement[0].Condition.StringLike["oidc.fly.io/exla:sub"] = "*"; },
    (template) => { template.Resources.WriterBoundary.Properties.PolicyDocument.Statement.find((rule) => rule.Sid === "RemoteBuildCacheObjects").Resource = "*"; },
  ]) {
    const altered = JSON.parse(JSON.stringify(next)); change(altered);
    assert.throws(() => validateRemoteBuildCacheUpdate(previous, altered));
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
