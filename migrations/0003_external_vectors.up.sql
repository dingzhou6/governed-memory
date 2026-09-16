BEGIN;

-- Native JSON admission is deliberately capped below pgvector 0.8.6's 16,000
-- untyped-vector storage limit. This path performs exact search and creates no ANN index.
ALTER TABLE embedding_generations DROP CONSTRAINT embedding_generations_dimensions_check;
ALTER TABLE embedding_jobs DROP CONSTRAINT embedding_jobs_dimensions_check;
ALTER TABLE embedding_representations DROP CONSTRAINT embedding_representations_dimensions_check;
ALTER TABLE embedding_representations
  ALTER COLUMN embedding TYPE vector USING embedding::vector;
ALTER TABLE embedding_generations
  ADD CONSTRAINT embedding_generations_dimensions_check CHECK (dimensions BETWEEN 1 AND 4096);
ALTER TABLE embedding_jobs
  ADD CONSTRAINT embedding_jobs_dimensions_check CHECK (dimensions BETWEEN 1 AND 4096);
ALTER TABLE embedding_representations
  ADD CONSTRAINT embedding_representations_dimensions_check CHECK (dimensions BETWEEN 1 AND 4096),
  ADD CONSTRAINT embedding_representations_nonzero_check CHECK (vector_norm(embedding) > 0),
  ADD CONSTRAINT embedding_representations_cosine_safe_check CHECK (
    (embedding <=> embedding) IS NOT NULL
    AND (embedding <=> embedding)::text NOT IN ('NaN','Infinity','-Infinity')
    AND (embedding <=> embedding) = 0
  );

CREATE OR REPLACE FUNCTION reject_embedding_job_identity_mutation()
RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
     OR NEW.id IS DISTINCT FROM OLD.id
     OR NEW.item_id IS DISTINCT FROM OLD.item_id
     OR NEW.revision_id IS DISTINCT FROM OLD.revision_id
     OR NEW.source_revision_id IS DISTINCT FROM OLD.source_revision_id
     OR NEW.extraction_set_id IS DISTINCT FROM OLD.extraction_set_id
     OR NEW.passage_id IS DISTINCT FROM OLD.passage_id
     OR NEW.generation_id IS DISTINCT FROM OLD.generation_id
     OR NEW.model_version IS DISTINCT FROM OLD.model_version
     OR NEW.input_recipe_version IS DISTINCT FROM OLD.input_recipe_version
     OR NEW.dimensions IS DISTINCT FROM OLD.dimensions
     OR NEW.input_digest IS DISTINCT FROM OLD.input_digest
     OR NEW.deletion_generation IS DISTINCT FROM OLD.deletion_generation THEN
    RAISE EXCEPTION 'embedding job identity is immutable' USING ERRCODE='55000';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER embedding_job_identity_is_immutable
BEFORE UPDATE ON embedding_jobs
FOR EACH ROW EXECUTE FUNCTION reject_embedding_job_identity_mutation();

REVOKE ALL ON FUNCTION reject_embedding_job_identity_mutation() FROM PUBLIC;

CREATE OR REPLACE FUNCTION complete_embedding_job(
  p_tenant_id text, p_job_id text, p_attempt bigint, p_embedding text
)
RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_job public.embedding_jobs%ROWTYPE; v_current boolean; v_embedding vector;
        v_self_distance double precision;
