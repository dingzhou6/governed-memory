BEGIN;

-- Han bigrams are computed at ingest. Existing source_passages.search_document rows are not backfilled.

CREATE FUNCTION han_bigram_document_v1(p_content text)
RETURNS tsvector
LANGUAGE sql
IMMUTABLE
SET search_path = pg_catalog, public
AS $fn$
  SELECT COALESCE((
    SELECT to_tsvector('simple', string_agg(gram, ' ' ORDER BY gram))
    FROM (
      SELECT DISTINCT substr(chars, i, 2) AS gram
      FROM (
        SELECT regexp_replace(COALESCE(p_content, ''), $re$[^一-龥]$re$, '', 'g') AS chars
      ) s
      CROSS JOIN LATERAL generate_series(1, GREATEST(char_length(s.chars) - 1, 0)) AS i
      WHERE char_length(substr(s.chars, i, 2)) = 2
    ) g
  ), ''::tsvector);
$fn$;

CREATE OR REPLACE FUNCTION source_search_document_v1(p_content text, p_locator jsonb)
RETURNS tsvector
LANGUAGE plpgsql
IMMUTABLE
STRICT
SET search_path = pg_catalog, public
AS $fn$
DECLARE
  v_fields jsonb := jsonb_build_array(p_content);
  v_header text;
  v_header_bytes integer := 0;
