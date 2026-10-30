#!/usr/bin/env node
import fs from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { parseArgs } from "node:util";
import { fabricStoreTemplate, validateFabricCacheMissUpdate } from "../infra/fabric-store.mjs";

const { values } = parseArgs({ options: { apply: { type: "boolean" }, "update-policy": { type: "boolean" }, "module-root": { type: "string" }, account: { type: "string", default: "597088032164" }, region: { type: "string", default: "us-east-1" } } });
const require = createRequire(path.resolve(values["module-root"] ?? "cloud", "package.json"));
const { fromIni } = require("@aws-sdk/credential-provider-ini");
const { STSClient, GetCallerIdentityCommand } = require("@aws-sdk/client-sts");
const { IAMClient, GetOpenIDConnectProviderCommand } = require("@aws-sdk/client-iam");
const { CloudFormationClient, DescribeStacksCommand, GetTemplateCommand, ValidateTemplateCommand, CreateStackCommand, CreateChangeSetCommand, DescribeChangeSetCommand, ExecuteChangeSetCommand } = require("@aws-sdk/client-cloudformation");
const config = { region: values.region, credentials: fromIni({ profile: "default" }) };
const identity = await new STSClient(config).send(new GetCallerIdentityCommand({}));
if (identity.Account !== values.account) throw new Error("Default AWS profile does not match the requested account");
const cfn = new CloudFormationClient(config);
const StackName = "rudder-fabric-v1";
let existingStack;
try { existingStack = (await cfn.send(new DescribeStacksCommand({ StackName }))).Stacks?.[0]; }
catch (error) { if (error.name !== "ValidationError" || !String(error.message).includes("does not exist")) throw error; }
let existingProviderArn;
if (!existingStack) {
  const iam = new IAMClient(config);
  const arn = `arn:aws:iam::${values.account}:oidc-provider/oidc.fly.io/exla`;
  try {
    const provider = await iam.send(new GetOpenIDConnectProviderCommand({ OpenIDConnectProviderArn: arn }));
    if (provider.Url !== "oidc.fly.io/exla" || !provider.ClientIDList?.includes("sts.amazonaws.com")) throw new Error("Existing OIDC provider requires an explicit audience/issuer migration");
    existingProviderArn = arn;
  } catch (error) { if (error.name !== "NoSuchEntityException") throw error; }
} else {
  const saved = await cfn.send(new GetTemplateCommand({ StackName }));
  const template = typeof saved.TemplateBody === "string" ? JSON.parse(saved.TemplateBody) : saved.TemplateBody;
  if (template?.Metadata?.RudderComponent !== "FabricV1") throw new Error("Stack name is already owned by another application");
  if (!template.Resources.FlyIdentityProvider) existingProviderArn = template.Resources.FabricRole.Properties.AssumeRolePolicyDocument.Statement[0].Principal.Federated;
}
const template = fabricStoreTemplate({ accountId: values.account, existingProviderArn });
const TemplateBody = JSON.stringify(template, null, 2);
await fs.writeFile(new URL("../infra/fabric-store.cfn.json", import.meta.url), `${TemplateBody}\n`);
await cfn.send(new ValidateTemplateCommand({ TemplateBody }));
if (!values.apply) {
  console.log(JSON.stringify({ account: identity.Account, region: values.region, stack: StackName, status: "validated", template: "infra/fabric-store.cfn.json" }));
  process.exit(0);
}
if (!existingStack) {
  const created = await cfn.send(new CreateStackCommand({ StackName, TemplateBody, Capabilities: ["CAPABILITY_NAMED_IAM"], Tags: [{ Key: "Project", Value: "Rudder" }, { Key: "Component", Value: "FabricV1" }] }));
  console.log(JSON.stringify({ stackId: created.StackId, status: "CREATE_IN_PROGRESS" }));
} else {
  const saved = await cfn.send(new GetTemplateCommand({ StackName }));
  const previous = typeof saved.TemplateBody === "string" ? JSON.parse(saved.TemplateBody) : saved.TemplateBody;
  if (JSON.stringify(previous) !== JSON.stringify(template)) {
    if (!values["update-policy"]) throw new Error("Existing stack differs; create and review an explicit change set before changing retained infrastructure");
    validateFabricCacheMissUpdate(previous, template);
    const ChangeSetName = `rudder-fabric-cache-misses-${Date.now()}`;
    const created = await cfn.send(new CreateChangeSetCommand({ StackName, ChangeSetName, ChangeSetType: "UPDATE", TemplateBody, Capabilities: ["CAPABILITY_NAMED_IAM"], Description: "Allow the dedicated cache control role to distinguish missing S3 keys from access denial" }));
    const deadline = Date.now() + 120_000;
    for (;;) {
      const proposed = await cfn.send(new DescribeChangeSetCommand({ StackName, ChangeSetName }));
      if (proposed.Status === "CREATE_COMPLETE") {
        const changes = (proposed.Changes ?? []).map(({ ResourceChange }) => ({ resource: ResourceChange.LogicalResourceId, action: ResourceChange.Action, replacement: ResourceChange.Replacement }));
        if (!changes.length || changes.some((change) => !["WriterBoundary", "FabricRole", "ContentPolicy"].includes(change.resource) || change.action !== "Modify" || change.replacement !== "False")) throw new Error("Cache-miss change set contains unexpected resource actions or replacement");
        console.log(JSON.stringify({ changeSetId: created.Id, status: "REVIEWED", changes }));
        await cfn.send(new ExecuteChangeSetCommand({ StackName, ChangeSetName }));
        break;
      }
      if (proposed.Status === "FAILED" || Date.now() > deadline) throw new Error("Cache-miss change set did not become executable");
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  }
}
const deadline = Date.now() + 10 * 60_000;
for (;;) {
  const stack = (await cfn.send(new DescribeStacksCommand({ StackName }))).Stacks?.[0];
  if (stack?.StackStatus === "CREATE_COMPLETE" || stack?.StackStatus === "UPDATE_COMPLETE") {
    console.log(JSON.stringify({ verifiedAt: new Date().toISOString(), stackId: stack.StackId, status: stack.StackStatus, outputs: Object.fromEntries((stack.Outputs ?? []).map(({ OutputKey, OutputValue }) => [OutputKey, OutputValue])) }));
    break;
  }
  if (!stack || !stack.StackStatus?.endsWith("IN_PROGRESS") || Date.now() > deadline) throw new Error(`Fabric stack did not complete: ${stack?.StackStatus ?? "absent"}`);
  await new Promise((resolve) => setTimeout(resolve, 5000));
}
