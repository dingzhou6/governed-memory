BEGIN;

-- Minimum Slice A: split Han clauses on ideographic comma U+FF0C and enumeration comma U+3001.
-- CREATE OR REPLACE preserves 0008 EXECUTE grants (runtime has clause_union only).

CREATE OR REPLACE FUNCTION lexical_clause_queries_v1(p_query text)
RETURNS TABLE (clause_ordinal integer, native_query tsquery, local_k integer, preparation_status text)
LANGUAGE plpgsql
IMMUTABLE
SET search_path = pg_catalog, public
AS $fn$
DECLARE
  v_clause text;
  v_clauses text[];
  v_ordinal integer := 0;
  v_lexeme text;
  v_part tsquery;
  v_intended tsquery;
  v_cjk text;
  v_gram text;
  v_i integer;
  v_stop text[] := ARRAY[
    'a','an','the','about','of','on','for','in','to','please','and','or','is','are',
    'was','were','be','been','being','this','that','these','those','it','its','as',
    'at','by','with','from','into','over','after','before','not','no','nor','but',
    'if','then','so','than','too','very','can','could','would','should','will',
    'just','also','only','have','has','had','do','does','did','we','you','they',
    'i','m','who','whom','whose','which','what','when','where','why','how','each',
    'every','their','them'
  ];
BEGIN
  IF p_query IS NULL OR octet_length(p_query) NOT BETWEEN 1 AND 4096 OR p_query !~ '[^[:space:]]' THEN
    RETURN;
  END IF;
  IF p_query ~ '"' THEN
    clause_ordinal := 1;
    native_query := public.lexical_query_bounded_question_v1(p_query);
    local_k := NULL;
    preparation_status := 'explicit_quotes';
    IF native_query IS NOT NULL THEN
      RETURN NEXT;
    END IF;
    RETURN;
  END IF;
  SELECT COALESCE(ARRAY(
    SELECT btrim(piece)
    FROM unnest(regexp_split_to_array(p_query, $re$[\n\r。！？!?;；，、]+|\.[[:space:]]+$re$)) AS piece
    WHERE btrim(piece) <> '' AND btrim(piece) ~ '[^[:space:]]'
    LIMIT 16
  ), ARRAY[]::text[]) INTO v_clauses;
  IF cardinality(v_clauses) = 0 THEN
    RETURN;
  END IF;
  IF cardinality(v_clauses) = 1 AND v_clauses[1] !~ $re$[一-龥]$re$ THEN
    clause_ordinal := 1;
    native_query := public.lexical_query_bounded_question_v1(p_query);
    local_k := NULL;
    preparation_status := 'single_bound';
    IF native_query IS NOT NULL THEN
      RETURN NEXT;
    END IF;
    RETURN;
  END IF;
  FOREACH v_clause IN ARRAY v_clauses LOOP
    v_ordinal := v_ordinal + 1;
    v_intended := NULL;
    IF v_clause ~ $re$[一-龥]$re$ THEN
      v_cjk := regexp_replace(v_clause, $re$[^一-龥]$re$, '', 'g');
      FOR v_i IN 1..GREATEST(char_length(v_cjk) - 1, 0) LOOP
        v_gram := substr(v_cjk, v_i, 2);
        IF char_length(v_gram) = 2 THEN
          v_part := quote_literal(v_gram)::tsquery;
          v_intended := CASE WHEN v_intended IS NULL THEN v_part ELSE v_intended || v_part END;
        END IF;
      END LOOP;
      IF v_intended IS NULL THEN
        CONTINUE;
      END IF;
      clause_ordinal := v_ordinal;
      native_query := v_intended;
      local_k := 4;
      preparation_status := 'han_bigram';
      RETURN NEXT;
    ELSE
      IF v_clause ~ '(^|[^[:alnum:]_])-[[:space:]]*[^[:space:]]' THEN
        native_query := public.lexical_query_bounded_question_v1(v_clause);
        IF native_query IS NULL THEN
          CONTINUE;
        END IF;
        clause_ordinal := v_ordinal;
        local_k := 8;
        preparation_status := 'fallback_bound';
        RETURN NEXT;
        CONTINUE;
      END IF;
      FOREACH v_lexeme IN ARRAY tsvector_to_array(to_tsvector('simple', v_clause)) LOOP
        IF v_lexeme = ANY (v_stop) THEN
          CONTINUE;
        END IF;
        v_part := quote_literal(v_lexeme)::tsquery;
        v_intended := CASE WHEN v_intended IS NULL THEN v_part ELSE v_intended || v_part END;
      END LOOP;
      IF v_intended IS NULL THEN
        CONTINUE;
      END IF;
      clause_ordinal := v_ordinal;
      native_query := v_intended;
      local_k := 8;
      preparation_status := 'clause_disjunctive';
      RETURN NEXT;
    END IF;
  END LOOP;
END
$fn$;

COMMIT;
