BEGIN; INSERT INTO q_results(label,receipt) VALUES('server-crash-uncommitted',q_claim('server-crash','a','boot-a',1,'never-committed')); SELECT pg_sleep(30); COMMIT;
