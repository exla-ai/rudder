BEGIN; SELECT q_lock('head'); SELECT pg_sleep(1.5); INSERT INTO q_results(label,receipt) VALUES('head-a',q_integrate('head','a','boot-a',2,'a','a','integration-a')); COMMIT;
