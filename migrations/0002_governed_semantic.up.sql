BEGIN;

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE embedding_generations (
  tenant_id text NOT NULL,
  id text NOT NULL CHECK (id ~ '^[A-Za-z0-9_-]{1,64}$'),
  model_version text NOT NULL CHECK (octet_length(model_version) BETWEEN 1 AND 128),
  input_recipe_version text NOT NULL CHECK (input_recipe_version ~ '^[A-Za-z0-9_-]{1,64}$'),
  input_recipe text NOT NULL CHECK (octet_length(input_recipe) BETWEEN 1 AND 512),
  dimensions integer NOT NULL CHECK (dimensions = 3),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (tenant_id, id),
  UNIQUE (tenant_id, model_version, input_recipe_version),
  FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE
);

CREATE TABLE active_embedding_generations (
  tenant_id text NOT NULL,
  app_id text NOT NULL,
  generation_id text NOT NULL,
  selected_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (tenant_id, app_id),
  FOREIGN KEY (tenant_id, app_id) REFERENCES apps (tenant_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, generation_id)
    REFERENCES embedding_generations (tenant_id, id) ON DELETE RESTRICT
);

CREATE TABLE embedding_jobs (
  tenant_id text NOT NULL,
  id text NOT NULL DEFAULT replace(gen_random_uuid()::text, '-', ''),
  item_id text NOT NULL,
  revision_id text NOT NULL,
  source_revision_id text,
  extraction_set_id text,
  passage_id text,
  generation_id text NOT NULL,
  model_version text NOT NULL CHECK (octet_length(model_version) BETWEEN 1 AND 128),
  input_recipe_version text NOT NULL CHECK (input_recipe_version ~ '^[A-Za-z0-9_-]{1,64}$'),
  dimensions integer NOT NULL CHECK (dimensions = 3),
  input_digest bytea NOT NULL CHECK (octet_length(input_digest) = 32),
  deletion_generation bigint NOT NULL,
  status text NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending','leased','retry','failed','cancelled','stopped_stale','complete')),
  attempt bigint NOT NULL DEFAULT 0 CHECK (attempt >= 0),
  max_attempts integer NOT NULL DEFAULT 3 CHECK (max_attempts BETWEEN 1 AND 5),
  available_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  lease_expires_at timestamptz,
  error_code text CHECK (error_code IS NULL OR error_code IN
    ('provider_failed','provider_timeout','invalid_response','cancelled')),
  completed_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  deadline_at timestamptz NOT NULL DEFAULT statement_timestamp()+interval '10 minutes',
  PRIMARY KEY (tenant_id, id),
  UNIQUE NULLS NOT DISTINCT (
    tenant_id, item_id, revision_id, source_revision_id,
    extraction_set_id, passage_id, generation_id
  ),
  FOREIGN KEY (tenant_id, item_id, revision_id)
    REFERENCES revisions (tenant_id, item_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, generation_id)
    REFERENCES embedding_generations (tenant_id, id) ON DELETE RESTRICT,
  CHECK (
    (source_revision_id IS NULL AND extraction_set_id IS NULL AND passage_id IS NULL)
    OR
    (source_revision_id IS NOT NULL AND extraction_set_id IS NOT NULL AND passage_id IS NOT NULL)
  ),
  CHECK ((status = 'leased') = (lease_expires_at IS NOT NULL)),
  CHECK ((status IN ('failed','cancelled')) = (error_code IS NOT NULL)),
  CHECK (deadline_at <= created_at+interval '10 minutes')
);

CREATE TABLE embedding_representations (
  tenant_id text NOT NULL,
  job_id text NOT NULL,
  item_id text NOT NULL,
  revision_id text NOT NULL,
  source_revision_id text,
  extraction_set_id text,
  passage_id text,
  generation_id text NOT NULL,
  model_version text NOT NULL,
  input_recipe_version text NOT NULL,
  dimensions integer NOT NULL CHECK (dimensions = 3),
  input_digest bytea NOT NULL CHECK (octet_length(input_digest) = 32),
  embedding vector(3) NOT NULL,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (tenant_id, job_id),
  UNIQUE NULLS NOT DISTINCT (
    tenant_id, item_id, revision_id, source_revision_id,
    extraction_set_id, passage_id, generation_id
  ),
  FOREIGN KEY (tenant_id, job_id)
    REFERENCES embedding_jobs (tenant_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, item_id, revision_id)
    REFERENCES revisions (tenant_id, item_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, generation_id)
    REFERENCES embedding_generations (tenant_id, id) ON DELETE RESTRICT,
  CHECK (
    (source_revision_id IS NULL AND extraction_set_id IS NULL AND passage_id IS NULL)
    OR
    (source_revision_id IS NOT NULL AND extraction_set_id IS NOT NULL AND passage_id IS NOT NULL)
  ),
  CHECK (vector_dims(embedding) = dimensions),
  CHECK (embedding::text !~ 'NaN|Infinity')
);

