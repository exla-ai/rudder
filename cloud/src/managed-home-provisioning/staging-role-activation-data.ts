/** Historical completed-publication DATA, never an issuer. Only the original
 * private-branded publisher permit authorizes the separate admin transition. */
export interface StagingDatabaseIdentity {
 database:string;database_oid:string;postmaster_started:string;
}
export interface StagingRoleActivationFact {
 format_version:1;role:"sdk"|"runtime";authority_id:string;policy_sha256:string;
 installation_policy_sha256:string;database:StagingDatabaseIdentity;
 prepared:Record<string,any>;completion:Record<string,any>;completion_sha256:string;
}
export const STAGING_DATABASE_IDENTITY_SQL=`SELECT current_database() AS database,
 (SELECT oid::text FROM pg_catalog.pg_database WHERE datname=current_database()) AS database_oid,
 pg_postmaster_start_time()::text AS postmaster_started`;
export function stagingDatabaseIdentityData(value:unknown):StagingDatabaseIdentity {
 const v=value as StagingDatabaseIdentity;
 if(!v||typeof v.database!=="string"||!v.database||!/^\d+$/.test(v.database_oid)
   ||typeof v.postmaster_started!=="string"||!v.postmaster_started||v.postmaster_started.length>80)
  throw Error("Original staging database identity unavailable");
 return {database:v.database,database_oid:v.database_oid,postmaster_started:v.postmaster_started};
}
