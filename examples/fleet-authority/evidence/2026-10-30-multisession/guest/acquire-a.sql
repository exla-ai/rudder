BEGIN; SELECT q_lock('acquire'); SELECT pg_sleep(1.5); INSERT INTO q_results VALUES('acquire-a',q_claim('acquire','a','boot-a',1,'claim-a'),pg_backend_pid(),clock_timestamp()); COMMIT;
