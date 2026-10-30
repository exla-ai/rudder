-- Generated without changing either documented UPDATE; parameter names become typed function arguments.
SET search_path=rudder_fleet_design,pg_catalog;
CREATE FUNCTION q_documented_renew(p_a uuid,p_acct text,p_p uuid,p_epoch bigint,p_engine uuid,p_lease uuid,p_expected_serial bigint,p_db_now timestamptz)
RETURNS TABLE(owner_epoch bigint,lease_id uuid,renew_serial bigint,lease_until timestamptz)
LANGUAGE sql VOLATILE SET search_path=rudder_fleet_design,pg_catalog AS $documented$
UPDATE projects SET lease_until = CAST(p_db_now AS timestamptz) + interval '15 seconds',
                    renew_serial = renew_serial + 1
 WHERE authority_id = p_a AND account_id = p_acct AND project_id = p_p
   AND mode = 'ready' AND owner_epoch = p_epoch
   AND owner_engine_instance = p_engine AND lease_id = p_lease
   AND renew_serial = p_expected_serial AND lease_until > CAST(p_db_now AS timestamptz)
 RETURNING owner_epoch, lease_id, renew_serial, lease_until;
$documented$;
CREATE FUNCTION q_documented_integration(p_a uuid,p_acct text,p_p uuid,p_epoch bigint,p_owner_engine uuid,p_lease uuid,p_expected_version bigint,p_old_head text,p_new_head text,p_source_root text,p_environment_root text,p_verification uuid)
RETURNS TABLE(integration_version bigint,event_seq bigint)
LANGUAGE sql VOLATILE SET search_path=rudder_fleet_design,pg_catalog AS $documented$
UPDATE projects AS p
   SET head_vcs_commit = p_new_head, head_source_root = p_source_root,
       head_environment_root = p_environment_root,
       integration_version = integration_version + 1,
       event_seq = event_seq + 1
 WHERE p.authority_id = p_a AND p.account_id = p_acct AND p.project_id = p_p
   AND p.mode = 'ready' AND p.owner_epoch = p_epoch
   AND p.owner_engine_instance = p_owner_engine AND p.lease_id = p_lease
   AND p.lease_until > clock_timestamp()
   AND p.integration_version = p_expected_version AND p.head_vcs_commit = p_old_head
   AND EXISTS (
     SELECT 1 FROM verifications v
      WHERE v.authority_id = p.authority_id AND v.account_id = p.account_id
        AND v.project_id = p.project_id AND v.verification_id = p_verification
        AND v.owner_epoch = p.owner_epoch
        AND v.base_version = p.integration_version AND v.base_head = p.head_vcs_commit
        AND v.tested_head = p_new_head AND v.tested_source_root = p_source_root
        AND v.tested_environment_root = p_environment_root
        AND NOT EXISTS (
          SELECT 1 FROM verification_topics vt LEFT JOIN topics t
            ON t.authority_id = vt.authority_id AND t.account_id = vt.account_id
           AND t.project_id = vt.project_id AND t.topic = vt.topic
           WHERE vt.authority_id = v.authority_id AND vt.account_id = v.account_id
             AND vt.project_id = v.project_id AND vt.verification_id = v.verification_id
             AND COALESCE(t.version, 0) <> vt.expected_version))
 RETURNING integration_version, event_seq;
$documented$;