BEGIN
  BEGIN
    v_embedding := p_embedding::vector;
  EXCEPTION WHEN OTHERS THEN
    RAISE EXCEPTION 'invalid embedding response' USING ERRCODE='22023';
  END;
  IF p_embedding IS NULL OR p_embedding ~ 'NaN|Infinity'
     OR vector_dims(v_embedding) NOT BETWEEN 1 AND 4096
     OR vector_norm(v_embedding) <= 0 THEN
    RAISE EXCEPTION 'invalid embedding response' USING ERRCODE='22023';
  END IF;
  v_self_distance := v_embedding <=> v_embedding;
  IF v_self_distance IS NULL
     OR v_self_distance::text IN ('NaN','Infinity','-Infinity')
     OR v_self_distance <> 0 THEN
    RAISE EXCEPTION 'invalid embedding response' USING ERRCODE='22023';
  END IF;
  PERFORM 1 FROM public.tenant_authority WHERE tenant_id=p_tenant_id FOR UPDATE;
  IF NOT FOUND THEN RAISE EXCEPTION 'stale embedding tenant' USING ERRCODE='P0003'; END IF;
  SELECT * INTO v_job FROM public.embedding_jobs
  WHERE tenant_id=p_tenant_id AND id=p_job_id FOR UPDATE;
  IF NOT FOUND OR v_job.status <> 'leased' OR (v_job.attempt=p_attempt) IS NOT TRUE
     OR v_job.lease_expires_at <= clock_timestamp()
     OR v_job.deadline_at <= clock_timestamp() THEN
    IF FOUND AND v_job.deadline_at<=clock_timestamp()
       AND v_job.status IN ('pending','retry','leased') THEN
      UPDATE public.embedding_jobs SET status='failed',lease_expires_at=NULL,
        error_code='provider_timeout',completed_at=clock_timestamp()
      WHERE tenant_id=p_tenant_id AND id=p_job_id;
    END IF;
    RETURN 'stopped_stale';
  END IF;
  IF vector_dims(v_embedding) <> v_job.dimensions THEN
    RAISE EXCEPTION 'invalid embedding response' USING ERRCODE='22023';
  END IF;
  PERFORM 1 FROM public.items WHERE tenant_id=p_tenant_id AND id=v_job.item_id FOR UPDATE;
  SELECT EXISTS (
    SELECT 1 FROM public.items i
    JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    JOIN public.principals p
      ON p.tenant_id=c.tenant_id AND p.id=c.owner_principal_id AND p.active
    JOIN public.active_embedding_generations a
      ON a.tenant_id=c.tenant_id AND a.app_id=c.app_id
     AND a.generation_id=v_job.generation_id
    WHERE i.tenant_id=p_tenant_id AND i.id=v_job.item_id
      AND i.active_revision_id=v_job.revision_id AND i.deleted_at IS NULL
      AND i.deletion_generation=v_job.deletion_generation
      AND c.audience_kind='private' AND c.withdrawn_at IS NULL
      AND (v_job.passage_id IS NULL OR EXISTS (
        SELECT 1 FROM public.active_extraction_sets x
        WHERE x.tenant_id=v_job.tenant_id AND x.item_id=v_job.item_id
          AND x.revision_id=v_job.revision_id
          AND x.source_revision_id=v_job.source_revision_id
          AND x.extraction_set_id=v_job.extraction_set_id
      ))
  ) INTO v_current;
  IF NOT v_current THEN
    UPDATE public.embedding_jobs SET status='stopped_stale',lease_expires_at=NULL
    WHERE tenant_id=p_tenant_id AND id=p_job_id;
    RETURN 'stopped_stale';
  END IF;
  INSERT INTO public.embedding_representations
    (tenant_id,job_id,item_id,revision_id,source_revision_id,extraction_set_id,passage_id,
     generation_id,model_version,input_recipe_version,dimensions,input_digest,embedding)
  VALUES
    (v_job.tenant_id,v_job.id,v_job.item_id,v_job.revision_id,v_job.source_revision_id,
     v_job.extraction_set_id,v_job.passage_id,v_job.generation_id,v_job.model_version,
     v_job.input_recipe_version,v_job.dimensions,v_job.input_digest,v_embedding)
  ON CONFLICT DO NOTHING;
  UPDATE public.embedding_jobs SET status='complete',lease_expires_at=NULL,
    completed_at=clock_timestamp() WHERE tenant_id=p_tenant_id AND id=p_job_id;
  RETURN 'complete';
END
$$;

CREATE OR REPLACE FUNCTION search_exact_semantic_candidates(
  p_tenant_id text, p_credential_id text, p_generation_id text,
  p_query_embedding text, p_scope_subject_ids text[], p_limit integer
)
RETURNS TABLE (
  item_id text, revision_id text, source_revision_id text,
  extraction_set_id text, passage_id text, distance double precision
)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_principal_id text; v_app_id text; v_generation public.embedding_generations%ROWTYPE;
        v_query vector; v_self_distance double precision;
