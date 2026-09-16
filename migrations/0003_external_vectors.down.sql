BEGIN;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM embedding_generations WHERE dimensions <> 3)
     OR EXISTS (SELECT 1 FROM embedding_jobs WHERE dimensions <> 3)
     OR EXISTS (SELECT 1 FROM embedding_representations
                WHERE dimensions <> 3 OR vector_dims(embedding) <> 3) THEN
    RAISE EXCEPTION 'cannot downgrade external vectors while non-3-dimensional state exists'
      USING ERRCODE='55000';
  END IF;
END
$$;

DROP TRIGGER IF EXISTS embedding_job_identity_is_immutable ON embedding_jobs;
DROP FUNCTION IF EXISTS reject_embedding_job_identity_mutation();

ALTER TABLE embedding_representations
  DROP CONSTRAINT IF EXISTS embedding_representations_cosine_safe_check,
  DROP CONSTRAINT embedding_representations_nonzero_check,
  DROP CONSTRAINT embedding_representations_dimensions_check;
ALTER TABLE embedding_jobs DROP CONSTRAINT embedding_jobs_dimensions_check;
ALTER TABLE embedding_generations DROP CONSTRAINT embedding_generations_dimensions_check;
ALTER TABLE embedding_representations
  ALTER COLUMN embedding TYPE vector(3) USING embedding::vector(3);
ALTER TABLE embedding_generations
  ADD CONSTRAINT embedding_generations_dimensions_check CHECK (dimensions = 3);
ALTER TABLE embedding_jobs
  ADD CONSTRAINT embedding_jobs_dimensions_check CHECK (dimensions = 3);
ALTER TABLE embedding_representations
  ADD CONSTRAINT embedding_representations_dimensions_check CHECK (dimensions = 3);

COMMIT;
