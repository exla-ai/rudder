import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {check,fail,sha} from "./types.js";
export const renewalSql=`
CREATE TABLE ${SCHEMA}.managed_owner_clock_installation(authority_id uuid PRIMARY KEY REFERENCES ${SCHEMA}.authorities,
 state text NOT NULL CHECK(state IN('frozen','ready','retired')),policy_sha256 text NOT NULL,migration_sha256 text NOT NULL);
CREATE TABLE ${SCHEMA}.managed_owner_renewals(
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,project_id uuid NOT NULL,
 request_id uuid NOT NULL,request_nonce uuid NOT NULL,owner_epoch bigint NOT NULL CHECK(owner_epoch>0),lease_id uuid NOT NULL,
 request_sha256 text NOT NULL,request_json jsonb NOT NULL CHECK(pg_column_size(request_json)<=24576),
 state text NOT NULL CHECK(state IN('started','replied')),ack_json jsonb CHECK(pg_column_size(ack_json)<=24576),
 PRIMARY KEY(authority_id,account_id,home_id,request_id),UNIQUE(authority_id,account_id,request_nonce),
 FOREIGN KEY(authority_id,account_id,project_id) REFERENCES ${SCHEMA}.managed_home_fresh_projects);
CREATE UNIQUE INDEX one_pending_managed_owner_renewal ON ${SCHEMA}.managed_owner_renewals(authority_id,account_id,home_id,project_id,owner_epoch,lease_id) WHERE state='started';
ALTER TABLE ${SCHEMA}.managed_owner_renewals ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.managed_owner_renewals FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.managed_owner_renewals USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));
CREATE TABLE ${SCHEMA}.managed_owner_renewal_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
`;
export const RENEWAL_MIGRATION_SHA256=createHash("sha256").update(renewalSql).digest("hex");
/** Clock safety and provider continuity require separately qualified operator
 * admission. Installing this schema cannot activate an authority or policy. */
export async function migrateManagedOwnerRenewals(pool:AuthorityPool,authorityId:string,clockPolicySha256:string,role:string){check(pool.role==="migration"&&/^[a-z][a-z0-9_]{0,62}$/.test(role));sha(clockPolicySha256);
 return pool.run("",async tx=>{await tx.query("SELECT pg_advisory_xact_lock(1824529,5)");const old=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_owner_renewal_migrations"]);if(!old.rows[0].name){await tx.query(renewalSql);await tx.query(`INSERT INTO ${SCHEMA}.managed_owner_renewal_migrations VALUES(4,$1)`,[RENEWAL_MIGRATION_SHA256]);}
  const version=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_owner_renewal_migrations WHERE version=4`);check(version.rows[0]?.sha256===RENEWAL_MIGRATION_SHA256,"unconfigured");await tx.query(`INSERT INTO ${SCHEMA}.managed_owner_clock_installation VALUES($1,'frozen',$2,$3) ON CONFLICT DO NOTHING`,[authorityId,clockPolicySha256,RENEWAL_MIGRATION_SHA256]);const clock=await tx.query(`SELECT policy_sha256,migration_sha256 FROM ${SCHEMA}.managed_owner_clock_installation WHERE authority_id=$1`,[authorityId]);check(clock.rows[0]?.policy_sha256===clockPolicySha256&&clock.rows[0]?.migration_sha256===RENEWAL_MIGRATION_SHA256,"unconfigured");const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");const r='"'+role+'"';await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_owner_clock_installation,${SCHEMA}.managed_owner_renewals,${SCHEMA}.managed_owner_renewal_migrations FROM PUBLIC;
   GRANT SELECT ON ${SCHEMA}.managed_owner_clock_installation,${SCHEMA}.managed_owner_renewals,${SCHEMA}.managed_owner_renewal_migrations TO ${r};
   GRANT INSERT ON ${SCHEMA}.managed_owner_renewals TO ${r};GRANT UPDATE(state,ack_json) ON ${SCHEMA}.managed_owner_renewals TO ${r};`);return {version:4,state:"frozen-or-existing" as const};
 }).catch(()=>fail("unavailable"));}