CREATE TRIGGER embedding_generations_are_immutable
BEFORE UPDATE ON embedding_generations
FOR EACH ROW EXECUTE FUNCTION reject_derived_record_mutation();

CREATE TRIGGER embedding_representations_are_immutable
BEFORE UPDATE ON embedding_representations
FOR EACH ROW EXECUTE FUNCTION reject_derived_record_mutation();

CREATE OR REPLACE FUNCTION enqueue_selected_embedding_generation()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
BEGIN
  PERFORM 1 FROM public.tenant_authority
  WHERE tenant_id=NEW.tenant_id FOR UPDATE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'missing embedding tenant authority' USING ERRCODE='P0003';
  END IF;
  INSERT INTO public.embedding_jobs
    (tenant_id,item_id,revision_id,generation_id,model_version,
     input_recipe_version,dimensions,input_digest,deletion_generation)
  SELECT i.tenant_id,i.id,r.id,g.id,g.model_version,g.input_recipe_version,g.dimensions,
         digest(g.input_recipe_version || E'\n' || r.content,'sha256'),i.deletion_generation
  FROM public.embedding_generations g
  JOIN public.items i ON i.tenant_id=g.tenant_id AND i.deleted_at IS NULL
  JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
  JOIN public.revisions r
    ON r.tenant_id=i.tenant_id AND r.item_id=i.id AND r.id=i.active_revision_id
  JOIN public.lexical_representations l
    ON l.tenant_id=r.tenant_id AND l.item_id=r.item_id AND l.revision_id=r.id
  WHERE g.tenant_id=NEW.tenant_id AND g.id=NEW.generation_id
    AND c.app_id=NEW.app_id AND c.audience_kind='private' AND c.withdrawn_at IS NULL
  ON CONFLICT DO NOTHING;

  INSERT INTO public.embedding_jobs
    (tenant_id,item_id,revision_id,source_revision_id,extraction_set_id,passage_id,
     generation_id,model_version,input_recipe_version,dimensions,input_digest,
     deletion_generation)
  SELECT p.tenant_id,p.item_id,p.revision_id,p.source_revision_id,p.extraction_set_id,p.id,
         g.id,g.model_version,g.input_recipe_version,g.dimensions,
         digest(g.input_recipe_version || E'\n' || p.content,'sha256'),i.deletion_generation
  FROM public.embedding_generations g
  JOIN public.items i ON i.tenant_id=g.tenant_id AND i.deleted_at IS NULL
  JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
  JOIN public.active_extraction_sets x
    ON x.tenant_id=i.tenant_id AND x.item_id=i.id AND x.revision_id=i.active_revision_id
  JOIN public.source_passages p
    ON p.tenant_id=x.tenant_id AND p.item_id=x.item_id AND p.revision_id=x.revision_id
   AND p.source_revision_id=x.source_revision_id AND p.extraction_set_id=x.extraction_set_id
  WHERE g.tenant_id=NEW.tenant_id AND g.id=NEW.generation_id
    AND c.app_id=NEW.app_id AND c.audience_kind='private' AND c.withdrawn_at IS NULL
  ON CONFLICT DO NOTHING;
  RETURN NEW;
END
$$;

CREATE TRIGGER selected_embedding_generation_intent
AFTER INSERT OR UPDATE OF generation_id ON active_embedding_generations
FOR EACH ROW EXECUTE FUNCTION enqueue_selected_embedding_generation();

CREATE OR REPLACE FUNCTION enqueue_embedding_job()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE
  v_generation public.embedding_generations%ROWTYPE;
  v_deletion_generation bigint;
  v_content text;
  v_app_id text;
