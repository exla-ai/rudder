# Unapplied project authority foundation

Read the [provider and rollout review](../../../docs/architecture/authority-provider-rollout.md)
before using this directory. **Nothing here has been deployed.** Provider choice,
Fly transport, preproduction spend and production activation are separate gates.
No AWS account API, credential store or production database was accessed for this
proposal; pricing capture uses public HTTPS catalogs only.

`private-rds.cfn.json` follows the repository's JSON CloudFormation style under
`infra/fabric-store.*`. `template.mjs` is its pure, deterministic generator. All
19 resources are disabled by default. The enabled template would create an
isolated two-AZ traditional PostgreSQL DB instance, retained encrypted storage
and secrets, separate operator access and an **unattached** runtime secret policy.
It creates neither a Fly identity grant nor a network transport. It cannot activate
the authority library. `rollout.example.json` remains inactive/unresolved and
contains no deployable password or authority incarnation.

`bootstrap-roles.sql` is an **unexecuted** operator fixture for the new dedicated
database. It creates a restricted NOLOGIN runtime role, then leaves migration,
private credential installation and activation as separate reviewed work. It has
not been qualified against the RDS master user's actual privileges. The real PG
adapter evidence elsewhere in the repository does not qualify this provider.

`stack-policy.json` additionally refuses replacement/deletion updates to retained
state and its private dependencies. A stack policy does not prevent stack deletion
or replace deletion protection, retained-resource inventory and a reviewed cleanup
procedure. Changing the condition back to disabled is not application freeze or
resource cleanup; retained infrastructure can continue billing.

Offline validation, from the repository root:

```sh
node cloud/infra/project-authority/validate.mjs
cfn-lint --template cloud/infra/project-authority/private-rds.cfn.json --regions us-east-1 --format json
```

The local validation checks default inactivity, resource/network/credential
boundaries, retained durability settings, generator equality, no runtime policy
attachment, and rejection of 11 unsafe mutations. It also recomputes the review's
USD estimates from exact published SKUs. CloudFormation lint **1.59.0** returned
`[]` with exit code 0 for us-east-1 on 2026-10-30. See [validation.json](validation.json).
AWS `ValidateTemplate`, a change set, IAM policy evaluation, engine/class/AZ/CA
orderability, live RDS SQL/bootstrap, TLS, backup/restore, provider failover and
actual Fly transport have **not** been run. No apply/deploy helper is provided.

Refresh public pricing only when reviewing a new proposal:

```sh
python3 cloud/infra/project-authority/capture-prices.py
node cloud/infra/project-authority/validate.mjs
```

The script reads seven public us-east-1 catalogs without SDKs or credentials and
saves only 19 selected price entries plus full-catalog digests. The large EC2 CSV
is streamed in memory. Refreshing intentionally changes the dated snapshot and
requires reviewing estimates; it is not part of routine offline validation.

Regenerate the JSON offline after an intentional generator change:

```sh
node --input-type=module - <<'JS'
import fs from 'node:fs';
import { projectAuthorityTemplate } from './cloud/infra/project-authority/template.mjs';
fs.writeFileSync('cloud/infra/project-authority/private-rds.cfn.json', JSON.stringify(projectAuthorityTemplate(), null, 2) + '\n');
JS
```

No resource-creation command is documented here because the provider/transport
decision and persistent spend have not been approved. Reuse the existing Fly
OIDC → AWS STS pattern in a future reviewed transport role; preserve the Fabric
S3 role and provider. Do not copy admin AWS credentials into an app or provision
an additional PKI system.
