BEGIN; SELECT q_lock('topic'); SELECT pg_sleep(1.5); INSERT INTO q_results(label,receipt) VALUES('topic-writer',q_topic('topic','policy-2',1)); COMMIT;
