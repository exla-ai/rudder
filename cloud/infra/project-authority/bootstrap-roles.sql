-- UNAPPLIED. Run only through a separately authorized private operator channel
-- against the new dedicated rudder_authority database as its administrator.
-- Contains no passwords; generated credentials are NOT installed by this file.
-- Existing active/mismatched roles fail closed instead of being taken over.
BEGIN;
SELECT pg_advisory_xact_lock(1824529, 2);
DO $bootstrap$
BEGIN
  IF current_database() <> 'rudder_authority' THEN
    RAISE EXCEPTION 'Wrong bootstrap database';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'rudder_authority_runtime') THEN
    CREATE ROLE rudder_authority_runtime NOLOGIN NOSUPERUSER NOCREATEDB
      NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 16;
  END IF;
  IF EXISTS (
    SELECT FROM pg_roles r WHERE r.rolname = 'rudder_authority_runtime'
      AND (r.rolcanlogin OR r.rolsuper OR r.rolcreatedb OR r.rolcreaterole
           OR r.rolinherit OR r.rolreplication OR r.rolbypassrls
           OR r.rolconnlimit <> 16
           OR EXISTS (SELECT FROM pg_auth_members m WHERE m.member = r.oid))
  ) THEN
    RAISE EXCEPTION 'Runtime role requires explicit review';
  END IF;
END
$bootstrap$;
REVOKE ALL ON DATABASE rudder_authority FROM PUBLIC;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
REVOKE ALL ON DATABASE rudder_authority FROM rudder_authority_runtime;
GRANT CONNECT ON DATABASE rudder_authority TO rudder_authority_runtime;
ALTER ROLE rudder_authority_runtime IN DATABASE rudder_authority SET search_path = pg_catalog;
ALTER ROLE rudder_authority_runtime IN DATABASE rudder_authority SET statement_timeout = '1500ms';
ALTER ROLE rudder_authority_runtime IN DATABASE rudder_authority SET lock_timeout = '250ms';
ALTER ROLE rudder_authority_runtime IN DATABASE rudder_authority SET idle_in_transaction_session_timeout = '2000ms';
COMMIT;
-- Next, separately: createAuthorityMigrator(...).migrate('rudder_authority_runtime')
-- installs the checked schema and a FROZEN fresh authority incarnation.
-- Installing a generated password and granting LOGIN is another gated step.
-- This bootstrap, a role login, and a CloudFormation parameter NEVER activate
-- authority or prove that incompatible legacy writers have physically stopped.