BEGIN
  SELECT g.* INTO v_generation
  FROM public.items i
  JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
  JOIN public.active_embedding_generations a
    ON a.tenant_id=c.tenant_id AND a.app_id=c.app_id
  JOIN public.embedding_generations g
    ON g.tenant_id=a.tenant_id AND g.id=a.generation_id
  WHERE i.tenant_id=NEW.tenant_id AND i.id=NEW.item_id
    AND c.audience_kind='private';
  IF NOT FOUND THEN RETURN NEW; END IF;

  SELECT i.deletion_generation, c.app_id INTO v_deletion_generation, v_app_id
  FROM public.items i JOIN public.collections c
    ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
  WHERE i.tenant_id=NEW.tenant_id AND i.id=NEW.item_id;
  IF TG_TABLE_NAME = 'lexical_representations' THEN
    SELECT r.content INTO v_content FROM public.revisions r
    WHERE r.tenant_id=NEW.tenant_id AND r.item_id=NEW.item_id AND r.id=NEW.revision_id;
    INSERT INTO public.embedding_jobs
      (tenant_id,item_id,revision_id,generation_id,model_version,
       input_recipe_version,dimensions,input_digest,deletion_generation)
    VALUES
      (NEW.tenant_id,NEW.item_id,NEW.revision_id,v_generation.id,v_generation.model_version,
       v_generation.input_recipe_version,v_generation.dimensions,
       digest(v_generation.input_recipe_version || E'\n' || v_content,'sha256'),
       v_deletion_generation)
    ON CONFLICT DO NOTHING;
  ELSE
    INSERT INTO public.embedding_jobs
      (tenant_id,item_id,revision_id,source_revision_id,extraction_set_id,passage_id,
       generation_id,model_version,input_recipe_version,dimensions,input_digest,
       deletion_generation)
    VALUES
      (NEW.tenant_id,NEW.item_id,NEW.revision_id,NEW.source_revision_id,
       NEW.extraction_set_id,NEW.id,v_generation.id,v_generation.model_version,
       v_generation.input_recipe_version,v_generation.dimensions,
       digest(v_generation.input_recipe_version || E'\n' || NEW.content,'sha256'),
       v_deletion_generation)
    ON CONFLICT DO NOTHING;
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER lexical_embedding_intent
AFTER INSERT ON lexical_representations
FOR EACH ROW EXECUTE FUNCTION enqueue_embedding_job();
CREATE TRIGGER passage_embedding_intent
AFTER INSERT ON source_passages
FOR EACH ROW EXECUTE FUNCTION enqueue_embedding_job();

