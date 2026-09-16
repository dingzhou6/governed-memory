BEGIN;
DO $migration$
DECLARE
  definition text;
  old_predicate constant text := 'AND p_max_context_bytes BETWEEN 1 AND 4096';
  new_predicate constant text := 'AND p_max_context_bytes BETWEEN 1 AND 65535';
BEGIN
  SELECT pg_get_functiondef('public.search_current_memories_query_core(text,text,text,text,integer,text[],tsquery)'::regprocedure) INTO definition;
  IF (length(definition)-length(replace(definition,old_predicate,'')))/length(old_predicate) <> 1 THEN
    RAISE EXCEPTION 'unexpected search context admission definition';
  END IF;
  EXECUTE replace(definition,old_predicate,new_predicate);
END
$migration$;
COMMIT;
