#!/usr/bin/env node
// Offline review checks only. Never loads AWS SDKs, profiles or environment data.
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { projectAuthorityTemplate } from "./template.mjs";

const read = async name => JSON.parse(await fs.readFile(new URL(name, import.meta.url), "utf8"));
const ref = name => ({ Ref: name });
function validate(template) {
  assert.equal(template.Parameters.ProvisioningState.Default, "disabled");
  assert.deepEqual(template.Conditions.CreateReviewedPreproduction, { "Fn::Equals": [ref("ProvisioningState"), "reviewed-preproduction"] });
  const resources = Object.values(template.Resources);
  assert.equal(resources.length, 19);
  for (const resource of resources) assert.equal(resource.Condition, "CreateReviewedPreproduction");
  assert.equal(resources.filter(r => r.Type === "AWS::RDS::DBInstance").length, 1);
  assert.equal(resources.filter(r => r.Type === "AWS::RDS::DBCluster").length, 0);
  const db = template.Resources.Database;
  assert.equal(db.DeletionPolicy, "Retain");
  assert.equal(db.UpdateReplacePolicy, "Retain");
  assert.equal(db.Properties.Engine, "postgres");
  assert.deepEqual(db.Properties.EngineVersion, ref("PostgreSQL17MinorVersion"));
  assert.equal(db.Properties.MultiAZ, true);
  assert.equal(db.Properties.PubliclyAccessible, false);
  assert.equal(db.Properties.StorageEncrypted, true);
  assert.equal(db.Properties.DeletionProtection, true);
  assert.equal(db.Properties.BackupRetentionPeriod, 35);
  assert.equal(db.Properties.DeleteAutomatedBackups, false);
  assert.equal(db.Properties.AutoMinorVersionUpgrade, false);
  assert.equal(db.Properties.AllowMajorVersionUpgrade, false);
  assert.equal(db.Properties.ManageMasterUserPassword, true);
  assert.equal(db.Properties.EnableIAMDatabaseAuthentication, false);
  assert.equal("MasterUserPassword" in db.Properties, false);
  assert.equal(db.Properties.StorageType, "gp3");
  assert.equal(db.Properties.AllocatedStorage, "20");
  assert.equal(db.Properties.MaxAllocatedStorage, 100);
  const parameters = template.Resources.DatabaseParameters.Properties;
  assert.equal(parameters.Family, "postgres17");
  assert.equal(parameters.Parameters["rds.force_ssl"], "1");
  assert.equal(parameters.Parameters.synchronous_commit, "on");
  assert.equal(parameters.Parameters.log_parameter_max_length_on_error, "0");
  for (const r of resources) {
    assert.equal(["AWS::EC2::Route", "AWS::EC2::InternetGateway", "AWS::EC2::NatGateway", "AWS::EC2::Instance", "AWS::SSM::Document", "AWS::IAM::OIDCProvider", "AWS::RolesAnywhere::TrustAnchor"].includes(r.Type), false);
    if (r.Type === "AWS::EC2::SecurityGroupIngress") {
      assert.equal(r.Properties.CidrIp, undefined);
      assert.equal(r.Properties.CidrIpv6, undefined);
      assert.equal(r.Properties.FromPort, 5432);
      assert.equal(r.Properties.ToPort, 5432);
      assert.ok(r.Properties.SourceSecurityGroupId);
    }
    if (r.Type === "AWS::EC2::SecurityGroup") {
      assert.equal(r.Properties.SecurityGroupIngress, undefined);
      for (const e of r.Properties.SecurityGroupEgress) {
        assert.equal(e.FromPort, 5432);
        assert.equal(e.ToPort, 5432);
        assert.notEqual(e.CidrIp, "0.0.0.0/0");
        assert.notEqual(e.CidrIpv6, "::/0");
      }
    }
  }
  const runtime = template.Resources.RuntimeSecretReadPolicy.Properties;
  assert.equal(runtime.Roles, undefined);
  assert.equal(runtime.Users, undefined);
  assert.equal(runtime.Groups, undefined);
  assert.deepEqual(runtime.PolicyDocument.Statement.map(s => s.Action), ["secretsmanager:GetSecretValue", "kms:Decrypt"]);
  assert.deepEqual(runtime.PolicyDocument.Statement[0].Resource, ref("RuntimeSecret"));
  assert.deepEqual(runtime.PolicyDocument.Statement[1].Condition.StringEquals["kms:EncryptionContext:SecretARN"], ref("RuntimeSecret"));
  const operator = template.Resources.OperatorRole.Properties;
  assert.deepEqual(operator.AssumeRolePolicyDocument.Statement[0].Principal, { AWS: ref("OperatorPrincipalArn") });
  assert.equal(resources.filter(r => r.Type === "AWS::IAM::Role").length, 1);
  for (const r of ["Vpc", "PrivateSubnetA", "PrivateSubnetB", "DatabaseSubnets", "DatabaseParameters", "DatabaseSecurityGroup", "RuntimeClientSecurityGroup", "OperatorClientSecurityGroup", "StorageKey", "SecretsKey", "RuntimeSecret"]) {
    assert.equal(template.Resources[r].DeletionPolicy, "Retain");
    assert.equal(template.Resources[r].UpdateReplacePolicy, "Retain");
  }
  for (const output of Object.values(template.Outputs)) {
    assert.equal(output.Condition, "CreateReviewedPreproduction");
    assert.equal(JSON.stringify(output).includes("GenerateSecretString"), false);
  }
}

