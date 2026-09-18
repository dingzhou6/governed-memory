BEGIN;

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
  FROM public.search_current_memories(
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
  RETURN to_tsvector('simple', v_fields);
END
$fn$;

DROP FUNCTION IF EXISTS search_current_memories_clause_union_v1(text,text,text,text,integer,text[]);
DROP FUNCTION IF EXISTS lexical_clause_queries_v1(text);
DROP FUNCTION IF EXISTS han_bigram_document_v1(text);

COMMIT;
