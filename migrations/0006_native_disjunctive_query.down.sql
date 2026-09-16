BEGIN;
CREATE OR REPLACE FUNCTION search_current_memories(
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
AS $$
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
    AND p_max_context_bytes BETWEEN 1 AND 4096
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
    SELECT public.lexical_query_bounded_question_v1(p_query) AS query
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
  memory_candidates AS (
    SELECT a.item_id, a.revision_id, a.content, a.recorded_at, a.valid_from,
           a.valid_until, a.validity_status, 'memory'::text AS hit_kind,
           NULL::text AS source_revision_id, NULL::text AS extraction_set_id,
           NULL::text AS passage_id, NULL::text AS locator,
           ts_rank(a.document, query_plan.query) AS relevance,
           NULL::integer AS passage_order, true AS fits_global_budget
    FROM authorized_revisions a
    CROSS JOIN query_plan
    WHERE query_plan.query IS NOT NULL AND a.document @@ query_plan.query
      AND NOT EXISTS (
        SELECT 1 FROM public.active_extraction_sets active
        WHERE active.tenant_id = a.tenant_id AND active.item_id = a.item_id
          AND active.revision_id = a.revision_id
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
  passage_spans AS (
    SELECT a.item_id, a.revision_id, p.content, a.recorded_at, a.valid_from,
           a.valid_until, a.validity_status, 'passage'::text AS hit_kind,
           p.source_revision_id, p.extraction_set_id, p.id AS passage_id,
           p.locator::text,
           ts_rank(p.search_document, query_plan.query) AS relevance,
           p.passage_order,
           octet_length(p.content) <= p_max_context_bytes AS fits_global_budget
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
$$;
DROP FUNCTION search_current_memories_disjunctive_v3(text,text,text,text,integer,text[]);
DROP FUNCTION lexical_query_disjunctive_v3(text);
DROP FUNCTION search_current_memories_query_core(text,text,text,text,integer,text[],tsquery);
COMMIT;
