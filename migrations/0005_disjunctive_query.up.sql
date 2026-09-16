BEGIN;

-- Opt-in preparation only. The v1 keyword function and search ranking are unchanged.
CREATE FUNCTION lexical_query_disjunctive_v2(p_query text)
RETURNS TABLE (prepared_query text, preparation_status text)
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path = pg_catalog, public AS $$
DECLARE
  v_match text[]; v_piece text; v_lexeme text; v_inner text;
  v_terms text[] := ARRAY[]::text[]; v_intended tsquery; v_part tsquery;
BEGIN
  prepared_query := p_query;
  preparation_status := 'fallback_bound';
  IF octet_length(p_query) NOT BETWEEN 1 AND 4096 THEN RETURN NEXT; RETURN; END IF;
  -- Delegate explicit websearch syntax before any token/curly-quote handling.
  IF p_query ~ '"'
     OR p_query ~* '(^|[^[:alnum:]_])OR([^[:alnum:]_]|$)'
     OR p_query ~ '(^|[^[:alnum:]_])-[[:space:]]*[^[:space:]]' THEN
    preparation_status := 'explicit_syntax'; RETURN NEXT; RETURN;
  END IF;
  FOR v_match IN SELECT regexp_matches(p_query, '“[^“”]*”|‘[^‘’]*’|[^“”‘’]+|[“”‘’]', 'g') LOOP
    v_piece := v_match[1];
    IF left(v_piece,1) IN ('“','‘') THEN
      IF (left(v_piece,1)='“' AND right(v_piece,1)<>'”')
         OR (left(v_piece,1)='‘' AND right(v_piece,1)<>'’') THEN
        preparation_status := 'fallback_curly'; RETURN NEXT; RETURN;
      END IF;
      v_inner := substr(v_piece,2,char_length(v_piece)-2);
      IF btrim(v_inner)='' OR v_inner ~ '[“”‘’]' THEN
        preparation_status := 'fallback_curly'; RETURN NEXT; RETURN;
      END IF;
      v_part := phraseto_tsquery('simple',v_inner);
      IF numnode(v_part)>0 THEN
        v_terms := array_append(v_terms,'"'||v_inner||'"');
        v_intended := CASE WHEN v_intended IS NULL THEN v_part ELSE v_intended || v_part END;
      END IF;
    ELSIF v_piece ~ '[“”‘’]' THEN
      preparation_status := 'fallback_curly'; RETURN NEXT; RETURN;
    ELSE
      FOREACH v_lexeme IN ARRAY tsvector_to_array(to_tsvector('simple',v_piece)) LOOP
        v_part := quote_literal(v_lexeme)::tsquery;
        v_terms := array_append(v_terms,'"'||v_lexeme||'"');
        v_intended := CASE WHEN v_intended IS NULL THEN v_part ELSE v_intended || v_part END;
      END LOOP;
    END IF;
  END LOOP;
  IF v_intended IS NULL THEN preparation_status := 'fallback_empty'; RETURN NEXT; RETURN; END IF;
  IF octet_length(array_to_string(v_terms,' OR '))>4096 THEN RETURN NEXT; RETURN; END IF;
  -- Quoted serialization must survive the existing v1 parser as exactly this predicate.
  IF public.lexical_query_bounded_question_v1(array_to_string(v_terms,' OR ')) <> v_intended THEN
    preparation_status := 'fallback_roundtrip'; RETURN NEXT; RETURN;
  END IF;
  prepared_query := array_to_string(v_terms,' OR ');
  preparation_status := 'disjunctive'; RETURN NEXT;
EXCEPTION WHEN syntax_error OR invalid_text_representation THEN
  prepared_query := p_query; preparation_status := 'fallback_roundtrip'; RETURN NEXT;
END
$$;

CREATE FUNCTION search_current_memories_disjunctive_v2(
  p_tenant_id text, p_credential_id text, p_request_id text, p_query text,
  p_max_context_bytes integer, p_scope_subject_ids text[]
)
RETURNS TABLE (item_id text, revision_id text, content text, recorded_at text,
               valid_from text, valid_until text, validity_status text,
               hit_kind text, source_revision_id text, extraction_set_id text,
               passage_id text, locator text, parent_passage_ids text[], candidate_omitted boolean)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_prepared record;
BEGIN
  SELECT * INTO v_prepared FROM public.lexical_query_disjunctive_v2(p_query);
  PERFORM set_config('app.lexical_preparation_status',coalesce(v_prepared.preparation_status,'fallback_bound'),true);
  RETURN QUERY SELECT * FROM public.search_current_memories(
    p_tenant_id,p_credential_id,p_request_id,v_prepared.prepared_query,p_max_context_bytes,p_scope_subject_ids);
END
$$;
REVOKE ALL ON FUNCTION lexical_query_disjunctive_v2(text), search_current_memories_disjunctive_v2(text,text,text,text,integer,text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION search_current_memories_disjunctive_v2(text,text,text,text,integer,text[]) TO agentic_memory_runtime;
COMMIT;