CREATE FUNCTION claim_embedding_jobs(p_limit integer, p_lease_seconds integer)
RETURNS TABLE (
  tenant_id text, job_id text, attempt bigint, item_id text, revision_id text,
  source_revision_id text, extraction_set_id text, passage_id text,
  generation_id text, model_version text, input_recipe_version text,
  dimensions integer, input_digest bytea, content text
)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_tenant_ids text[];
BEGIN
  IF (p_limit BETWEEN 1 AND 16) IS NOT TRUE
     OR (p_lease_seconds BETWEEN 1 AND 300) IS NOT TRUE THEN
    RAISE EXCEPTION 'invalid embedding claim bounds' USING ERRCODE='22023';
  END IF;

  -- Raw input leaves PostgreSQL at claim time. Join the same tenant-wide
  -- happens-before boundary used by revocation/withdrawal before rechecking
  -- current private authority and returning any content.
  SELECT coalesce(array_agg(t.tenant_id ORDER BY t.tenant_id),ARRAY[]::text[])
  INTO v_tenant_ids
  FROM (
    SELECT DISTINCT bounded.tenant_id
    FROM (
      SELECT j.tenant_id
      FROM public.embedding_jobs j
      WHERE (j.status IN ('pending','retry') OR
             (j.status='leased' AND j.lease_expires_at<=clock_timestamp()))
        AND j.available_at<=clock_timestamp()
      ORDER BY j.available_at,j.created_at,j.tenant_id,j.id
      LIMIT p_limit
    ) bounded
  ) t;

  PERFORM 1
  FROM public.tenant_authority t
  WHERE t.tenant_id=ANY(v_tenant_ids)
  ORDER BY t.tenant_id
  FOR SHARE;

  WITH exhausted AS (
    SELECT j.tenant_id,j.id
    FROM public.embedding_jobs j
    WHERE j.tenant_id=ANY(v_tenant_ids)
      AND (j.status IN ('pending','retry') OR
           (j.status='leased' AND j.lease_expires_at<=clock_timestamp()))
      AND (j.deadline_at<=clock_timestamp()
        OR (j.status='leased' AND j.attempt>=j.max_attempts))
    ORDER BY j.available_at,j.created_at,j.tenant_id,j.id
    FOR UPDATE SKIP LOCKED LIMIT p_limit
  )
  UPDATE public.embedding_jobs j
  SET status='failed',lease_expires_at=NULL,error_code='provider_timeout',
      completed_at=clock_timestamp()
  FROM exhausted e WHERE j.tenant_id=e.tenant_id AND j.id=e.id;

  WITH stale AS (
    SELECT j.tenant_id,j.id
    FROM public.embedding_jobs j
    WHERE j.tenant_id=ANY(v_tenant_ids)
      AND (j.status IN ('pending','retry') OR
           (j.status='leased' AND j.lease_expires_at<=clock_timestamp()))
      AND NOT EXISTS (
        SELECT 1 FROM public.items i
        JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
        JOIN public.principals p
          ON p.tenant_id=c.tenant_id AND p.id=c.owner_principal_id AND p.active
        JOIN public.active_embedding_generations a
          ON a.tenant_id=c.tenant_id AND a.app_id=c.app_id AND a.generation_id=j.generation_id
        WHERE i.tenant_id=j.tenant_id AND i.id=j.item_id
          AND i.active_revision_id=j.revision_id AND i.deleted_at IS NULL
          AND i.deletion_generation=j.deletion_generation
          AND c.audience_kind='private' AND c.withdrawn_at IS NULL
          AND (j.passage_id IS NULL OR EXISTS (
            SELECT 1 FROM public.active_extraction_sets x
            WHERE x.tenant_id=j.tenant_id AND x.item_id=j.item_id
              AND x.revision_id=j.revision_id
              AND x.source_revision_id=j.source_revision_id
              AND x.extraction_set_id=j.extraction_set_id)))
    ORDER BY j.available_at,j.created_at,j.tenant_id,j.id
    FOR UPDATE SKIP LOCKED LIMIT p_limit
  )
  UPDATE public.embedding_jobs j SET status='stopped_stale',lease_expires_at=NULL
  FROM stale s WHERE j.tenant_id=s.tenant_id AND j.id=s.id;

  RETURN QUERY
  WITH selected AS (
    SELECT j.tenant_id, j.id
    FROM public.embedding_jobs j
    WHERE j.tenant_id=ANY(v_tenant_ids)
      AND (j.status IN ('pending','retry') OR
           (j.status='leased' AND j.lease_expires_at <= clock_timestamp()))
      AND j.attempt < j.max_attempts AND j.available_at <= clock_timestamp()
      AND j.deadline_at > clock_timestamp()
      AND EXISTS (
        SELECT 1 FROM public.items i
        JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
        JOIN public.principals p
          ON p.tenant_id=c.tenant_id AND p.id=c.owner_principal_id AND p.active
        JOIN public.active_embedding_generations a
          ON a.tenant_id=c.tenant_id AND a.app_id=c.app_id AND a.generation_id=j.generation_id
        WHERE i.tenant_id=j.tenant_id AND i.id=j.item_id
          AND i.active_revision_id=j.revision_id AND i.deleted_at IS NULL
          AND i.deletion_generation=j.deletion_generation
          AND c.audience_kind='private' AND c.withdrawn_at IS NULL
          AND (j.passage_id IS NULL OR EXISTS (
            SELECT 1 FROM public.active_extraction_sets x
            WHERE x.tenant_id=j.tenant_id AND x.item_id=j.item_id
              AND x.revision_id=j.revision_id
              AND x.source_revision_id=j.source_revision_id
              AND x.extraction_set_id=j.extraction_set_id)))
      ORDER BY j.available_at, j.created_at, j.tenant_id, j.id
    FOR UPDATE SKIP LOCKED LIMIT p_limit
  ), claimed AS (
    UPDATE public.embedding_jobs j
    SET status='leased', attempt=j.attempt+1,
        lease_expires_at=clock_timestamp()+make_interval(secs=>p_lease_seconds),
        error_code=NULL
    FROM selected s WHERE j.tenant_id=s.tenant_id AND j.id=s.id
    RETURNING j.*
  )
  SELECT j.tenant_id,j.id,j.attempt,j.item_id,j.revision_id,
         j.source_revision_id,j.extraction_set_id,j.passage_id,j.generation_id,
         j.model_version,j.input_recipe_version,j.dimensions,j.input_digest,
         coalesce(sp.content,r.content)
  FROM claimed j
  JOIN public.revisions r
    ON r.tenant_id=j.tenant_id AND r.item_id=j.item_id AND r.id=j.revision_id
  LEFT JOIN public.source_passages sp
    ON sp.tenant_id=j.tenant_id AND sp.item_id=j.item_id
   AND sp.revision_id=j.revision_id AND sp.source_revision_id=j.source_revision_id
   AND sp.extraction_set_id=j.extraction_set_id AND sp.id=j.passage_id;
