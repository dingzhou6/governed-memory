BEGIN;

-- Slice I: keep 0015 English emit. For queries containing Han (一-龥), also:
-- defer non-neighbor extras of sources with 2+ lex heads; defer extra_seq>=2 of
-- headed sources that missed the neighbor band; unheaded remainder emits
-- extra_seq=1 across sources before extra_seq=2 (round-robin). Packs a later
-- unheaded required row (zh11-p01 analog) without raising Han local_k, 12/4/2160,
-- or stealing EN 4-row unheaded bases. Singleton k=4 heads stay.

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
  SELECT r.*, min(r.direct_rank) OVER (
           PARTITION BY r.item_id, r.revision_id, r.source_revision_id, r.extraction_set_id
         ) AS source_first
  FROM ranked_direct r
  WHERE r.direct_rank<=41
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
       f.candidate_omitted,0 AS phase,f.direct_rank AS phase_rank,f.source_first,f.lexical_rank
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
         1 AS phase,c.lexical_rank AS phase_rank,c.lexical_rank AS source_first,c.lexical_rank
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
), combined_marked AS (
  SELECT c.*,
    (c.phase=0 AND c.lexical_rank IS NOT NULL AND c.lexical_rank<=4
      AND octet_length(c.content)<=2048) AS is_lex_head,
    (c.phase=0 AND octet_length(c.content)>2048) AS is_huge,
    (p_query ~ '[一-龥]') AS is_han_query
  FROM combined c
), combined_keyed AS (
  SELECT m.*,
    coalesce(bool_or(m.is_lex_head) OVER (
      PARTITION BY m.item_id, m.revision_id, m.source_revision_id, m.extraction_set_id
    ), false) AS source_has_head,
    count(*) FILTER (WHERE m.is_lex_head) OVER (
      PARTITION BY m.item_id, m.revision_id, m.source_revision_id, m.extraction_set_id
    ) AS source_head_count,
    CASE WHEN m.is_lex_head OR m.is_huge OR m.phase<>0 THEN NULL
    ELSE row_number() OVER (
      PARTITION BY m.item_id, m.revision_id, m.source_revision_id, m.extraction_set_id,
        (NOT m.is_lex_head AND NOT m.is_huge AND m.phase=0)
      ORDER BY m.phase_rank, m.item_id, m.revision_id,
        m.source_revision_id NULLS FIRST, m.extraction_set_id NULLS FIRST,
        m.passage_id NULLS FIRST
    ) END AS extra_seq
  FROM combined_marked m
), combined_neighbored AS (
  SELECT k.*,
    coalesce(bool_or(k.extra_seq=1 AND k.lexical_rank<=16) OVER (
      PARTITION BY k.item_id, k.revision_id, k.source_revision_id, k.extraction_set_id
    ), false) AS source_has_neighbor
  FROM combined_keyed k
), final_ranked AS (
  SELECT k.*,row_number() OVER (ORDER BY k.phase,
    CASE WHEN k.is_huge THEN 4
         WHEN k.is_lex_head THEN 0
         WHEN k.source_has_head AND k.extra_seq=1 AND k.lexical_rank<=16 THEN 1
         WHEN k.is_han_query AND k.source_head_count>=2 AND k.extra_seq IS NOT NULL
              AND NOT (k.extra_seq=1 AND k.lexical_rank IS NOT NULL AND k.lexical_rank<=16) THEN 3
         WHEN k.is_han_query AND k.source_has_head AND NOT k.source_has_neighbor
              AND k.extra_seq>=2 THEN 3
         WHEN k.source_has_head AND k.source_has_neighbor AND k.extra_seq>=3 THEN 3
         ELSE 2 END,
    CASE WHEN k.is_han_query AND NOT k.source_has_head AND k.extra_seq IS NOT NULL
         THEN k.extra_seq ELSE 0 END,
    CASE WHEN k.phase=0 THEN k.source_first ELSE k.phase_rank END,
    k.phase_rank,k.item_id,k.revision_id,
    k.source_revision_id NULLS FIRST,k.extraction_set_id NULLS FIRST,
    k.passage_id NULLS FIRST) AS final_rank,
    count(*) OVER ()>41 OR coalesce(bool_or(k.candidate_omitted) OVER (),false) AS omitted
  FROM combined_neighbored k
)
SELECT item_id,revision_id,content,recorded_at,valid_from,valid_until,validity_status,
       hit_kind,source_revision_id,extraction_set_id,passage_id,locator,
       parent_passage_ids,omitted
FROM final_ranked WHERE final_rank<=41 ORDER BY final_rank
$fn$;

COMMIT;
