BEGIN;
DROP FUNCTION IF EXISTS semantic_readiness(text,text);
DROP FUNCTION IF EXISTS search_hybrid_current_memories(text,text,text,text,integer,text[],text,text);
DROP FUNCTION IF EXISTS search_exact_semantic_candidates(text,text,text,text,text[],integer);
DROP FUNCTION IF EXISTS cancel_embedding_job(text,text,bigint);
DROP FUNCTION IF EXISTS fail_embedding_job(text,text,bigint,text);
DROP FUNCTION IF EXISTS complete_embedding_job(text,text,bigint,text);
DROP FUNCTION IF EXISTS claim_embedding_jobs(integer,integer);
DROP TRIGGER IF EXISTS selected_embedding_generation_intent ON active_embedding_generations;
DROP FUNCTION IF EXISTS enqueue_selected_embedding_generation();
DROP TRIGGER IF EXISTS passage_embedding_intent ON source_passages;
DROP TRIGGER IF EXISTS lexical_embedding_intent ON lexical_representations;
DROP FUNCTION IF EXISTS enqueue_embedding_job();
DROP TABLE IF EXISTS embedding_representations;
DROP TABLE IF EXISTS embedding_jobs;
DROP TABLE IF EXISTS active_embedding_generations;
DROP TABLE IF EXISTS embedding_generations;
-- The shared vector extension is a platform capability, not N3-owned data. Keep it on rollback.
COMMIT;