END
$$;

CREATE FUNCTION complete_embedding_job(
  p_tenant_id text, p_job_id text, p_attempt bigint, p_embedding text
)
RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_job public.embedding_jobs%ROWTYPE; v_current boolean;
BEGIN
  IF p_embedding IS NULL OR vector_dims(p_embedding::vector) <> 3
     OR p_embedding ~ 'NaN|Infinity' THEN
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
     v_job.input_recipe_version,v_job.dimensions,v_job.input_digest,p_embedding::vector(3))
  ON CONFLICT DO NOTHING;
  UPDATE public.embedding_jobs SET status='complete',lease_expires_at=NULL,
    completed_at=clock_timestamp() WHERE tenant_id=p_tenant_id AND id=p_job_id;
  RETURN 'complete';
END
$$;

CREATE FUNCTION fail_embedding_job(
  p_tenant_id text, p_job_id text, p_attempt bigint, p_error_code text
)
RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_job public.embedding_jobs%ROWTYPE; v_status text;
BEGIN
  IF p_error_code NOT IN ('provider_failed','provider_timeout','invalid_response') THEN
    RAISE EXCEPTION 'invalid embedding failure code' USING ERRCODE='22023';
  END IF;
  SELECT * INTO v_job FROM public.embedding_jobs
  WHERE tenant_id=p_tenant_id AND id=p_job_id FOR UPDATE;
  IF NOT FOUND OR v_job.status <> 'leased' OR (v_job.attempt=p_attempt) IS NOT TRUE
     OR v_job.lease_expires_at <= clock_timestamp() THEN
    RETURN 'stopped_stale';
  END IF;
  v_status := CASE WHEN v_job.attempt < v_job.max_attempts THEN 'retry' ELSE 'failed' END;
  UPDATE public.embedding_jobs SET status=v_status, lease_expires_at=NULL,
    available_at=clock_timestamp()+make_interval(secs=>least(60,attempt*attempt)),
    error_code=CASE WHEN v_status='failed' THEN p_error_code ELSE NULL END
  WHERE tenant_id=p_tenant_id AND id=p_job_id;
  RETURN v_status;
END
$$;

CREATE FUNCTION cancel_embedding_job(p_tenant_id text, p_job_id text, p_attempt bigint)
RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
BEGIN
  UPDATE public.embedding_jobs SET status='cancelled',lease_expires_at=NULL,
    error_code='cancelled'
  WHERE tenant_id=p_tenant_id AND id=p_job_id AND status='leased' AND attempt=p_attempt
    AND lease_expires_at > clock_timestamp();
  IF FOUND THEN RETURN 'cancelled'; END IF;
  RETURN 'stopped_stale';
END
$$;

