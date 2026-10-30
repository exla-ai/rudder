BEGIN; SELECT q_lock('clock'); SELECT pg_sleep(2); COMMIT;