const compiled = await read("private-rds.cfn.json");
assert.deepEqual(compiled, projectAuthorityTemplate());
validate(compiled);
const negativeMutations = [
  t => t.Parameters.ProvisioningState.Default = "reviewed-preproduction",
  t => t.Resources.Database.Condition = undefined,
  t => t.Resources.Database.Properties.PubliclyAccessible = true,
  t => t.Resources.Database.Properties.MasterUserPassword = "unsafe-placeholder",
  t => t.Resources.Database.Properties.MultiAZ = false,
  t => t.Resources.Database.Properties.BackupRetentionPeriod = 0,
  t => t.Resources.Database.DeletionPolicy = "Delete",
  t => t.Resources.DatabaseParameters.Properties.Parameters.synchronous_commit = "off",
  t => t.Resources.RuntimeDatabaseIngress.Properties.CidrIp = "0.0.0.0/0",
  t => t.Resources.RuntimeSecretReadPolicy.Properties.Roles = ["some-app-role"],
  t => t.Resources.RuntimeSecretReadPolicy.Properties.PolicyDocument.Statement[0].Resource = "*",
];
for (const mutate of negativeMutations) {
  const unsafe = structuredClone(compiled);
  mutate(unsafe);
  assert.throws(() => validate(unsafe));
}
const config = await read("rollout.example.json");
assert.deepEqual(config.authority, { mode: "inactive" });
assert.equal(config.transport, "unresolved");
assert.equal(config.productionSpendApproved, false);
assert.equal(config.workloadIdentity.reuseExistingProvider, true);
assert.equal(config.workloadIdentity.authorityRuntimeRoleArn, null);
const prices = await read("pricing-us-east-1.json");
assert.equal(prices.region, "us-east-1");
const entries = prices.catalogs.flatMap(c => c.selected);
const rate = sku => {
  const item = entries.find(p => (p.sku ?? p.SKU) === sku);
  assert.ok(item, `Missing published SKU ${sku}`);
  return Number(item.prices?.find(p => p.begin === "0")?.usd ?? item.PricePerUnit);
};
const small = 730 * rate("5SY529FVXY3EARD8") + 20 * rate("J7S7KD4WFDNQWKNX") + 2 * rate("U553K98XGDXCYHWS") + 2 * rate("BJ3PQ9BYGU6P632F");
const large = small + 730 * (rate("Z8HB5KU2V54F7ATT") - rate("5SY529FVXY3EARD8"));
const ssm = 2 * 730 * (rate("DU8E8NJG743V3F8M") + rate("4GQUNXTFWVSGPUZK")) + 16 * rate("JG3KUJMBRGHV3N8G");
const api = 2 * 730 * rate("EN2N5TATXE673A3B") + 100_000 * (rate("FC2TWT2UEPTBKVBX") + rate("GU2ZS9HVP6QTQ7KE") + .5 * .25 * rate("TG3M4CAGBA3NYQBH"));
const money = value => Math.round(value * 100) / 100;
console.log(JSON.stringify({
  scope: "offline only; no resources, credentials, production activation or HA qualification",
  resourcesDisabledByDefault: Object.keys(compiled.Resources).length,
  unsafeMutationsRejected: negativeMutations.length,
  estimatedUSD730Hours: { smallPrivateFoundation: money(small), largePrivateFoundation: money(large), twoSsmRelaysAdditional: money(ssm), proposedVpcApiAdditionalAt100kRequests: money(api), smallWithProposedVpcApi: money(small + api) },
}));