CREATE FUNCTION search_exact_semantic_candidates(
  p_tenant_id text, p_credential_id text, p_generation_id text,
  p_query_embedding text, p_scope_subject_ids text[], p_limit integer
)
RETURNS TABLE (
  item_id text, revision_id text, source_revision_id text,
  extraction_set_id text, passage_id text, distance double precision
)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE v_principal_id text; v_app_id text;
BEGIN
  IF (p_limit BETWEEN 1 AND 41) IS NOT TRUE OR p_query_embedding IS NULL
     OR vector_dims(p_query_embedding::vector) <> 3
     OR p_query_embedding ~ 'NaN|Infinity'
     OR nullif(current_setting('app.operation',true),'') <> 'search' THEN
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
  IF NOT FOUND OR NOT EXISTS (
    SELECT 1 FROM public.active_embedding_generations g
    WHERE g.tenant_id=p_tenant_id AND g.app_id=v_app_id AND g.generation_id=p_generation_id
  ) THEN RAISE EXCEPTION 'unavailable semantic generation' USING ERRCODE='42501'; END IF;
  IF p_scope_subject_ids IS NOT NULL AND (
       cardinality(p_scope_subject_ids) NOT BETWEEN 1 AND 8
       OR cardinality(p_scope_subject_ids) <>
          (SELECT count(DISTINCT value) FROM unnest(p_scope_subject_ids) AS value)
       OR EXISTS (SELECT 1 FROM unnest(p_scope_subject_ids) AS value
                  WHERE value !~ '^[A-Za-z0-9_-]{1,64}$')
     ) THEN RAISE EXCEPTION 'invalid semantic scope' USING ERRCODE='22023'; END IF;

  RETURN QUERY
  SELECT e.item_id,e.revision_id,e.source_revision_id,e.extraction_set_id,e.passage_id,
         e.embedding <=> p_query_embedding::vector AS distance
  FROM public.embedding_representations e
  JOIN public.items i ON i.tenant_id=e.tenant_id AND i.id=e.item_id
  JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
  JOIN public.revisions r
    ON r.tenant_id=e.tenant_id AND r.item_id=e.item_id AND r.id=e.revision_id
  WHERE e.tenant_id=p_tenant_id AND e.generation_id=p_generation_id
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
            AND er.extraction_set_id=xs.id AND er.generation_id=p_generation_id)))
    AND (e.passage_id IS NOT NULL OR NOT EXISTS (
      SELECT 1 FROM public.active_extraction_sets x
      WHERE x.tenant_id=e.tenant_id AND x.item_id=e.item_id AND x.revision_id=e.revision_id))
  ORDER BY e.embedding <=> p_query_embedding::vector,e.item_id,e.revision_id,
           e.source_revision_id NULLS FIRST,e.extraction_set_id NULLS FIRST,e.passage_id NULLS FIRST
  LIMIT p_limit;
END
$$;

CREATE FUNCTION search_hybrid_current_memories(
  p_tenant_id text, p_credential_id text, p_request_id text, p_query text,
  p_max_context_bytes integer, p_scope_subject_ids text[],
  p_generation_id text, p_query_embedding text
)
RETURNS TABLE (item_id text, revision_id text, content text, recorded_at text,
               valid_from text, valid_until text, validity_status text,
               hit_kind text, source_revision_id text, extraction_set_id text,
               passage_id text, locator text, parent_passage_ids text[],
               candidate_omitted boolean)
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
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
$$;

