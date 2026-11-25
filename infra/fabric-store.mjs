import { isDeepStrictEqual } from "node:util";

const cacheObjects = {
  Sid: "RemoteBuildCacheObjects",
  Effect: "Allow",
  Action: ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
  Resource: { "Fn::Sub": "${ContentBucket.Arn}/cache/v1/accounts/*" },
};
const cachePolicy = {
  PolicyName: "RudderRemoteBuildCache",
  PolicyDocument: { Version: "2012-10-17", Statement: [cacheObjects] },
};

// The data plane uses Fly workload identity. Admin credentials are used only
// to provision this stack, never copied into the application or worker image.
export function fabricStoreTemplate({ accountId, flyOrg = "exla", controlApp = "rudder-cloud-control", existingProviderArn } = {}) {
  if (!/^\d{12}$/.test(accountId ?? "") || !/^[a-z0-9][a-z0-9-]{0,62}$/.test(flyOrg) || !/^[a-z0-9][a-z0-9-]{0,62}$/.test(controlApp)) throw new Error("Invalid fabric infrastructure identity");
  const issuer = `oidc.fly.io/${flyOrg}`;
  if (existingProviderArn && existingProviderArn !== `arn:aws:iam::${accountId}:oidc-provider/${issuer}`) throw new Error("OIDC provider belongs to a different issuer/account");
  const sub = (value) => ({ "Fn::Sub": value });
  const attr = (name, key) => ({ "Fn::GetAtt": [name, key] });
  const bucket = attr("ContentBucket", "Arn");
  const immutable = [sub("${ContentBucket.Arn}/fabric/v1/accounts/*/objects/*"), sub("${ContentBucket.Arn}/fabric/v1/accounts/*/roots/*")];
  const tags = [{ Key: "Project", Value: "Rudder" }, { Key: "Component", Value: "FabricV1" }];
  const permission = {
    Version: "2012-10-17",
    Statement: [
      { Sid: "ReadWriteImmutableContent", Effect: "Allow", Action: ["s3:GetObject", "s3:PutObject"], Resource: sub("${ContentBucket.Arn}/fabric/v1/accounts/*") },
      // S3 otherwise returns 403 for absent keys, which is indistinguishable
      // from denied access. This dedicated private bucket belongs to the
      // trusted control service; listing is never exposed through its API.
      { Sid: "DistinguishCacheMisses", Effect: "Allow", Action: "s3:ListBucket", Resource: bucket },
      { Sid: "ReleaseStagingOnly", Effect: "Allow", Action: "s3:DeleteObject", Resource: sub("${ContentBucket.Arn}/fabric/v1/accounts/*/staging/*") },
      { Sid: "InspectRetention", Effect: "Allow", Action: ["s3:GetBucketLocation", "s3:GetBucketVersioning", "s3:GetLifecycleConfiguration"], Resource: bucket },
    ],
  };
  const resources = {
    ContentBucket: {
      Type: "AWS::S3::Bucket", DeletionPolicy: "Retain", UpdateReplacePolicy: "Retain",
      Properties: {
        BucketName: sub("${AWS::StackName}-${AWS::AccountId}-${AWS::Region}"),
        PublicAccessBlockConfiguration: { BlockPublicAcls: true, BlockPublicPolicy: true, IgnorePublicAcls: true, RestrictPublicBuckets: true },
        OwnershipControls: { Rules: [{ ObjectOwnership: "BucketOwnerEnforced" }] },
        BucketEncryption: { ServerSideEncryptionConfiguration: [{ ServerSideEncryptionByDefault: { SSEAlgorithm: "AES256" } }] },
        VersioningConfiguration: { Status: "Enabled" },
        Tags: tags,
      },
    },
    WriterBoundary: {
      Type: "AWS::IAM::ManagedPolicy",
      Properties: { Description: "Maximum permissions for the Rudder Fabric object-store writer", PolicyDocument: { ...permission, Statement: [...permission.Statement, cacheObjects] } },
    },
    FabricRole: {
      Type: "AWS::IAM::Role",
      Properties: {
        RoleName: "rudder-fabric-control-v1", MaxSessionDuration: 3600,
        PermissionsBoundary: { Ref: "WriterBoundary" },
        AssumeRolePolicyDocument: {
          Version: "2012-10-17",
          Statement: [{ Effect: "Allow", Principal: { Federated: existingProviderArn ?? { Ref: "FlyIdentityProvider" } }, Action: "sts:AssumeRoleWithWebIdentity",
            Condition: { StringEquals: { [`${issuer}:aud`]: "sts.amazonaws.com" }, StringLike: { [`${issuer}:sub`]: `${flyOrg}:${controlApp}:*` } } }],
        },
        Policies: [{ PolicyName: "RudderFabricObjects", PolicyDocument: permission }, cachePolicy],
        Tags: tags,
      },
    },
    ContentPolicy: {
      Type: "AWS::S3::BucketPolicy",
      Properties: {
        Bucket: { Ref: "ContentBucket" },
        PolicyDocument: { Version: "2012-10-17", Statement: [
          { Sid: "RequireTLS", Effect: "Deny", Principal: "*", Action: "s3:*", Resource: [bucket, sub("${ContentBucket.Arn}/*")], Condition: { Bool: { "aws:SecureTransport": "false" } } },
          { Sid: "RequireImmutableConditionalPut", Effect: "Deny", Principal: "*", Action: "s3:PutObject", Resource: immutable,
            Condition: { Null: { "s3:if-none-match": "true" }, Bool: { "s3:ObjectCreationOperation": "true" } } },
          { Sid: "RequireStagingCAS", Effect: "Deny", Principal: "*", Action: "s3:PutObject", Resource: sub("${ContentBucket.Arn}/fabric/v1/accounts/*/staging/*"),
            Condition: { Null: { "s3:if-none-match": "true", "s3:if-match": "true" }, Bool: { "s3:ObjectCreationOperation": "true" } } },
          { Sid: "WriterCannotSweepRetainedContent", Effect: "Deny", Principal: "*", Action: ["s3:DeleteObject", "s3:DeleteObjectVersion"], Resource: immutable,
            Condition: { ArnEquals: { "aws:PrincipalArn": attr("FabricRole", "Arn") } } },
        ] },
      },
    },
  };
  if (!existingProviderArn) resources.FlyIdentityProvider = {
    Type: "AWS::IAM::OIDCProvider", Properties: { Url: `https://${issuer}`, ClientIdList: ["sts.amazonaws.com"], Tags: tags },
  };
  return {
    AWSTemplateFormatVersion: "2010-09-09",
    Description: "Rudder immutable Fabric CAS with private versioned retention and Fly workload identity; no access keys or automatic GC",
    Metadata: { RudderComponent: "FabricV1", AccountId: accountId, FlyOrg: flyOrg, ControlApp: controlApp },
    Resources: resources,
    Outputs: { BucketName: { Value: { Ref: "ContentBucket" } }, RoleArn: { Value: attr("FabricRole", "Arn") }, OidcIssuer: { Value: `https://${issuer}` } },
  };
}

/** Allow only the account cache grant in the boundary and the role. */
export function validateRemoteBuildCacheUpdate(previous, next) {
  const expected = JSON.parse(JSON.stringify(previous));
  const boundary = expected.Resources.WriterBoundary.Properties.PolicyDocument;
  const policies = expected.Resources.FabricRole.Properties.Policies;
  if (boundary.Statement.some((rule) => rule.Sid === cacheObjects.Sid)
      || policies.some((policy) => policy.PolicyName === cachePolicy.PolicyName)) {
    throw new Error("Remote build cache policy already present in the stack template");
  }
  boundary.Statement.push(cacheObjects);
  policies.push(cachePolicy);
  if (!isDeepStrictEqual(expected, next)) throw new Error("Unexpected infrastructure change outside the remote build cache grant");
}
