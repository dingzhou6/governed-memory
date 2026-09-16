BEGIN;
DROP FUNCTION IF EXISTS search_current_memories_disjunctive_v2(text,text,text,text,integer,text[]);
DROP FUNCTION IF EXISTS lexical_query_disjunctive_v2(text);
COMMIT;