CREATE FUNCTION semantic_readiness(p_tenant_id text, p_credential_id text)
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
  SELECT a.generation_id INTO v_generation_id FROM public.active_embedding_generations a
  WHERE a.tenant_id=p_tenant_id AND a.app_id=v_app_id;
  IF NOT FOUND THEN RETURN QUERY SELECT NULL::text,NULL::text,NULL::text,NULL::integer,'pending'::text,'semantic_configuration_missing'::text; RETURN; END IF;
  IF EXISTS (
    SELECT 1 FROM public.items i JOIN public.collections c
      ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    JOIN public.source_revisions s
      ON s.tenant_id=i.tenant_id AND s.item_id=i.id AND s.revision_id=i.active_revision_id
    WHERE i.tenant_id=p_tenant_id AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL
      AND c.app_id=v_app_id AND NOT EXISTS (SELECT 1 FROM public.active_extraction_sets x
        WHERE x.tenant_id=i.tenant_id AND x.item_id=i.id AND x.revision_id=i.active_revision_id)
      AND ((c.audience_kind='private' AND c.owner_principal_id=v_principal_id)
        OR (c.audience_kind='restricted' AND EXISTS (SELECT 1 FROM public.collection_grants cg
          WHERE cg.tenant_id=c.tenant_id AND cg.collection_id=c.id
            AND cg.principal_id=v_principal_id AND cg.can_read)))
  ) THEN RETURN QUERY SELECT g.id,g.model_version,g.input_recipe_version,g.dimensions,'pending'::text,'extraction_not_ready'::text FROM public.embedding_generations g WHERE g.tenant_id=p_tenant_id AND g.id=v_generation_id; RETURN; END IF;
  IF EXISTS (
    SELECT 1 FROM public.embedding_jobs j
    JOIN public.items i ON i.tenant_id=j.tenant_id AND i.id=j.item_id
    JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    WHERE j.tenant_id=p_tenant_id AND j.generation_id=v_generation_id
      AND i.active_revision_id=j.revision_id AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL
      AND c.app_id=v_app_id
      AND (j.passage_id IS NULL OR EXISTS (SELECT 1 FROM public.active_extraction_sets x
        WHERE x.tenant_id=j.tenant_id AND x.item_id=j.item_id AND x.revision_id=j.revision_id
          AND x.source_revision_id=j.source_revision_id AND x.extraction_set_id=j.extraction_set_id))
      AND ((c.audience_kind='private' AND c.owner_principal_id=v_principal_id)
        OR (c.audience_kind='restricted' AND EXISTS (SELECT 1 FROM public.collection_grants cg
          WHERE cg.tenant_id=c.tenant_id AND cg.collection_id=c.id
            AND cg.principal_id=v_principal_id AND cg.can_read)))
      AND j.status IN ('failed','cancelled')
  ) THEN RETURN QUERY SELECT g.id,g.model_version,g.input_recipe_version,g.dimensions,'failed'::text,'semantic_processing_failed'::text FROM public.embedding_generations g WHERE g.tenant_id=p_tenant_id AND g.id=v_generation_id; RETURN; END IF;
  IF EXISTS (
    SELECT 1 FROM public.embedding_jobs j
    JOIN public.items i ON i.tenant_id=j.tenant_id AND i.id=j.item_id
    JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    WHERE j.tenant_id=p_tenant_id AND j.generation_id=v_generation_id
      AND i.active_revision_id=j.revision_id AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL
      AND c.app_id=v_app_id
      AND j.status IN ('pending','leased','retry')
      AND (j.passage_id IS NULL OR EXISTS (SELECT 1 FROM public.active_extraction_sets x
        WHERE x.tenant_id=j.tenant_id AND x.item_id=j.item_id AND x.revision_id=j.revision_id
          AND x.source_revision_id=j.source_revision_id AND x.extraction_set_id=j.extraction_set_id))
      AND ((c.audience_kind='private' AND c.owner_principal_id=v_principal_id)
        OR (c.audience_kind='restricted' AND EXISTS (SELECT 1 FROM public.collection_grants cg
          WHERE cg.tenant_id=c.tenant_id AND cg.collection_id=c.id
            AND cg.principal_id=v_principal_id AND cg.can_read)))
  ) THEN RETURN QUERY SELECT g.id,g.model_version,g.input_recipe_version,g.dimensions,'pending'::text,'semantic_processing_pending'::text FROM public.embedding_generations g WHERE g.tenant_id=p_tenant_id AND g.id=v_generation_id; RETURN; END IF;
  IF EXISTS (
    SELECT 1 FROM public.lexical_representations l
    JOIN public.items i ON i.tenant_id=l.tenant_id AND i.id=l.item_id
    JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    WHERE l.tenant_id=p_tenant_id AND i.active_revision_id=l.revision_id
      AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL AND c.app_id=v_app_id
      AND ((c.audience_kind='private' AND c.owner_principal_id=v_principal_id)
        OR (c.audience_kind='restricted' AND EXISTS (SELECT 1 FROM public.collection_grants cg
          WHERE cg.tenant_id=c.tenant_id AND cg.collection_id=c.id
            AND cg.principal_id=v_principal_id AND cg.can_read)))
      AND NOT EXISTS (SELECT 1 FROM public.embedding_representations e
        WHERE e.tenant_id=l.tenant_id AND e.item_id=l.item_id AND e.revision_id=l.revision_id
          AND e.source_revision_id IS NULL AND e.extraction_set_id IS NULL
          AND e.passage_id IS NULL AND e.generation_id=v_generation_id)
  ) OR EXISTS (
    SELECT 1 FROM public.active_extraction_sets x
    JOIN public.items i ON i.tenant_id=x.tenant_id AND i.id=x.item_id
    JOIN public.collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
    JOIN public.source_passages p
      ON p.tenant_id=x.tenant_id AND p.item_id=x.item_id AND p.revision_id=x.revision_id
     AND p.source_revision_id=x.source_revision_id AND p.extraction_set_id=x.extraction_set_id
    WHERE x.tenant_id=p_tenant_id AND i.active_revision_id=x.revision_id
      AND i.deleted_at IS NULL AND c.withdrawn_at IS NULL AND c.app_id=v_app_id
      AND ((c.audience_kind='private' AND c.owner_principal_id=v_principal_id)
        OR (c.audience_kind='restricted' AND EXISTS (SELECT 1 FROM public.collection_grants cg
          WHERE cg.tenant_id=c.tenant_id AND cg.collection_id=c.id
            AND cg.principal_id=v_principal_id AND cg.can_read)))
      AND NOT EXISTS (SELECT 1 FROM public.embedding_representations e
        WHERE e.tenant_id=p.tenant_id AND e.item_id=p.item_id AND e.revision_id=p.revision_id
          AND e.source_revision_id=p.source_revision_id AND e.extraction_set_id=p.extraction_set_id
          AND e.passage_id=p.id AND e.generation_id=v_generation_id)
  ) THEN RETURN QUERY SELECT g.id,g.model_version,g.input_recipe_version,g.dimensions,'pending'::text,'semantic_processing_pending'::text FROM public.embedding_generations g WHERE g.tenant_id=p_tenant_id AND g.id=v_generation_id; RETURN; END IF;
  RETURN QUERY SELECT g.id,g.model_version,g.input_recipe_version,g.dimensions,'ready'::text,'semantic_ready'::text FROM public.embedding_generations g WHERE g.tenant_id=p_tenant_id AND g.id=v_generation_id;
