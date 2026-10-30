BEGIN; INSERT INTO q_results(label,receipt) VALUES('discarded-reply',q_claim('lostreply','a','boot-a',1,'lost-reply')); COMMIT; SELECT pg_sleep(30);
