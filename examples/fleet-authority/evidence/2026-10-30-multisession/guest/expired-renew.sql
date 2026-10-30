BEGIN; SELECT q_lock('clock'); SELECT pg_sleep(1.5); INSERT INTO q_results(label,receipt) VALUES('expired-renew',q_renew('clock','a','boot-a',2,0,'late-renew')); COMMIT;