END
$$;

ALTER TABLE embedding_generations ENABLE ROW LEVEL SECURITY;
ALTER TABLE active_embedding_generations ENABLE ROW LEVEL SECURITY;
ALTER TABLE embedding_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE embedding_representations ENABLE ROW LEVEL SECURITY;
ALTER TABLE embedding_generations FORCE ROW LEVEL SECURITY;
ALTER TABLE active_embedding_generations FORCE ROW LEVEL SECURITY;
ALTER TABLE embedding_jobs FORCE ROW LEVEL SECURITY;
ALTER TABLE embedding_representations FORCE ROW LEVEL SECURITY;

DO $$
DECLARE v_function regprocedure;
BEGIN
  FOR v_function IN
    SELECT p.oid::regprocedure
    FROM pg_proc p
    JOIN pg_depend d ON d.objid=p.oid AND d.classid='pg_proc'::regclass
    JOIN pg_extension e ON e.oid=d.refobjid
    WHERE e.extname='vector'
  LOOP
    EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC',v_function);
  END LOOP;
END
$$;

REVOKE ALL ON FUNCTION enqueue_selected_embedding_generation(), enqueue_embedding_job(), claim_embedding_jobs(integer,integer),
  complete_embedding_job(text,text,bigint,text), fail_embedding_job(text,text,bigint,text),
  cancel_embedding_job(text,text,bigint),
  search_exact_semantic_candidates(text,text,text,text,text[],integer),
  search_hybrid_current_memories(text,text,text,text,integer,text[],text,text),
  semantic_readiness(text,text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION claim_embedding_jobs(integer,integer),
  complete_embedding_job(text,text,bigint,text), fail_embedding_job(text,text,bigint,text),
  cancel_embedding_job(text,text,bigint) TO agentic_memory_purge_worker;
GRANT EXECUTE ON FUNCTION
  search_exact_semantic_candidates(text,text,text,text,text[],integer),
  search_hybrid_current_memories(text,text,text,text,integer,text[],text,text),
  semantic_readiness(text,text) TO agentic_memory_runtime;

COMMIT;
