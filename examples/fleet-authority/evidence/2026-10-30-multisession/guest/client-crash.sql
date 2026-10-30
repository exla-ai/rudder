BEGIN; INSERT INTO q_results(label,receipt) VALUES('client-crash-uncommitted',q_claim('client-crash','a','boot-a',1,'never-committed')); SELECT pg_sleep(30); COMMIT;
