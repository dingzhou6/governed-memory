BEGIN;

-- Keep the two-argument aggregate diagnostic available to existing callers.
-- Search admission uses the same applicable current population as retrieval.
CREATE FUNCTION semantic_readiness(
  p_tenant_id text, p_credential_id text, p_scope_subject_ids text[]
)
RETURNS TABLE (generation_id text, model_version text, input_recipe_version text,
               dimensions integer, semantic_status text, diagnostic text)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_principal_id text; v_app_id text; v_generation_id text;
BEGIN
  PERFORM 1 FROM public.tenant_authority WHERE tenant_id=p_tenant_id FOR SHARE;
  IF nullif(current_setting('app.operation',true),'') <> 'search'
     OR NOT FOUND OR public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable semantic authority' USING ERRCODE='42501';
  END IF;
  SELECT c.principal_id,c.app_id INTO v_principal_id,v_app_id
  FROM public.credentials c
  JOIN public.principals p ON p.tenant_id=c.tenant_id AND p.id=c.principal_id
  JOIN public.apps a ON a.tenant_id=c.tenant_id AND a.id=c.app_id
  WHERE c.tenant_id=p_tenant_id AND c.id=p_credential_id
    AND c.token_digest=decode(current_setting('app.credential_digest',true),'hex')
    AND c.credential_class='agent_reader' AND 'search'=ANY(c.allowed_operations)
    AND c.allowed_operations <@ ARRAY['list','search','read']::text[]
    AND c.issued_at<=clock_timestamp() AND c.expires_at>clock_timestamp()
    AND c.revoked_at IS NULL AND p.active AND a.active;
  IF NOT FOUND THEN RAISE EXCEPTION 'unavailable semantic authority' USING ERRCODE='42501'; END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       cardinality(p_scope_subject_ids) NOT BETWEEN 1 AND 8
       OR cardinality(p_scope_subject_ids) <>
          (SELECT count(DISTINCT value) FROM unnest(p_scope_subject_ids) AS value)
       OR EXISTS (SELECT 1 FROM unnest(p_scope_subject_ids) AS value
                  WHERE value !~ '^[A-Za-z0-9_-]{1,64}$')
     ) THEN RAISE EXCEPTION 'invalid semantic scope' USING ERRCODE='22023'; END IF;
  SELECT a.generation_id INTO v_generation_id FROM public.active_embedding_generations a
  WHERE a.tenant_id=p_tenant_id AND a.app_id=v_app_id;
  IF NOT FOUND THEN RETURN QUERY SELECT NULL::text,NULL::text,NULL::text,NULL::integer,'pending'::text,'semantic_configuration_missing'::text; RETURN; END IF;

  RETURN QUERY
  WITH eligible AS MATERIALIZED (
    SELECT i.tenant_id,i.id AS item_id,r.id AS revision_id
    FROM public.items i
    JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    JOIN public.revisions r ON r.tenant_id=i.tenant_id AND r.item_id=i.id AND r.id=i.active_revision_id
    WHERE i.tenant_id=p_tenant_id AND i.deleted_at IS NULL
      AND c.app_id=v_app_id AND c.withdrawn_at IS NULL
      AND (r.valid_from IS NULL OR r.valid_from<=clock_timestamp())
      AND (r.valid_until IS NULL OR clock_timestamp()<r.valid_until)
      AND ((c.audience_kind='private' AND c.owner_principal_id=v_principal_id)
        OR (c.audience_kind='restricted' AND EXISTS (
          SELECT 1 FROM public.collection_grants g WHERE g.tenant_id=c.tenant_id
            AND g.collection_id=c.id AND g.principal_id=v_principal_id AND g.can_read)))
      AND NOT EXISTS (
        SELECT s.kind FROM public.revision_subjects rs
        JOIN public.subjects s ON s.tenant_id=rs.tenant_id AND s.id=rs.subject_id
        WHERE rs.tenant_id=r.tenant_id AND rs.item_id=r.item_id AND rs.revision_id=r.id
        GROUP BY s.kind HAVING NOT bool_or(
          (s.app_id=v_app_id AND s.kind='principal' AND s.principal_id=v_principal_id)
          OR (s.kind='app' AND s.app_id=v_app_id)
          OR (s.app_id=v_app_id AND s.kind IN ('customer','project')
            AND p_scope_subject_ids IS NOT NULL AND s.id=ANY(p_scope_subject_ids))))
  ), current_jobs AS MATERIALIZED (
    SELECT j.status FROM public.embedding_jobs j
    JOIN eligible i USING (tenant_id,item_id,revision_id)
    WHERE j.generation_id=v_generation_id
      AND (j.passage_id IS NULL OR EXISTS (SELECT 1 FROM public.active_extraction_sets x
        WHERE x.tenant_id=j.tenant_id AND x.item_id=j.item_id AND x.revision_id=j.revision_id
          AND x.source_revision_id=j.source_revision_id AND x.extraction_set_id=j.extraction_set_id))
  ), readiness AS (
    SELECT CASE
      WHEN EXISTS (
        SELECT 1 FROM eligible i JOIN public.source_revisions s USING (tenant_id,item_id,revision_id)
        WHERE NOT EXISTS (SELECT 1 FROM public.active_extraction_sets x
          WHERE x.tenant_id=i.tenant_id AND x.item_id=i.item_id AND x.revision_id=i.revision_id)
      ) THEN 'extraction_not_ready'
      WHEN EXISTS (SELECT 1 FROM current_jobs j WHERE j.status IN ('failed','cancelled'))
        THEN 'semantic_processing_failed'
      WHEN EXISTS (SELECT 1 FROM current_jobs j WHERE j.status IN ('pending','leased','retry'))
        THEN 'semantic_processing_pending'
      WHEN EXISTS (
        SELECT 1 FROM eligible i JOIN public.lexical_representations l USING (tenant_id,item_id,revision_id)
        WHERE NOT EXISTS (SELECT 1 FROM public.embedding_representations e
          WHERE e.tenant_id=l.tenant_id AND e.item_id=l.item_id AND e.revision_id=l.revision_id
            AND e.source_revision_id IS NULL AND e.extraction_set_id IS NULL
            AND e.passage_id IS NULL AND e.generation_id=v_generation_id)
      ) OR EXISTS (
        SELECT 1 FROM eligible i JOIN public.active_extraction_sets x USING (tenant_id,item_id,revision_id)
        JOIN public.source_passages p
          ON p.tenant_id=x.tenant_id AND p.item_id=x.item_id AND p.revision_id=x.revision_id
         AND p.source_revision_id=x.source_revision_id AND p.extraction_set_id=x.extraction_set_id
        WHERE NOT EXISTS (SELECT 1 FROM public.embedding_representations e
          WHERE e.tenant_id=p.tenant_id AND e.item_id=p.item_id AND e.revision_id=p.revision_id
            AND e.source_revision_id=p.source_revision_id AND e.extraction_set_id=p.extraction_set_id
            AND e.passage_id=p.id AND e.generation_id=v_generation_id)
      ) THEN 'semantic_processing_pending'
      ELSE 'semantic_ready' END AS reason
  )
  SELECT g.id,g.model_version,g.input_recipe_version,g.dimensions,
    CASE r.reason WHEN 'semantic_ready' THEN 'ready'
      WHEN 'semantic_processing_failed' THEN 'failed' ELSE 'pending' END,r.reason
  FROM public.embedding_generations g CROSS JOIN readiness r
  WHERE g.tenant_id=p_tenant_id AND g.id=v_generation_id;
END
$$;

REVOKE ALL ON FUNCTION semantic_readiness(text,text,text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION semantic_readiness(text,text,text[]) TO agentic_memory_runtime;
COMMIT;
