use super::*;

pub(super) fn initialize(conn: &mut Connection) -> Result<()> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(version <= 5, "journal schema is newer than this engine");
    if version >= 4 {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(version <= 5, "journal schema is newer than this engine");
    if version == 0 {
        tx.execute_batch(r#"
CREATE TABLE projects(
 project_id TEXT PRIMARY KEY, authority_kind TEXT NOT NULL CHECK(authority_kind='local'),
 owner_id TEXT NOT NULL, owner_epoch INTEGER NOT NULL CHECK(owner_epoch>0), owner_active INTEGER NOT NULL CHECK(owner_active IN(0,1)),
 integration_head TEXT NOT NULL, event_seq INTEGER NOT NULL CHECK(event_seq>=0));
CREATE TABLE runs(project_id TEXT NOT NULL REFERENCES projects(project_id),run_id TEXT NOT NULL,epoch INTEGER NOT NULL CHECK(epoch>0),instance_id TEXT,PRIMARY KEY(project_id,run_id));
CREATE TABLE events(project_id TEXT NOT NULL REFERENCES projects(project_id),sequence INTEGER NOT NULL CHECK(sequence>0),event_id TEXT NOT NULL UNIQUE,kind TEXT NOT NULL,principal TEXT NOT NULL,payload_json TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(project_id,sequence));
CREATE TABLE capabilities(token_hash TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(project_id),grant_json TEXT NOT NULL,revoked INTEGER NOT NULL CHECK(revoked IN(0,1)),issued_at INTEGER NOT NULL);
CREATE TABLE commands(command_id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(project_id),principal TEXT NOT NULL,sender_run TEXT NOT NULL,sender_epoch INTEGER NOT NULL,idempotency_key TEXT NOT NULL,request_json TEXT NOT NULL,target_run TEXT NOT NULL,target_epoch INTEGER NOT NULL,kind TEXT NOT NULL,payload_json TEXT NOT NULL,accepted_sequence INTEGER NOT NULL,accepted_at INTEGER NOT NULL,state TEXT NOT NULL CHECK(state IN('accepted','started','uncertain','completed')),result_json TEXT,UNIQUE(project_id,principal,idempotency_key),FOREIGN KEY(project_id,target_run) REFERENCES runs(project_id,run_id));
CREATE TABLE effects(command_id TEXT PRIMARY KEY REFERENCES commands(command_id),state TEXT NOT NULL CHECK(state IN('pending','started','uncertain','completed')),claim_id TEXT,owner_epoch INTEGER,started_at INTEGER);
CREATE INDEX effects_pending ON effects(state,command_id);
CREATE INDEX commands_target ON commands(project_id,target_run,accepted_sequence);
CREATE TABLE messages(message_id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(project_id),principal TEXT NOT NULL,idempotency_key TEXT NOT NULL,request_json TEXT NOT NULL,envelope_json TEXT NOT NULL,sender_run TEXT NOT NULL,sender_epoch INTEGER NOT NULL,recipient_run TEXT NOT NULL,recipient_epoch INTEGER NOT NULL,sequence INTEGER NOT NULL,expires_at INTEGER NOT NULL,byte_size INTEGER NOT NULL,request_id TEXT,reply_to TEXT REFERENCES messages(message_id),UNIQUE(project_id,principal,idempotency_key),FOREIGN KEY(project_id,recipient_run) REFERENCES runs(project_id,run_id));
CREATE UNIQUE INDEX message_requests ON messages(project_id,sender_run,sender_epoch,request_id) WHERE request_id IS NOT NULL;
CREATE TABLE deliveries(message_id TEXT PRIMARY KEY REFERENCES messages(message_id),project_id TEXT NOT NULL,recipient_run TEXT NOT NULL,recipient_epoch INTEGER NOT NULL,sequence INTEGER NOT NULL,status TEXT NOT NULL CHECK(status IN('pending','acked','skipped','expired')),ack_kind TEXT,updated_at INTEGER NOT NULL);
CREATE INDEX inbox_pending ON deliveries(project_id,recipient_run,recipient_epoch,status,sequence);
CREATE TABLE mailbox_cursors(project_id TEXT NOT NULL,run_id TEXT NOT NULL,run_epoch INTEGER NOT NULL,cursor INTEGER NOT NULL CHECK(cursor>=0),PRIMARY KEY(project_id,run_id,run_epoch));
CREATE TABLE topics(project_id TEXT NOT NULL REFERENCES projects(project_id),topic TEXT NOT NULL,version INTEGER NOT NULL CHECK(version>0),decision_id TEXT NOT NULL,PRIMARY KEY(project_id,topic));
CREATE TABLE decisions(decision_id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(project_id),topic TEXT NOT NULL,version INTEGER NOT NULL,principal TEXT NOT NULL,scope TEXT NOT NULL,statement_json TEXT NOT NULL,previous_id TEXT REFERENCES decisions(decision_id),sequence INTEGER NOT NULL,idempotency_key TEXT NOT NULL,request_json TEXT NOT NULL,UNIQUE(project_id,topic,version),UNIQUE(project_id,principal,idempotency_key));
CREATE TABLE integrations(integration_id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(project_id),previous_head TEXT NOT NULL,new_head TEXT NOT NULL,owner_epoch INTEGER NOT NULL,versions_json TEXT NOT NULL,evidence_json TEXT NOT NULL,sequence INTEGER NOT NULL,idempotency_key TEXT NOT NULL,request_json TEXT NOT NULL,receipt_json TEXT NOT NULL,UNIQUE(project_id,idempotency_key));
PRAGMA user_version=1;
"#)?;
    }
    if version <= 1 {
        tx.execute_batch(r#"
CREATE TABLE runtime_observations(
 project_id TEXT NOT NULL, run_id TEXT NOT NULL,
 run_epoch INTEGER NOT NULL CHECK(run_epoch>0), owner_epoch INTEGER NOT NULL CHECK(owner_epoch>0),
 instance_id TEXT NOT NULL, observation_json TEXT NOT NULL, observed_at INTEGER NOT NULL,
 PRIMARY KEY(project_id,run_id), FOREIGN KEY(project_id,run_id) REFERENCES runs(project_id,run_id));
PRAGMA user_version=2;
"#)?;
    }
    if version <= 2 {
        tx.execute_batch("ALTER TABLE commands ADD COLUMN capability_hash TEXT REFERENCES capabilities(token_hash); PRAGMA user_version=3;")?;
    }
    if version <= 3 {
        tx.execute_batch(r#"
CREATE TABLE vcs_intents(
 intent_id TEXT PRIMARY KEY,
 project_id TEXT NOT NULL REFERENCES projects(project_id),
 idempotency_key TEXT NOT NULL,
 request_json TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN('accepted','prepared','started','uncertain','published','rejected')),
 receipt_json TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 UNIQUE(project_id,idempotency_key));
CREATE INDEX vcs_intents_pending ON vcs_intents(project_id,state);
PRAGMA user_version=4;
"#)?;
    }
    tx.commit()?;
    Ok(())
}