BEGIN
  BEGIN
    v_query := p_query_embedding::vector;
  EXCEPTION WHEN OTHERS THEN
    RAISE EXCEPTION 'invalid semantic search input' USING ERRCODE='22023';
  END;
  IF (p_limit BETWEEN 1 AND 41) IS NOT TRUE OR p_query_embedding IS NULL
     OR vector_dims(v_query) NOT BETWEEN 1 AND 4096
     OR p_query_embedding ~ 'NaN|Infinity' OR vector_norm(v_query) <= 0
     OR nullif(current_setting('app.operation',true),'') <> 'search' THEN
    RAISE EXCEPTION 'invalid semantic search input' USING ERRCODE='22023';
  END IF;
  v_self_distance := v_query <=> v_query;
  IF v_self_distance IS NULL
     OR v_self_distance::text IN ('NaN','Infinity','-Infinity')
     OR v_self_distance <> 0 THEN
    RAISE EXCEPTION 'invalid semantic search input' USING ERRCODE='22023';
  END IF;
  PERFORM 1 FROM public.tenant_authority WHERE tenant_id=p_tenant_id FOR SHARE;
  IF NOT FOUND OR public.current_reader_context(p_tenant_id) IS NOT TRUE THEN
    RAISE EXCEPTION 'unavailable semantic authority' USING ERRCODE='42501';
  END IF;
  SELECT c.principal_id,c.app_id INTO v_principal_id,v_app_id
  FROM public.credentials c JOIN public.principals p
    ON p.tenant_id=c.tenant_id AND p.id=c.principal_id
  JOIN public.apps a ON a.tenant_id=c.tenant_id AND a.id=c.app_id
  WHERE c.tenant_id=p_tenant_id AND c.id=p_credential_id
    AND c.token_digest=decode(current_setting('app.credential_digest',true),'hex')
    AND c.credential_class='agent_reader' AND 'search'=ANY(c.allowed_operations)
    AND c.issued_at<=clock_timestamp() AND c.expires_at>clock_timestamp()
    AND c.revoked_at IS NULL AND p.active AND a.active;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'unavailable semantic generation' USING ERRCODE='42501';
  END IF;
  SELECT g.* INTO v_generation
  FROM public.active_embedding_generations a
  JOIN public.embedding_generations g
    ON g.tenant_id=a.tenant_id AND g.id=a.generation_id
  WHERE a.tenant_id=p_tenant_id AND a.app_id=v_app_id
    AND a.generation_id=p_generation_id;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'unavailable semantic generation' USING ERRCODE='42501';
  END IF;
  IF vector_dims(v_query) <> v_generation.dimensions THEN
    RAISE EXCEPTION 'invalid semantic search input' USING ERRCODE='22023';
  END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       cardinality(p_scope_subject_ids) NOT BETWEEN 1 AND 8
       OR cardinality(p_scope_subject_ids) <>
          (SELECT count(DISTINCT value) FROM unnest(p_scope_subject_ids) AS value)
       OR EXISTS (SELECT 1 FROM unnest(p_scope_subject_ids) AS value
                  WHERE value !~ '^[A-Za-z0-9_-]{1,64}$')
     ) THEN RAISE EXCEPTION 'invalid semantic scope' USING ERRCODE='22023'; END IF;

  RETURN QUERY
  SELECT e.item_id,e.revision_id,e.source_revision_id,e.extraction_set_id,e.passage_id,
         e.embedding <=> v_query AS distance
  FROM public.embedding_representations e
  JOIN public.items i ON i.tenant_id=e.tenant_id AND i.id=e.item_id
  JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
  JOIN public.revisions r
    ON r.tenant_id=e.tenant_id AND r.item_id=e.item_id AND r.id=e.revision_id
  WHERE e.tenant_id=p_tenant_id AND e.generation_id=v_generation.id
    AND e.model_version=v_generation.model_version
    AND e.input_recipe_version=v_generation.input_recipe_version
    AND e.dimensions=v_generation.dimensions
    AND vector_dims(e.embedding)=v_generation.dimensions
    AND i.active_revision_id=e.revision_id AND i.deleted_at IS NULL
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
    AND (e.passage_id IS NULL OR EXISTS (
      SELECT 1 FROM public.active_extraction_sets x
      JOIN public.extraction_sets xs
        ON xs.tenant_id=x.tenant_id AND xs.item_id=x.item_id
       AND xs.revision_id=x.revision_id AND xs.source_revision_id=x.source_revision_id
       AND xs.id=x.extraction_set_id
      WHERE x.tenant_id=e.tenant_id AND x.item_id=e.item_id
        AND x.revision_id=e.revision_id AND x.source_revision_id=e.source_revision_id
        AND x.extraction_set_id=e.extraction_set_id
        AND xs.passage_count=(SELECT count(*) FROM public.source_passages sp
          WHERE sp.tenant_id=xs.tenant_id AND sp.item_id=xs.item_id
            AND sp.revision_id=xs.revision_id AND sp.source_revision_id=xs.source_revision_id
            AND sp.extraction_set_id=xs.id)
        AND xs.content_bytes=(SELECT sum(octet_length(sp.content)) FROM public.source_passages sp
          WHERE sp.tenant_id=xs.tenant_id AND sp.item_id=xs.item_id
            AND sp.revision_id=xs.revision_id AND sp.source_revision_id=xs.source_revision_id
            AND sp.extraction_set_id=xs.id)
        AND xs.passage_count=(SELECT count(*) FROM public.embedding_representations er
          WHERE er.tenant_id=xs.tenant_id AND er.item_id=xs.item_id
            AND er.revision_id=xs.revision_id AND er.source_revision_id=xs.source_revision_id
            AND er.extraction_set_id=xs.id AND er.generation_id=v_generation.id)))
    AND (e.passage_id IS NOT NULL OR NOT EXISTS (
      SELECT 1 FROM public.active_extraction_sets x
      WHERE x.tenant_id=e.tenant_id AND x.item_id=e.item_id AND x.revision_id=e.revision_id))
  ORDER BY e.embedding <=> v_query,e.item_id,e.revision_id,
           e.source_revision_id NULLS FIRST,e.extraction_set_id NULLS FIRST,e.passage_id NULLS FIRST
  LIMIT p_limit;
END
$$;

COMMIT;