BEGIN
  IF p_locator ? 'title' THEN
    IF jsonb_typeof(p_locator->'title') <> 'string'
       OR octet_length(p_locator->>'title') NOT BETWEEN 1 AND 256
       OR p_locator->>'title' !~ '[^[:space:]]' THEN
      RAISE EXCEPTION 'invalid source title' USING ERRCODE = '22023';
    END IF;
    v_fields := v_fields || jsonb_build_array(p_locator->>'title');
  END IF;
  IF p_locator ? 'heading' THEN
    IF jsonb_typeof(p_locator->'heading') <> 'string'
       OR octet_length(p_locator->>'heading') NOT BETWEEN 1 AND 512
       OR p_locator->>'heading' !~ '[^[:space:]]' THEN
      RAISE EXCEPTION 'invalid source heading' USING ERRCODE = '22023';
    END IF;
    v_fields := v_fields || jsonb_build_array(p_locator->>'heading');
  END IF;
  IF p_locator ? 'table_headers' THEN
    IF jsonb_typeof(p_locator->'table_headers') <> 'array'
       OR jsonb_array_length(p_locator->'table_headers') NOT BETWEEN 1 AND 32
       OR EXISTS (
         SELECT 1 FROM jsonb_array_elements(p_locator->'table_headers') header
         WHERE jsonb_typeof(header) <> 'string'
            OR octet_length(header#>>'{}') NOT BETWEEN 1 AND 256
            OR header#>>'{}' !~ '[^[:space:]]'
       ) THEN
      RAISE EXCEPTION 'invalid source table headers' USING ERRCODE = '22023';
    END IF;
    FOR v_header IN
      SELECT header#>>'{}'
      FROM jsonb_array_elements(p_locator->'table_headers') WITH ORDINALITY values_(header, ordinal)
      ORDER BY ordinal
    LOOP
      v_header_bytes := v_header_bytes + octet_length(v_header);
      IF v_header_bytes > 2048 THEN
        RAISE EXCEPTION 'source table headers budget exceeded' USING ERRCODE = '22023';
      END IF;
      v_fields := v_fields || jsonb_build_array(v_header);
    END LOOP;
  END IF;
  RETURN to_tsvector('simple', v_fields)
    || public.han_bigram_document_v1(
         COALESCE((SELECT string_agg(value, ' ') FROM jsonb_array_elements_text(v_fields)), '')
       );
END
$fn$;

CREATE FUNCTION lexical_clause_queries_v1(p_query text)
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
    FROM unnest(regexp_split_to_array(p_query, $re$[\n\r。！？!?;；]+|\.[[:space:]]+$re$)) AS piece
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

CREATE FUNCTION search_current_memories_clause_union_v1(
  p_tenant_id text,
  p_credential_id text,
  p_request_id text,
  p_query text,
  p_max_context_bytes integer,
  p_scope_subject_ids text[]
)
RETURNS TABLE (item_id text, revision_id text, content text, recorded_at text,
               valid_from text, valid_until text, validity_status text,
               hit_kind text, source_revision_id text, extraction_set_id text,
               passage_id text, locator text, parent_passage_ids text[],
               candidate_omitted boolean)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $fn$
DECLARE
  v_principal_id text;
  v_app_id text;
  v_authority_epoch bigint;
BEGIN
  IF (
    p_tenant_id IS NOT NULL
    AND p_credential_id IS NOT NULL
    AND p_request_id IS NOT NULL
    AND p_request_id ~ '^[0-9a-f-]{36}$'
    AND p_query IS NOT NULL
    AND octet_length(p_query) BETWEEN 1 AND 4096
    AND p_query ~ '[^[:space:]]'
    AND p_max_context_bytes BETWEEN 1 AND 65535
    AND nullif(current_setting('app.operation', true), '') = 'search'
  ) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid search authority context' USING ERRCODE = '42501';
  END IF;
  SELECT t.authority_epoch INTO v_authority_epoch
  FROM public.tenant_authority t
  WHERE t.tenant_id = p_tenant_id
  FOR SHARE;
  IF NOT FOUND OR public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable search authority context' USING ERRCODE = '42501';
  END IF;
  SELECT c.principal_id, c.app_id INTO v_principal_id, v_app_id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id = c.tenant_id AND p.id = c.principal_id
  JOIN public.apps a ON a.tenant_id = c.tenant_id AND a.id = c.app_id
  WHERE c.tenant_id = p_tenant_id
    AND c.id = p_credential_id
    AND c.token_digest = decode(current_setting('app.credential_digest', true), 'hex')
    AND c.credential_class = 'agent_reader'
    AND 'search' = ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['list', 'search', 'read']::text[]
    AND c.issued_at <= clock_timestamp()
    AND c.expires_at > clock_timestamp()
    AND c.revoked_at IS NULL
    AND p.active AND a.active;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'invalid search credential' USING ERRCODE = '42501';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       cardinality(p_scope_subject_ids) NOT BETWEEN 1 AND 8
       OR cardinality(p_scope_subject_ids) <>
          (SELECT count(DISTINCT subject_id) FROM unnest(p_scope_subject_ids) AS subject_id)
       OR EXISTS (
         SELECT 1 FROM unnest(p_scope_subject_ids) AS subject_id
         WHERE subject_id !~ '^[A-Za-z0-9_-]{1,64}$'
       )
     ) THEN
    RAISE EXCEPTION 'invalid search scope' USING ERRCODE = '22023';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       SELECT count(*) FROM public.subjects s
       WHERE s.tenant_id = p_tenant_id AND s.app_id = v_app_id
         AND s.kind IN ('customer', 'project')
         AND s.id = ANY(p_scope_subject_ids)
     ) <> cardinality(p_scope_subject_ids) THEN
    RAISE EXCEPTION 'invalid search scope' USING ERRCODE = '22023';
  END IF;

  RETURN QUERY
  WITH query_plan AS (
    SELECT c.clause_ordinal, c.native_query AS query, c.local_k
    FROM public.lexical_clause_queries_v1(p_query) c
  ),
  authorized_revisions AS (
    SELECT i.tenant_id, i.id AS item_id, r.id AS revision_id, r.content,
           to_char(r.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') AS recorded_at,
           CASE WHEN r.valid_from IS NULL THEN NULL ELSE
             to_char(r.valid_from AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END AS valid_from,
           CASE WHEN r.valid_until IS NULL THEN NULL ELSE
             to_char(r.valid_until AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END AS valid_until,
           CASE WHEN r.valid_from IS NULL AND r.valid_until IS NULL THEN 'unknown' ELSE 'known' END AS validity_status,
           l.document
    FROM public.items i
    JOIN public.collections c ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
    JOIN public.revisions r
      ON r.tenant_id = i.tenant_id AND r.item_id = i.id AND r.id = i.active_revision_id
    LEFT JOIN public.lexical_representations l
      ON l.tenant_id = r.tenant_id AND l.item_id = r.item_id AND l.revision_id = r.id
    WHERE i.tenant_id = p_tenant_id AND c.app_id = v_app_id
      AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL
      AND (r.valid_from IS NULL OR r.valid_from <= clock_timestamp())
      AND (r.valid_until IS NULL OR clock_timestamp() < r.valid_until)
      AND (
        (c.audience_kind = 'private' AND c.owner_principal_id = v_principal_id)
        OR (
          c.audience_kind = 'restricted'
          AND EXISTS (
            SELECT 1 FROM public.collection_grants g
            WHERE g.tenant_id = c.tenant_id AND g.collection_id = c.id
              AND g.principal_id = v_principal_id AND g.can_read
          )
        )
      )
      AND NOT EXISTS (
        SELECT s.kind
        FROM public.revision_subjects rs
        JOIN public.subjects s ON s.tenant_id = rs.tenant_id AND s.id = rs.subject_id
        WHERE rs.tenant_id = r.tenant_id AND rs.item_id = r.item_id
          AND rs.revision_id = r.id
        GROUP BY s.kind
        HAVING NOT bool_or(
          (s.app_id = v_app_id AND s.kind = 'principal' AND s.principal_id = v_principal_id)
          OR (s.kind = 'app' AND s.app_id = v_app_id)
          OR (s.app_id = v_app_id AND s.kind IN ('customer', 'project')
              AND p_scope_subject_ids IS NOT NULL AND s.id = ANY(p_scope_subject_ids))
        )
      )
  ),
  complete_active_sets AS (
    SELECT active.tenant_id, active.item_id, active.revision_id,
           active.source_revision_id, active.extraction_set_id
    FROM authorized_revisions authorized
    JOIN public.active_extraction_sets active
      ON active.tenant_id = authorized.tenant_id
     AND active.item_id = authorized.item_id
     AND active.revision_id = authorized.revision_id
    JOIN public.extraction_sets e
      ON e.tenant_id = active.tenant_id AND e.item_id = active.item_id
     AND e.revision_id = active.revision_id
     AND e.source_revision_id = active.source_revision_id
     AND e.id = active.extraction_set_id
    JOIN public.source_passages complete
      ON complete.tenant_id = e.tenant_id AND complete.item_id = e.item_id
     AND complete.revision_id = e.revision_id
     AND complete.source_revision_id = e.source_revision_id
     AND complete.extraction_set_id = e.id
    GROUP BY active.tenant_id, active.item_id, active.revision_id,
             active.source_revision_id, active.extraction_set_id,
             e.passage_count, e.content_bytes
    HAVING count(*) = e.passage_count
       AND sum(octet_length(complete.content)) = e.content_bytes
  ),
  memory_hits AS (
    SELECT a.item_id, a.revision_id, a.content, a.recorded_at, a.valid_from,
           a.valid_until, a.validity_status, 'memory'::text AS hit_kind,
           NULL::text AS source_revision_id, NULL::text AS extraction_set_id,
           NULL::text AS passage_id, NULL::text AS locator,
           ts_rank(a.document, query_plan.query) AS relevance,
           NULL::integer AS passage_order, true AS fits_global_budget,
           query_plan.clause_ordinal, query_plan.local_k
    FROM authorized_revisions a
    CROSS JOIN query_plan
    WHERE query_plan.query IS NOT NULL AND a.document @@ query_plan.query
      AND NOT EXISTS (
        SELECT 1 FROM public.active_extraction_sets active
        WHERE active.tenant_id = a.tenant_id AND active.item_id = a.item_id
          AND active.revision_id = a.revision_id
      )
  ),
  memory_limited AS (
    SELECT ranked.*
    FROM (
      SELECT h.*,
             row_number() OVER (
               PARTITION BY h.clause_ordinal
               ORDER BY h.relevance DESC, h.item_id, h.revision_id
             ) AS local_rank
      FROM memory_hits h
    ) ranked
    WHERE ranked.local_k IS NULL OR ranked.local_rank <= ranked.local_k
  ),
  memory_candidates AS (
    SELECT DISTINCT ON (limited.item_id, limited.revision_id)
           limited.item_id, limited.revision_id, limited.content, limited.recorded_at,
           limited.valid_from, limited.valid_until, limited.validity_status, limited.hit_kind,
           limited.source_revision_id, limited.extraction_set_id, limited.passage_id,
           limited.locator, limited.relevance, limited.passage_order, limited.fits_global_budget
    FROM memory_limited limited
    ORDER BY limited.item_id, limited.revision_id, limited.relevance DESC
  ),
  passage_hits AS (
    SELECT a.item_id, a.revision_id, p.content, a.recorded_at, a.valid_from,
           a.valid_until, a.validity_status, 'passage'::text AS hit_kind,
           p.source_revision_id, p.extraction_set_id, p.id AS passage_id,
           p.locator::text,
           ts_rank(p.search_document, query_plan.query) AS relevance,
           p.passage_order,
           octet_length(p.content) <= p_max_context_bytes AS fits_global_budget,
           query_plan.clause_ordinal, query_plan.local_k
    FROM authorized_revisions a
    JOIN complete_active_sets active
      ON active.tenant_id = a.tenant_id AND active.item_id = a.item_id
     AND active.revision_id = a.revision_id
    JOIN public.source_passages p
      ON p.tenant_id = active.tenant_id AND p.item_id = active.item_id
     AND p.revision_id = active.revision_id
     AND p.source_revision_id = active.source_revision_id
     AND p.extraction_set_id = active.extraction_set_id
    CROSS JOIN query_plan
    WHERE query_plan.query IS NOT NULL AND p.search_document @@ query_plan.query
  ),
  passage_limited AS (
    SELECT ranked.*
    FROM (
      SELECT h.*,
             row_number() OVER (
               PARTITION BY h.clause_ordinal
               ORDER BY h.fits_global_budget DESC, h.relevance DESC,
                        h.passage_order, h.passage_id
             ) AS local_rank
      FROM passage_hits h
    ) ranked
    WHERE ranked.local_k IS NULL OR ranked.local_rank <= ranked.local_k
  ),
  passage_spans AS (
    SELECT DISTINCT ON (limited.item_id, limited.revision_id, limited.source_revision_id,
                        limited.extraction_set_id, limited.passage_id)
           limited.item_id, limited.revision_id, limited.content, limited.recorded_at,
           limited.valid_from, limited.valid_until, limited.validity_status, limited.hit_kind,
           limited.source_revision_id, limited.extraction_set_id, limited.passage_id,
           limited.locator, limited.relevance, limited.passage_order, limited.fits_global_budget
    FROM passage_limited limited
    ORDER BY limited.item_id, limited.revision_id, limited.source_revision_id,
             limited.extraction_set_id, limited.passage_id, limited.relevance DESC
  ),
  passage_ranked AS (
    SELECT spans.*,
           count(*) OVER (
             PARTITION BY spans.item_id, spans.revision_id,
                          spans.source_revision_id, spans.extraction_set_id
           ) AS source_match_count,
           row_number() OVER (
             PARTITION BY spans.item_id, spans.revision_id,
                          spans.source_revision_id, spans.extraction_set_id
             ORDER BY spans.fits_global_budget DESC, spans.relevance DESC,
                      spans.passage_order, spans.passage_id
           ) AS source_rank
    FROM passage_spans spans
  ),
  direct_candidates AS (
    SELECT memory.*, false AS source_cap_omitted FROM memory_candidates memory
    UNION ALL
    SELECT passage.item_id, passage.revision_id, passage.content, passage.recorded_at,
           passage.valid_from, passage.valid_until, passage.validity_status,
           passage.hit_kind, passage.source_revision_id, passage.extraction_set_id,
           passage.passage_id, passage.locator, passage.relevance, passage.passage_order,
           passage.fits_global_budget, passage.source_match_count > 16 AS source_cap_omitted
    FROM passage_ranked passage
    WHERE passage.source_rank <= 16
  ),
  direct_ranked AS (
    SELECT candidate.*,
           row_number() OVER (
             ORDER BY candidate.fits_global_budget DESC, candidate.relevance DESC,
                      candidate.item_id, candidate.hit_kind,
                      candidate.passage_order NULLS FIRST,
                      candidate.passage_id NULLS FIRST
           ) AS direct_rank,
           count(*) OVER () AS direct_candidate_count,
           bool_or(candidate.source_cap_omitted) OVER () AS any_source_cap_omitted
    FROM direct_candidates candidate
  ),
  direct_window AS (
    SELECT * FROM direct_ranked WHERE direct_rank <= 41
  ),
  expansion_spans AS (
    SELECT neighbor.item_id, neighbor.revision_id, neighbor.content,
           parent.recorded_at, parent.valid_from, parent.valid_until,
           parent.validity_status, 'adjacent_continuation'::text AS hit_kind,
           neighbor.source_revision_id, neighbor.extraction_set_id,
           neighbor.id AS passage_id, neighbor.locator::text,
           parent.relevance, neighbor.passage_order,
           octet_length(neighbor.content) <= p_max_context_bytes AS fits_global_budget,
           parent.passage_id AS parent_passage_id, parent.direct_rank AS parent_rank
    FROM direct_window parent
    JOIN public.source_passages direct
      ON parent.hit_kind = 'passage'
     AND direct.tenant_id = p_tenant_id AND direct.item_id = parent.item_id
     AND direct.revision_id = parent.revision_id
     AND direct.source_revision_id = parent.source_revision_id
     AND direct.extraction_set_id = parent.extraction_set_id
     AND direct.id = parent.passage_id
    JOIN public.source_passages neighbor
      ON neighbor.tenant_id = direct.tenant_id AND neighbor.item_id = direct.item_id
     AND neighbor.revision_id = direct.revision_id
     AND neighbor.source_revision_id = direct.source_revision_id
     AND neighbor.extraction_set_id = direct.extraction_set_id
     AND neighbor.structural_parent_id = direct.structural_parent_id
     AND (
       (
         neighbor.passage_order = direct.passage_order - 1
         AND direct.continuation_direction IN ('from_previous', 'both')
         AND neighbor.continuation_direction IN ('to_next', 'both')
       ) OR (
         neighbor.passage_order = direct.passage_order + 1
         AND direct.continuation_direction IN ('to_next', 'both')
         AND neighbor.continuation_direction IN ('from_previous', 'both')
       )
     )
    WHERE NOT EXISTS (
      SELECT 1 FROM passage_spans lexical
      WHERE lexical.item_id = neighbor.item_id
        AND lexical.revision_id = neighbor.revision_id
        AND lexical.source_revision_id = neighbor.source_revision_id
        AND lexical.extraction_set_id = neighbor.extraction_set_id
        AND lexical.passage_id = neighbor.id
    )
  ),
  deduplicated_expansions AS (
    SELECT expansion.item_id, expansion.revision_id, expansion.content,
           expansion.recorded_at, expansion.valid_from, expansion.valid_until,
           expansion.validity_status, expansion.hit_kind,
           expansion.source_revision_id, expansion.extraction_set_id,
           expansion.passage_id, expansion.locator, expansion.passage_order,
           expansion.fits_global_budget, min(expansion.parent_rank) AS parent_rank,
           array_agg(expansion.parent_passage_id
                     ORDER BY expansion.parent_rank, expansion.parent_passage_id)
             AS parent_passage_ids
    FROM expansion_spans expansion
    GROUP BY expansion.item_id, expansion.revision_id, expansion.content,
             expansion.recorded_at, expansion.valid_from, expansion.valid_until,
             expansion.validity_status, expansion.hit_kind,
             expansion.source_revision_id, expansion.extraction_set_id,
             expansion.passage_id, expansion.locator, expansion.passage_order,
             expansion.fits_global_budget
  ),
  expansion_window AS (
    SELECT expansion.*,
           row_number() OVER (
             ORDER BY expansion.parent_rank, expansion.passage_order,
                      expansion.passage_id
           ) AS expansion_rank
    FROM deduplicated_expansions expansion
  ),
  omissions AS (
    SELECT coalesce(bool_or(direct.any_source_cap_omitted), false)
             OR coalesce(max(direct.direct_candidate_count), 0) > 41
             OR (SELECT count(*) FROM deduplicated_expansions) > 41 AS omitted
    FROM direct_window direct
  ),
  candidates AS (
    SELECT direct.item_id, direct.revision_id, direct.content, direct.recorded_at,
           direct.valid_from, direct.valid_until, direct.validity_status,
           direct.hit_kind, direct.source_revision_id, direct.extraction_set_id,
           direct.passage_id, direct.locator, NULL::text[] AS parent_passage_ids,
           0 AS phase, direct.direct_rank AS candidate_rank
    FROM direct_window direct
    UNION ALL
    SELECT expansion.item_id, expansion.revision_id, expansion.content,
           expansion.recorded_at, expansion.valid_from, expansion.valid_until,
           expansion.validity_status, expansion.hit_kind,
           expansion.source_revision_id, expansion.extraction_set_id,
           expansion.passage_id, expansion.locator, expansion.parent_passage_ids,
           1 AS phase, expansion.expansion_rank AS candidate_rank
    FROM expansion_window expansion
    WHERE expansion.expansion_rank <= 41
  )
  SELECT candidate.item_id, candidate.revision_id, candidate.content,
         candidate.recorded_at, candidate.valid_from, candidate.valid_until,
         candidate.validity_status, candidate.hit_kind, candidate.source_revision_id,
         candidate.extraction_set_id, candidate.passage_id, candidate.locator,
         candidate.parent_passage_ids, omissions.omitted
  FROM candidates candidate CROSS JOIN omissions
  ORDER BY candidate.phase, candidate.candidate_rank;


  INSERT INTO public.read_audit
    (request_id, tenant_id, principal_id, app_id, credential_id, operation,
     target_id, outcome, authority_epoch)
  VALUES (p_request_id, p_tenant_id, v_principal_id, v_app_id, p_credential_id,
          'search', NULL, 'released', v_authority_epoch);
END
$fn$;

CREATE OR REPLACE FUNCTION search_hybrid_current_memories(
  p_tenant_id text, p_credential_id text, p_request_id text, p_query text,
  p_max_context_bytes integer, p_scope_subject_ids text[],
  p_generation_id text, p_query_embedding text
)
RETURNS TABLE (item_id text, revision_id text, content text, recorded_at text,
               valid_from text, valid_until text, validity_status text,
               hit_kind text, source_revision_id text, extraction_set_id text,
               passage_id text, locator text, parent_passage_ids text[],
               candidate_omitted boolean)
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $fn$
WITH lexical_all AS MATERIALIZED (
  SELECT l.item_id,l.revision_id,l.content,l.recorded_at,l.valid_from,l.valid_until,
         l.validity_status,l.hit_kind,l.source_revision_id,l.extraction_set_id,
         l.passage_id,l.locator,l.parent_passage_ids,l.candidate_omitted,
         l.ordinality AS lexical_rank
  FROM public.search_current_memories_clause_union_v1(
    p_tenant_id,p_credential_id,p_request_id,p_query,
    p_max_context_bytes,p_scope_subject_ids
  ) WITH ORDINALITY l
), lexical_direct AS MATERIALIZED (
  SELECT * FROM lexical_all WHERE hit_kind<>'adjacent_continuation'
), lexical_continuations AS MATERIALIZED (
  SELECT * FROM lexical_all WHERE hit_kind='adjacent_continuation'
), semantic AS MATERIALIZED (
  SELECT s.item_id,s.revision_id,s.source_revision_id,s.extraction_set_id,
         s.passage_id,s.distance,s.ordinality AS semantic_rank
  FROM public.search_exact_semantic_candidates(
    p_tenant_id,p_credential_id,p_generation_id,p_query_embedding,
    p_scope_subject_ids,41
  ) WITH ORDINALITY s
), fused AS (
  SELECT coalesce(l.item_id,s.item_id) AS item_id,
         coalesce(l.revision_id,s.revision_id) AS revision_id,
         coalesce(l.source_revision_id,s.source_revision_id) AS source_revision_id,
         coalesce(l.extraction_set_id,s.extraction_set_id) AS extraction_set_id,
         coalesce(l.passage_id,s.passage_id) AS passage_id,
         l.lexical_rank,s.semantic_rank,l.content,l.recorded_at,l.valid_from,l.valid_until,
         l.validity_status,l.hit_kind,l.locator,l.parent_passage_ids,l.candidate_omitted,
         coalesce(1.0/(60+l.lexical_rank),0)+coalesce(1.0/(60+s.semantic_rank),0) AS rrf
  FROM lexical_direct l FULL JOIN semantic s
    ON s.item_id=l.item_id AND s.revision_id=l.revision_id
   AND s.source_revision_id IS NOT DISTINCT FROM l.source_revision_id
   AND s.extraction_set_id IS NOT DISTINCT FROM l.extraction_set_id
   AND s.passage_id IS NOT DISTINCT FROM l.passage_id
), ranked_direct AS (
  SELECT f.*, row_number() OVER (ORDER BY f.rrf DESC,f.item_id,f.revision_id,
    f.source_revision_id NULLS FIRST,f.extraction_set_id NULLS FIRST,
    f.passage_id NULLS FIRST) AS direct_rank
  FROM fused f
), selected_direct AS MATERIALIZED (
  SELECT * FROM ranked_direct WHERE direct_rank<=41
), combined AS (
  SELECT f.item_id,f.revision_id,
       coalesce(f.content,sp.content,r.content) AS content,
       coalesce(f.recorded_at,to_char(r.recorded_at AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"')) AS recorded_at,
       coalesce(f.valid_from,CASE WHEN r.valid_from IS NULL THEN NULL ELSE
         to_char(r.valid_from AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END) AS valid_from,
       coalesce(f.valid_until,CASE WHEN r.valid_until IS NULL THEN NULL ELSE
         to_char(r.valid_until AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"') END) AS valid_until,
       coalesce(f.validity_status,CASE WHEN r.valid_from IS NULL AND r.valid_until IS NULL
         THEN 'unknown' ELSE 'known' END) AS validity_status,
       CASE WHEN f.lexical_rank IS NOT NULL THEN f.hit_kind
            WHEN f.passage_id IS NULL THEN 'semantic_memory' ELSE 'semantic_passage' END AS hit_kind,
       f.source_revision_id,f.extraction_set_id,f.passage_id,
       coalesce(f.locator,sp.locator::text) AS locator,f.parent_passage_ids,
       f.candidate_omitted,0 AS phase,f.direct_rank AS phase_rank
  FROM selected_direct f
  JOIN public.revisions r
    ON r.tenant_id=p_tenant_id AND r.item_id=f.item_id AND r.id=f.revision_id
  LEFT JOIN public.source_passages sp
    ON sp.tenant_id=p_tenant_id AND sp.item_id=f.item_id AND sp.revision_id=f.revision_id
   AND sp.source_revision_id=f.source_revision_id AND sp.extraction_set_id=f.extraction_set_id
   AND sp.id=f.passage_id
  UNION ALL
  SELECT c.item_id,c.revision_id,c.content,c.recorded_at,c.valid_from,c.valid_until,
         c.validity_status,c.hit_kind,c.source_revision_id,c.extraction_set_id,
         c.passage_id,c.locator,c.parent_passage_ids,c.candidate_omitted,
         1 AS phase,c.lexical_rank AS phase_rank
  FROM lexical_continuations c
  WHERE EXISTS (
    SELECT 1 FROM selected_direct d
    WHERE d.item_id=c.item_id AND d.revision_id=c.revision_id
      AND d.source_revision_id=c.source_revision_id
      AND d.extraction_set_id=c.extraction_set_id
      AND d.passage_id=ANY(c.parent_passage_ids))
    AND NOT EXISTS (
      SELECT 1 FROM selected_direct d
      WHERE d.item_id=c.item_id AND d.revision_id=c.revision_id
        AND d.source_revision_id IS NOT DISTINCT FROM c.source_revision_id
        AND d.extraction_set_id IS NOT DISTINCT FROM c.extraction_set_id
        AND d.passage_id IS NOT DISTINCT FROM c.passage_id)
), final_ranked AS (
  SELECT c.*,row_number() OVER (ORDER BY c.phase,c.phase_rank,c.item_id,c.revision_id,
    c.source_revision_id NULLS FIRST,c.extraction_set_id NULLS FIRST,
    c.passage_id NULLS FIRST) AS final_rank,
    count(*) OVER ()>41 OR coalesce(bool_or(c.candidate_omitted) OVER (),false) AS omitted
  FROM combined c
)
SELECT item_id,revision_id,content,recorded_at,valid_from,valid_until,validity_status,
       hit_kind,source_revision_id,extraction_set_id,passage_id,locator,
       parent_passage_ids,omitted
FROM final_ranked WHERE final_rank<=41 ORDER BY final_rank
$fn$;

REVOKE ALL ON FUNCTION han_bigram_document_v1(text), lexical_clause_queries_v1(text),
  search_current_memories_clause_union_v1(text,text,text,text,integer,text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION search_current_memories_clause_union_v1(text,text,text,text,integer,text[])
  TO agentic_memory_runtime;

COMMIT;
