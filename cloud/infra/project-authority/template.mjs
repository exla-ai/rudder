// Pure CloudFormation generator. No SDK, environment/profile reads or deploy path.
// Matches infra/fabric-store.mjs's JSON style, with all spend disabled by default.
export function projectAuthorityTemplate() {
  const ref = (name) => ({ Ref: name });
  const sub = (value) => ({ "Fn::Sub": value });
  const attr = (name, key) => ({ "Fn::GetAtt": [name, key] });
  const tags = [{ Key: "Project", Value: "Rudder" }, { Key: "Component", Value: "ProjectAuthorityPreproduction" }];
  const resource = (Type, Properties, retain = false) => ({
    Type, Condition: "CreateReviewedPreproduction",
    ...(retain ? { DeletionPolicy: "Retain", UpdateReplacePolicy: "Retain" } : {}), Properties,
  });
  const key = (description) => resource("AWS::KMS::Key", {
    Description: description, EnableKeyRotation: true, PendingWindowInDays: 30,
    KeyPolicy: { Version: "2012-10-17", Statement: [{
      Sid: "AccountAdministeredIAMDelegation", Effect: "Allow",
      Principal: { AWS: sub("arn:${AWS::Partition}:iam::${AWS::AccountId}:root") },
      Action: "kms:*", Resource: "*",
    }] }, Tags: tags,
  }, true);
  const runtimeSecret = ref("RuntimeSecret");
  const masterSecret = attr("Database", "MasterUserSecret.SecretArn");
  const decrypt = (secrets) => ({
    Effect: "Allow", Action: "kms:Decrypt", Resource: attr("SecretsKey", "Arn"),
    Condition: { StringEquals: {
      "kms:ViaService": sub("secretsmanager.${AWS::Region}.${AWS::URLSuffix}"),
      "kms:EncryptionContext:SecretARN": secrets,
    } },
  });
  const output = (Value) => ({ Condition: "CreateReviewedPreproduction", Value });
  const nonempty = (name) => ({ "Fn::Not": [{ "Fn::Equals": [ref(name), ""] }] });
  return {
    AWSTemplateFormatVersion: "2010-09-09",
    Description: "UNAPPLIED Rudder private PostgreSQL authority candidate; disabled by default; no transport, application activation or provider qualification",
    Metadata: {
      RudderComponent: "ProjectAuthorityPreproduction",
      ReviewDate: "2026-10-30",
      ProviderStatus: "Candidate only; provider failure envelope and transport unresolved",
      WorkloadIdentity: "Reuse existing Fly OIDC provider and STS pattern; this stack grants no Fly runtime access",
      RuntimePolicy: "Unattached; future transport must assign its own reviewed principal",
    },
    Parameters: {
      ProvisioningState: { Type: "String", Default: "disabled", AllowedValues: ["disabled", "reviewed-preproduction"], Description: "Cost approval and reviewed change set required before changing this value; not authority activation" },
      PostgreSQL17MinorVersion: { Type: "String", Default: "", AllowedPattern: "^$|^17\\.[0-9]+$", Description: "Required explicit available 17.x minor, checked against us-east-1 orderability before any deployment" },
      AvailabilityZoneA: { Type: "String", Default: "", AllowedPattern: "^$|^us-east-1[a-z]$" },
      AvailabilityZoneB: { Type: "String", Default: "", AllowedPattern: "^$|^us-east-1[a-z]$" },
      OperatorPrincipalArn: { Type: "String", Default: "", AllowedPattern: "^$|^arn:aws:iam::[0-9]{12}:role/[A-Za-z0-9+=,.@_/-]+$", Description: "Explicit existing operator role with upstream MFA/SSO policy; never an application role" },
      DBInstanceClass: { Type: "String", Default: "db.t4g.small", AllowedValues: ["db.t4g.small", "db.m7g.large"], Description: "Small is a burstable preproduction sizing candidate; production sizing needs measured memory and credit headroom" },
      DBInstanceIdentifier: { Type: "String", Default: "rudder-authority-preprod", AllowedPattern: "^[a-z][a-z0-9-]{0,40}[a-z0-9]$" },
    },
    Rules: {
      ReviewedPreproductionInputs: {
        RuleCondition: { "Fn::Equals": [ref("ProvisioningState"), "reviewed-preproduction"] },
        Assertions: [
          { Assert: { "Fn::Equals": [ref("AWS::Region"), "us-east-1"] }, AssertDescription: "Only the reviewed us-east-1 region is supported" },
          ...["PostgreSQL17MinorVersion", "AvailabilityZoneA", "AvailabilityZoneB", "OperatorPrincipalArn"].map(name => ({ Assert: nonempty(name), AssertDescription: `${name} must be explicitly reviewed` })),
          { Assert: { "Fn::Not": [{ "Fn::Equals": [ref("AvailabilityZoneA"), ref("AvailabilityZoneB")] }] }, AssertDescription: "Two distinct available AZs are required" },
        ],
      },
    },
    Conditions: { CreateReviewedPreproduction: { "Fn::Equals": [ref("ProvisioningState"), "reviewed-preproduction"] } },
    Resources: {
      Vpc: resource("AWS::EC2::VPC", { CidrBlock: "10.48.0.0/16", EnableDnsSupport: true, EnableDnsHostnames: true, Tags: tags }, true),
      PrivateSubnetA: resource("AWS::EC2::Subnet", { VpcId: ref("Vpc"), CidrBlock: "10.48.0.0/24", AvailabilityZone: ref("AvailabilityZoneA"), MapPublicIpOnLaunch: false, Tags: tags }, true),
      PrivateSubnetB: resource("AWS::EC2::Subnet", { VpcId: ref("Vpc"), CidrBlock: "10.48.1.0/24", AvailabilityZone: ref("AvailabilityZoneB"), MapPublicIpOnLaunch: false, Tags: tags }, true),
      PrivateRoutes: resource("AWS::EC2::RouteTable", { VpcId: ref("Vpc"), Tags: tags }, true),
      PrivateRouteAssociationA: resource("AWS::EC2::SubnetRouteTableAssociation", { SubnetId: ref("PrivateSubnetA"), RouteTableId: ref("PrivateRoutes") }, true),
      PrivateRouteAssociationB: resource("AWS::EC2::SubnetRouteTableAssociation", { SubnetId: ref("PrivateSubnetB"), RouteTableId: ref("PrivateRoutes") }, true),
      DatabaseSecurityGroup: resource("AWS::EC2::SecurityGroup", {
        GroupDescription: "Private authority; inbound only from separately attached reviewed VPC clients", VpcId: ref("Vpc"),
        // Explicit restricted egress avoids EC2's implicit allow-all rule.
        SecurityGroupEgress: [{ IpProtocol: "tcp", FromPort: 5432, ToPort: 5432, CidrIp: "10.48.0.0/16", Description: "PostgreSQL only within the dedicated VPC" }], Tags: tags,
      }, true),
      RuntimeClientSecurityGroup: resource("AWS::EC2::SecurityGroup", {
        GroupDescription: "Unattached future VPC authority API; no inbound listener or transport is created", VpcId: ref("Vpc"),
        SecurityGroupEgress: [{ IpProtocol: "tcp", FromPort: 5432, ToPort: 5432, DestinationSecurityGroupId: ref("DatabaseSecurityGroup") }], Tags: tags,
      }, true),
      OperatorClientSecurityGroup: resource("AWS::EC2::SecurityGroup", {
        GroupDescription: "Unattached one-off private migration runner; separate from application", VpcId: ref("Vpc"),
        SecurityGroupEgress: [{ IpProtocol: "tcp", FromPort: 5432, ToPort: 5432, DestinationSecurityGroupId: ref("DatabaseSecurityGroup") }], Tags: tags,
      }, true),
      RuntimeDatabaseIngress: resource("AWS::EC2::SecurityGroupIngress", { GroupId: ref("DatabaseSecurityGroup"), IpProtocol: "tcp", FromPort: 5432, ToPort: 5432, SourceSecurityGroupId: ref("RuntimeClientSecurityGroup"), Description: "Only future reviewed VPC API clients" }, true),
      OperatorDatabaseIngress: resource("AWS::EC2::SecurityGroupIngress", { GroupId: ref("DatabaseSecurityGroup"), IpProtocol: "tcp", FromPort: 5432, ToPort: 5432, SourceSecurityGroupId: ref("OperatorClientSecurityGroup"), Description: "Only separately authorized migration runner" }, true),
      DatabaseSubnets: resource("AWS::RDS::DBSubnetGroup", { DBSubnetGroupDescription: "Two isolated AZ subnets for traditional Multi-AZ RDS instance", SubnetIds: [ref("PrivateSubnetA"), ref("PrivateSubnetB")], Tags: tags }, true),
      StorageKey: key("Retained Rudder authority RDS storage and snapshots"),
      SecretsKey: key("Retained separate Rudder authority Secrets Manager encryption"),
      DatabaseParameters: resource("AWS::RDS::DBParameterGroup", {
        Description: "PostgreSQL17 authority durability and TLS floor; exact parameter availability must be checked before deployment", Family: "postgres17",
        Parameters: {
          "rds.force_ssl": "1", ssl_min_protocol_version: "TLSv1.2", password_encryption: "scram-sha-256", synchronous_commit: "on",
          log_statement: "none", log_min_error_statement: "panic", log_parameter_max_length: "0", log_parameter_max_length_on_error: "0", log_min_duration_statement: "-1", log_duration: "0",
        }, Tags: tags,
      }, true),
      Database: resource("AWS::RDS::DBInstance", {
        DBInstanceIdentifier: ref("DBInstanceIdentifier"), DBName: "rudder_authority", DBInstanceClass: ref("DBInstanceClass"),
        Engine: "postgres", EngineVersion: ref("PostgreSQL17MinorVersion"), EngineLifecycleSupport: "open-source-rds-extended-support-disabled",
        MultiAZ: true, PubliclyAccessible: false, NetworkType: "IPV4", Port: "5432",
        DBSubnetGroupName: ref("DatabaseSubnets"), VPCSecurityGroups: [ref("DatabaseSecurityGroup")], DBParameterGroupName: ref("DatabaseParameters"),
        StorageType: "gp3", AllocatedStorage: "20", MaxAllocatedStorage: 100, StorageEncrypted: true, KmsKeyId: attr("StorageKey", "Arn"),
        MasterUsername: "rudder_administrator", ManageMasterUserPassword: true, MasterUserSecret: { KmsKeyId: attr("SecretsKey", "Arn") },
        EnableIAMDatabaseAuthentication: false, CACertificateIdentifier: "rds-ca-rsa2048-g1",
        BackupRetentionPeriod: 35, PreferredBackupWindow: "06:00-06:30", DeleteAutomatedBackups: false, CopyTagsToSnapshot: true, DeletionProtection: true,
        AutoMinorVersionUpgrade: false, AllowMajorVersionUpgrade: false, PreferredMaintenanceWindow: "sun:07:00-sun:08:00", Tags: tags,
      }, true),
      RuntimeSecret: resource("AWS::SecretsManager::Secret", {
        Description: "Generated candidate runtime credential; not installed in PostgreSQL and not granted to a runtime principal by this stack", KmsKeyId: attr("SecretsKey", "Arn"),
        GenerateSecretString: { SecretStringTemplate: JSON.stringify({ username: "rudder_authority_runtime", dbname: "rudder_authority" }), GenerateStringKey: "password", PasswordLength: 40, ExcludeCharacters: "\"'\\" }, Tags: tags,
      }, true),
      RuntimeSecretReadPolicy: resource("AWS::IAM::ManagedPolicy", {
        Description: "Unattached least-privilege runtime credential policy; future transport must review and attach explicitly",
        PolicyDocument: { Version: "2012-10-17", Statement: [
          { Effect: "Allow", Action: "secretsmanager:GetSecretValue", Resource: runtimeSecret }, decrypt(runtimeSecret),
        ] },
      }),
      OperatorRole: resource("AWS::IAM::Role", {
        Description: "Separate migration/bootstrap operator; no application trust or infrastructure modification permission", MaxSessionDuration: 3600,
        AssumeRolePolicyDocument: { Version: "2012-10-17", Statement: [{ Effect: "Allow", Principal: { AWS: ref("OperatorPrincipalArn") }, Action: "sts:AssumeRole" }] },
        Policies: [{ PolicyName: "PrivateMigrationCredentials", PolicyDocument: { Version: "2012-10-17", Statement: [
          { Effect: "Allow", Action: "secretsmanager:GetSecretValue", Resource: [masterSecret, runtimeSecret] }, decrypt([masterSecret, runtimeSecret]),
        ] } }], Tags: tags,
      }),
    },
    Outputs: {
      WriterEndpoint: output(attr("Database", "Endpoint.Address")), WriterPort: output(attr("Database", "Endpoint.Port")),
      DbiResourceId: output(attr("Database", "DbiResourceId")), DatabaseArn: output(attr("Database", "DBInstanceArn")),
      VpcId: output(ref("Vpc")), PrivateSubnetA: output(ref("PrivateSubnetA")), PrivateSubnetB: output(ref("PrivateSubnetB")),
      RuntimeClientSecurityGroupId: output(ref("RuntimeClientSecurityGroup")), OperatorClientSecurityGroupId: output(ref("OperatorClientSecurityGroup")),
      RuntimeSecretArn: output(runtimeSecret), MasterSecretArn: output(masterSecret),
      RuntimeSecretReadPolicyArn: output(ref("RuntimeSecretReadPolicy")), OperatorRoleArn: output(attr("OperatorRole", "Arn")),
    },
  };
}
