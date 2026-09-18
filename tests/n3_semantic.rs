use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use governed_memory::router;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::fmt::Write;
use tower::ServiceExt;

const MIGRATOR_URL: &str =
    "postgres://agentic_memory_migrator:synthetic-migrator-only@127.0.0.1:55432/agentic_memory";
const RUNTIME_URL: &str =
    "postgres://agentic_memory_runtime:synthetic-runtime-only@127.0.0.1:55432/agentic_memory";
const WORKER_URL: &str = "postgres://agentic_memory_purge_worker:synthetic-purge-worker-only@127.0.0.1:55432/agentic_memory";
const TENANT: &str = "00000000000000000000000000000001";
const APP: &str = "a0000000000000000000000000000001";
const PRINCIPAL: &str = "10000000000000000000000000000001";
const PRIVATE_COLLECTION: &str = "30000000000000000000000000000004";
const READER_ID: &str = "c0000000000000000000000000000099";
const TOKEN: &str = "n3-reader-token-000000000000000000";

fn test_database_url(url: &str) -> String {
    match option_env!("MEMORY_TEST_PG_PORT") {
        None | Some("55432") => url.to_owned(),
        Some("55433") => url.replace("127.0.0.1:55432", "127.0.0.1:55433"),
        Some(_) => panic!("unsupported synthetic test database port"),
    }
}

async fn pools() -> (PgPool, PgPool, PgPool) {
    let migrator = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url(MIGRATOR_URL))
        .await
        .unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .unwrap();
    let worker = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url(WORKER_URL))
        .await
        .unwrap();
    let mut tx = migrator.begin().await.unwrap();
    sqlx::raw_sql(include_str!("fixtures/reset.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO credentials (tenant_id,id,principal_id,app_id,token_digest,credential_class,allowed_operations,issued_at,expires_at) VALUES ($1,$2,$3,$4,$5,'agent_reader',ARRAY['search'],clock_timestamp(),clock_timestamp()+interval '1 hour')")
        .bind(TENANT).bind(READER_ID).bind(PRINCIPAL).bind(APP)
        .bind(Sha256::digest(TOKEN.as_bytes()).as_slice()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    (migrator, runtime, worker)
}

async fn seed_memory(pool: &PgPool, item: &str, revision: &str, content: &str) {
    sqlx::query(
        "INSERT INTO items (tenant_id,id,collection_id,active_revision_id) VALUES ($1,$2,$3,NULL)",
    )
    .bind(TENANT)
    .bind(item)
    .bind(PRIVATE_COLLECTION)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,$2,$3,$4)")
        .bind(TENANT)
        .bind(item)
        .bind(revision)
        .bind(content)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT)
        .bind(item)
        .bind(revision)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document) VALUES ($1,$2,$3,to_tsvector('simple',$4))")
        .bind(TENANT).bind(item).bind(revision).bind(content).execute(pool).await.unwrap();
}

async fn select_generation(pool: &PgPool, generation: &str, model: &str, recipe: &str) {
    sqlx::query("INSERT INTO embedding_generations (tenant_id,id,model_version,input_recipe_version,input_recipe,dimensions) VALUES ($1,$2,$3,$4,'trimmed UTF-8 body prefixed by recipe version',3)")
        .bind(TENANT).bind(generation).bind(model).bind(recipe).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO active_embedding_generations (tenant_id,app_id,generation_id) VALUES ($1,$2,$3) ON CONFLICT (tenant_id,app_id) DO UPDATE SET generation_id=excluded.generation_id,selected_at=clock_timestamp()")
        .bind(TENANT).bind(APP).bind(generation).execute(pool).await.unwrap();
}

async fn select_generation_with_dimensions(
    pool: &PgPool,
    generation: &str,
    model: &str,
    recipe: &str,
    dimensions: i32,
) {
    sqlx::query("INSERT INTO embedding_generations (tenant_id,id,model_version,input_recipe_version,input_recipe,dimensions) VALUES ($1,$2,$3,$4,'externally computed synthetic vector',$5)")
        .bind(TENANT).bind(generation).bind(model).bind(recipe).bind(dimensions)
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO active_embedding_generations (tenant_id,app_id,generation_id) VALUES ($1,$2,$3) ON CONFLICT (tenant_id,app_id) DO UPDATE SET generation_id=excluded.generation_id,selected_at=clock_timestamp()")
        .bind(TENANT).bind(APP).bind(generation).execute(pool).await.unwrap();
}

async fn claim_one(worker: &PgPool) -> sqlx::postgres::PgRow {
    sqlx::query("SELECT * FROM claim_embedding_jobs(1,60)")
        .fetch_one(worker)
        .await
        .unwrap()
}

async fn complete(worker: &PgPool, row: &sqlx::postgres::PgRow, vector: &str) -> String {
    sqlx::query_scalar("SELECT complete_embedding_job($1,$2,$3,$4)")
        .bind(row.get::<String, _>("tenant_id"))
        .bind(row.get::<String, _>("job_id"))
        .bind(row.get::<i64, _>("attempt"))
        .bind(vector)
        .fetch_one(worker)
        .await
        .unwrap()
}

async fn complete_all(worker: &PgPool, vector: &str) {
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            assert_eq!(complete(worker, &job, vector).await, "complete");
        }
    }
}

async fn search_tx(pool: &PgPool, sql: &'static str) -> Vec<sqlx::postgres::PgRow> {
    let mut tx = pool.begin().await.unwrap();
    let digest = Sha256::digest(TOKEN.as_bytes());
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&digest)).bind(TENANT).execute(&mut *tx).await.unwrap();
    let rows = sqlx::query(sql).fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rows
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut value, byte| {
        write!(value, "{byte:02x}").unwrap();
        value
    })
}

fn deterministic_vector(query: &str) -> String {
    let digest = Sha256::digest(query.as_bytes());
    format!(
        "[{},{},{}]",
        u16::from(digest[0]) + 1,
        u16::from(digest[1]) + 1,
        u16::from(digest[2]) + 1
    )
}

async fn http_search(pool: &PgPool, query: &str) -> (StatusCode, Value) {
    http_search_with_limit(pool, query, 4096).await
}

async fn http_search_with_limit(
    pool: &PgPool,
    query: &str,
    max_context_bytes: u16,
) -> (StatusCode, Value) {
    http_search_with_scope(pool, query, max_context_bytes, None).await
}

async fn http_search_with_scope(
    pool: &PgPool,
    query: &str,
    max_context_bytes: u16,
    subject: Option<&str>,
) -> (StatusCode, Value) {
    let mut input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":max_context_bytes
    });
    if let Some(subject) = subject {
        input["scope"] = serde_json::json!({"subjects":[subject]});
    }
    let response = router(pool.clone())
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn http_search_with_external_vector(
    pool: &PgPool,
    query: &str,
    generation: &str,
    model: &str,
    recipe: &str,
    vector: &[f64],
) -> (StatusCode, Value) {
    http_search_with_external_value(
        pool,
        query,
        serde_json::json!({
            "generation_id":generation,
            "model_version":model,
            "input_recipe_version":recipe,
            "dimensions":vector.len(),
            "original_query_sha256":hex(&Sha256::digest(query.as_bytes())),
            "vector":vector
        }),
    )
    .await
}

async fn http_search_with_external_value(
    pool: &PgPool,
    query: &str,
    semantic_query: Value,
) -> (StatusCode, Value) {
    let response = router(pool.clone())
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "query":query,
                        "semantic":true,
                        "semantic_query":semantic_query,
                        "max_context_bytes":4096
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[allow(clippy::too_many_lines)] // One E2E keeps admission, fallback and replacement identities together.
async fn external_vectors_are_generation_bound_and_provider_neutral() {
    let (db, runtime, worker) = pools().await;
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,'n3_external_item',$2)")
        .bind(TENANT)
        .bind(PRIVATE_COLLECTION)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_external_item','n3_external_revision','external document shell')")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_external_revision' WHERE tenant_id=$1 AND id='n3_external_item'")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document) VALUES ($1,'n3_external_item','n3_external_revision',to_tsvector('simple','external document shell'))")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO source_revisions (tenant_id,item_id,revision_id,id,source_sha256) VALUES ($1,'n3_external_item','n3_external_revision','n3_external_source',digest('external source','sha256'))")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("SELECT activate_document_extraction($1,'n3_external_item','n3_external_revision','n3_external_source',digest('external source','sha256'),'n3_external_set','fixture-parser','v1','external-config',ARRAY['external_passage'],ARRAY['root'],ARRAY['none'],ARRAY['{\"page\":7}'],ARRAY['cited external semantic payload'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    let query = "lexically absent lookup phrase";

    for (generation, model, recipe, dimensions) in [
        (
            "external_g1536",
            "synthetic-model-a",
            "synthetic-recipe-a",
            1536,
        ),
        (
            "external_g768",
            "synthetic-model-b",
            "synthetic-recipe-b",
            768,
        ),
    ] {
        let mut vector = vec![0.0; dimensions];
        vector[0] = 1.0;
        select_generation_with_dimensions(
            &db,
            generation,
            model,
            recipe,
            i32::try_from(vector.len()).unwrap(),
        )
        .await;
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        assert!(!jobs.is_empty());
        if dimensions == 1536 {
            let immutable = sqlx::query(
                "UPDATE embedding_jobs SET dimensions=768 WHERE tenant_id=$1 AND id=$2",
            )
            .bind(TENANT)
            .bind(jobs[0].get::<String, _>("job_id"))
            .execute(&db)
            .await
            .expect_err("leased job generation identity must be immutable");
            assert!(
                immutable
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::code)
                    .is_some_and(|code| code == "55000")
            );
            for component in [3.0e38_f64, 1.0e-30_f64] {
                let mut unsafe_vector = vec![0.0; dimensions];
                unsafe_vector[0] = component;
                unsafe_vector[1] = component;
                let rejected =
                    sqlx::query_scalar::<_, String>("SELECT complete_embedding_job($1,$2,$3,$4)")
                        .bind(TENANT)
                        .bind(jobs[0].get::<String, _>("job_id"))
                        .bind(jobs[0].get::<i64, _>("attempt"))
                        .bind(serde_json::to_string(&unsafe_vector).unwrap())
                        .fetch_one(&worker)
                        .await
                        .expect_err(
                            "finite vector with undefined pgvector cosine must be rejected",
                        );
                assert!(
                    rejected
                        .as_database_error()
                        .and_then(sqlx::error::DatabaseError::code)
                        .is_some_and(|code| code.starts_with("22"))
                );
            }
        }
        for job in &jobs {
            let stored = if job.get::<String, _>("item_id") == "n3_external_item" {
                vector.clone()
            } else {
                let mut other = vec![0.0; vector.len()];
                other[1] = 1.0;
                other
            };
            assert_eq!(
                complete(&worker, job, &serde_json::to_string(&stored).unwrap()).await,
                "complete"
            );
        }
        let response =
            http_search_with_external_vector(&runtime, query, generation, model, recipe, &vector)
                .await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1["items"][0]["item_id"], "n3_external_item");
        assert_eq!(
            response.1["items"][0]["excerpt"],
            "cited external semantic payload"
        );
        assert_eq!(response.1["items"][0]["reason"], "semantic");
        assert_eq!(
            response.1["items"][0]["citation"]["passage_id"],
            "external_passage"
        );

        if dimensions == 1536 {
            let fallback_query = "cited external semantic payload";
            let base = serde_json::json!({
                "generation_id":generation,
                "model_version":model,
                "input_recipe_version":recipe,
                "dimensions":dimensions,
                "original_query_sha256":hex(&Sha256::digest(fallback_query.as_bytes())),
                "vector":vector
            });
            let mut invalid = Vec::new();
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove("dimensions");
            invalid.push(value);
            for (field, replacement) in [
                ("dimensions", serde_json::json!(1535)),
                ("original_query_sha256", serde_json::json!("0".repeat(64))),
                ("original_query_sha256", serde_json::json!("A".repeat(64))),
                ("generation_id", serde_json::json!("wrong_generation")),
                ("model_version", serde_json::json!("wrong-model")),
                ("input_recipe_version", serde_json::json!("wrong-recipe")),
                ("vector", serde_json::json!(vec![0.0; 1536])),
                ("vector", {
                    let mut short = vec![0.0; 1535];
                    short[0] = 1.0;
                    serde_json::json!(short)
                }),
            ] {
                let mut value = base.clone();
                value[field] = replacement;
                invalid.push(value);
            }
            for value in invalid {
                let fallback =
                    http_search_with_external_value(&runtime, fallback_query, value).await;
                assert_eq!(fallback.0, StatusCode::OK);
                assert_eq!(fallback.1["items"][0]["item_id"], "n3_external_item");
                assert_eq!(fallback.1["items"][0]["semantic_status"], "pending");
                assert_eq!(
                    fallback.1["warnings"],
                    serde_json::json!(["lexical_ready", "semantic_external_input_invalid"])
                );
            }
            for component in [3.0e38_f64, 1.0e-30_f64] {
                let mut value = base.clone();
                let mut unsafe_vector = vec![0.0; 1536];
                unsafe_vector[0] = component;
                unsafe_vector[1] = component;
                value["vector"] = serde_json::json!(unsafe_vector);
                let fallback =
                    http_search_with_external_value(&runtime, fallback_query, value).await;
                assert_eq!(fallback.0, StatusCode::OK);
                assert_eq!(fallback.1["items"][0]["item_id"], "n3_external_item");
                assert_eq!(fallback.1["items"][0]["semantic_status"], "pending");
                assert_eq!(
                    fallback.1["warnings"],
                    serde_json::json!(["lexical_ready", "semantic_unavailable"])
                );
            }
        }
    }

    let stale_generation = http_search_with_external_vector(
        &runtime,
        query,
        "external_g1536",
        "synthetic-model-a",
        "synthetic-recipe-a",
        &{
            let mut vector = vec![0.0; 1536];
            vector[0] = 1.0;
            vector
        },
    )
    .await;
    assert_eq!(stale_generation.0, StatusCode::OK);
    assert!(stale_generation.1["items"].as_array().unwrap().is_empty());
    assert_eq!(
        stale_generation.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_external_input_invalid"])
    );

    let valid_body = serde_json::json!({
        "query":query,"semantic":true,"semantic_query":{
            "generation_id":"external_g768","model_version":"synthetic-model-b",
            "input_recipe_version":"synthetic-recipe-b","dimensions":768,
            "original_query_sha256":hex(&Sha256::digest(query.as_bytes())),
            "vector":vec![1.0;768]
        },"max_context_bytes":4096
    })
    .to_string();
    let boundary = format!("{valid_body}{}", " ".repeat(128 * 1024 - valid_body.len()));
    for (body, expected_status) in [
        (boundary.clone(), StatusCode::OK),
        (format!("{boundary} "), StatusCode::BAD_REQUEST),
    ] {
        let response = router(runtime.clone())
            .oneshot(
                Request::post("/v1/search")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected_status);
    }
}

#[allow(clippy::too_many_lines)] // One public degradation tracer keeps the ordered lane transitions visible.
async fn native_search_degrades_to_lexical_with_sanitized_diagnostics() {
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_empty",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    let no_match = http_search(&runtime, "genuine no match").await;
    assert_eq!(no_match.0, StatusCode::OK);
    assert!(no_match.1["items"].as_array().unwrap().is_empty());
    assert_eq!(
        no_match.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_ready", "no_match"])
    );
    sqlx::query("DELETE FROM active_embedding_generations WHERE tenant_id=$1 AND app_id=$2")
        .bind(TENANT)
        .bind(APP)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM embedding_generations WHERE tenant_id=$1 AND id='deterministic_empty'",
    )
    .bind(TENANT)
    .execute(&db)
    .await
    .unwrap();

    seed_memory(
        &db,
        "n3_native_item",
        "n3_native_revision",
        "native lexical fallback beacon",
    )
    .await;
    let missing = http_search(&runtime, "lexical fallback").await;
    assert_eq!(missing.0, StatusCode::OK);
    assert_eq!(missing.1["items"][0]["semantic_status"], "pending");
    assert_eq!(
        missing.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_configuration_missing"])
    );
    let mut unauthorized = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind("00".repeat(32)).bind(TENANT).execute(&mut *unauthorized).await.unwrap();
    let denied = sqlx::query("SELECT * FROM semantic_readiness($1,$2,NULL)")
        .bind(TENANT)
        .bind(READER_ID)
        .fetch_all(&mut *unauthorized)
        .await
        .expect_err("readiness cannot probe through another credential identity");
    assert_eq!(
        denied
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
    unauthorized.rollback().await.unwrap();

    select_generation(
        &db,
        "deterministic_native",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    let immutable = sqlx::query("UPDATE embedding_generations SET model_version='replacement-in-place' WHERE tenant_id=$1 AND id='deterministic_native'")
        .bind(TENANT)
        .execute(&db)
        .await
        .expect_err("model or recipe replacement requires a new generation");
    assert_eq!(
        immutable
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("55000")
    );
    let pending = http_search(&runtime, "lexical fallback").await;
    assert_eq!(pending.0, StatusCode::OK);
    assert_eq!(pending.1["warnings"][1], "semantic_processing_pending");

    let job = claim_one(&worker).await;
    assert_eq!(
        complete(&worker, &job, &deterministic_vector("lexical fallback")).await,
        "complete"
    );
    let ready = http_search(&runtime, "lexical fallback").await;
    assert_eq!(ready.0, StatusCode::OK);
    assert_eq!(
        ready.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_ready"])
    );
    assert_eq!(ready.1["items"][0]["semantic_status"], "ready");

    sqlx::query("INSERT INTO apps (tenant_id,id,active) VALUES ($1,'n3_other_app',true)")
        .bind(TENANT)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO collections (tenant_id,id,app_id,audience_kind,owner_principal_id) VALUES ($1,'n3_other_collection','n3_other_app','private',$2)")
        .bind(TENANT).bind(PRINCIPAL).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id,active_revision_id) VALUES ($1,'n3_other_item','n3_other_collection',NULL)")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_other_item','n3_other_revision','other app failure must not alter readiness')")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_other_revision' WHERE tenant_id=$1 AND id='n3_other_item'")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO lexical_representations VALUES ($1,'n3_other_item','n3_other_revision',to_tsvector('simple','other app failure'))")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO active_embedding_generations (tenant_id,app_id,generation_id) VALUES ($1,'n3_other_app','deterministic_native')")
        .bind(TENANT).execute(&db).await.unwrap();
    let other_app_job = claim_one(&worker).await;
    sqlx::query("UPDATE embedding_jobs SET max_attempts=attempt WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT)
        .bind(other_app_job.get::<String, _>("job_id"))
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT fail_embedding_job($1,$2,$3,'provider_failed')")
            .bind(TENANT)
            .bind(other_app_job.get::<String, _>("job_id"))
            .bind(other_app_job.get::<i64, _>("attempt"))
            .fetch_one(&worker)
            .await
            .unwrap(),
        "failed"
    );
    let still_ready = http_search(&runtime, "lexical fallback").await;
    assert_eq!(
        still_ready.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_ready"]),
        "same-principal work from another app cannot affect readiness"
    );

    seed_memory(
        &db,
        "n3_failed_item",
        "n3_failed_revision",
        "provider failure leaves lexical ready",
    )
    .await;
    let failed_job = claim_one(&worker).await;
    sqlx::query("UPDATE embedding_jobs SET max_attempts=attempt WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT)
        .bind(failed_job.get::<String, _>("job_id"))
        .execute(&db)
        .await
        .unwrap();
    let failure: String =
        sqlx::query_scalar("SELECT fail_embedding_job($1,$2,$3,'provider_failed')")
            .bind(TENANT)
            .bind(failed_job.get::<String, _>("job_id"))
            .bind(failed_job.get::<i64, _>("attempt"))
            .fetch_one(&worker)
            .await
            .unwrap();
    assert_eq!(failure, "failed");
    let failed = http_search(&runtime, "provider failure").await;
    assert_eq!(failed.0, StatusCode::OK);
    assert_eq!(failed.1["warnings"][1], "semantic_processing_failed");

    for (suffix, model, diagnostic) in [
        (
            "query_fail",
            "deterministic-fixture-provider-failed",
            "query_embedding_failed",
        ),
        (
            "query_timeout",
            "deterministic-fixture-provider-timeout",
            "query_embedding_timeout",
        ),
        (
            "query_invalid",
            "deterministic-fixture-invalid-response",
            "query_embedding_invalid",
        ),
    ] {
        select_generation(
            &db,
            &format!("deterministic_{suffix}"),
            model,
            "deterministic-input-v1",
        )
        .await;
        complete_all(&worker, "[1,0,0]").await;
        let degraded = http_search(&runtime, "provider failure").await;
        assert_eq!(degraded.0, StatusCode::OK);
        assert_eq!(
            degraded.1["warnings"],
            serde_json::json!(["lexical_ready", diagnostic])
        );
        assert_eq!(degraded.1["items"][0]["semantic_status"], "failed");
    }
    select_generation(
        &db,
        "unapproved_real_generation",
        "unapproved-real-model",
        "deterministic-input-v1",
    )
    .await;
    complete_all(&worker, "[1,0,0]").await;
    let unapproved = http_search(&runtime, "provider failure").await;
    assert_eq!(unapproved.0, StatusCode::OK);
    assert_eq!(
        unapproved.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_configuration_missing"])
    );

    let mut semantic_lock = db.begin().await.unwrap();
    sqlx::query("LOCK active_embedding_generations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *semantic_lock)
        .await
        .unwrap();
    let timed_out = http_search(&runtime, "provider failure").await;
    assert_eq!(timed_out.0, StatusCode::OK);
    assert_eq!(timed_out.1["warnings"][1], "semantic_database_unavailable");
    semantic_lock.rollback().await.unwrap();

    let mut all_lanes_lock = db.begin().await.unwrap();
    sqlx::query(
        "LOCK active_embedding_generations, lexical_representations IN ACCESS EXCLUSIVE MODE",
    )
    .execute(&mut *all_lanes_lock)
    .await
    .unwrap();
    let unavailable = http_search(&runtime, "provider failure").await;
    assert_eq!(unavailable.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unavailable.1["status"], "storage_unavailable");
    assert!(unavailable.1.get("items").is_none());
    all_lanes_lock.rollback().await.unwrap();
}

async fn document_vectors_activate_only_as_a_complete_current_set() {
    let (db, runtime, worker) = pools().await;
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,'n3_doc_item',$2)")
        .bind(TENANT)
        .bind(PRIVATE_COLLECTION)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_doc_item','n3_doc_revision','document shell')")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_doc_revision' WHERE tenant_id=$1 AND id='n3_doc_item'")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO source_revisions (tenant_id,item_id,revision_id,id,source_sha256) VALUES ($1,'n3_doc_item','n3_doc_revision','n3_source',digest('n3 source','sha256'))")
        .bind(TENANT).execute(&db).await.unwrap();
    select_generation(
        &db,
        "deterministic_doc",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    sqlx::query("SELECT activate_document_extraction($1,'n3_doc_item','n3_doc_revision','n3_source',digest('n3 source','sha256'),'n3_set_1','fixture-parser','v1','cfg1',ARRAY['p1','p2'],ARRAY['root','root'],ARRAY['none','none'],ARRAY['{\"page\":1}','{\"page\":2}'],ARRAY['first semantic passage','second semantic passage'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    let first = claim_one(&worker).await;
    assert_eq!(complete(&worker, &first, "[1,0,0]").await, "complete");
    let partial=search_tx(&runtime,"SELECT * FROM search_exact_semantic_candidates('00000000000000000000000000000001','c0000000000000000000000000000099','deterministic_doc','[1,0,0]',NULL,10)").await;
    assert!(
        partial.is_empty(),
        "a partial active extraction set is not semantically visible"
    );
    let second = claim_one(&worker).await;
    assert_eq!(complete(&worker, &second, "[1,0,0]").await, "complete");
    let complete_set=search_tx(&runtime,"SELECT * FROM search_exact_semantic_candidates('00000000000000000000000000000001','c0000000000000000000000000000099','deterministic_doc','[1,0,0]',NULL,10)").await;
    assert_eq!(complete_set.len(), 2);

    sqlx::query("SELECT activate_document_extraction($1,'n3_doc_item','n3_doc_revision','n3_source',digest('n3 source','sha256'),'n3_set_2','fixture-parser','v1','cfg2',ARRAY['p3'],ARRAY['root'],ARRAY['none'],ARRAY['{\"page\":3}'],ARRAY['replacement semantic passage'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    let replacement_gap=search_tx(&runtime,"SELECT * FROM search_exact_semantic_candidates('00000000000000000000000000000001','c0000000000000000000000000000099','deterministic_doc','[1,0,0]',NULL,10)").await;
    assert!(
        replacement_gap.is_empty(),
        "old-set vectors cannot bridge replacement backfill"
    );
    let stale = claim_one(&worker).await;
    sqlx::query("SELECT activate_document_extraction($1,'n3_doc_item','n3_doc_revision','n3_source',digest('n3 source','sha256'),'n3_set_3','fixture-parser','v1','cfg3',ARRAY['p4'],ARRAY['root'],ARRAY['none'],ARRAY['{\"page\":4}'],ARRAY['newest semantic passage'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    assert_eq!(complete(&worker, &stale, "[1,0,0]").await, "stopped_stale");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM embedding_representations WHERE extraction_set_id='n3_set_2'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );

    sqlx::query("SELECT activate_document_extraction($1,'n3_doc_item','n3_doc_revision','n3_source',digest('n3 source','sha256'),'n3_set_edge','fixture-parser','v1','cfge',ARRAY['edge_parent','edge_child'],ARRAY['edge_span','edge_span'],ARRAY['to_next','from_previous'],ARRAY['{\"page\":10}','{\"page\":11}'],ARRAY['PARENTKEY direct passage','semantic continuation passage'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    let edge_jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(&worker)
        .await
        .unwrap();
    assert_eq!(edge_jobs.len(), 2);
    for job in &edge_jobs {
        let vector = if job.get::<String, _>("passage_id") == "edge_child" {
            "[1,0,0]"
        } else {
            "[0.99,0.01,0]"
        };
        assert_eq!(complete(&worker, job, vector).await, "complete");
    }
    let direct_first=search_tx(&runtime,"SELECT * FROM search_hybrid_current_memories('00000000000000000000000000000001','c0000000000000000000000000000099','00000000-0000-0000-0000-000000000097','PARENTKEY',4096,NULL,'deterministic_doc','[1,0,0]')").await;
    assert_eq!(direct_first.len(), 2);
    assert_eq!(
        direct_first[0].get::<String, _>("passage_id"),
        "edge_parent"
    );
    assert_eq!(direct_first[0].get::<String, _>("hit_kind"), "passage");
    assert_eq!(direct_first[1].get::<String, _>("passage_id"), "edge_child");
    assert_eq!(
        direct_first[1].get::<String, _>("hit_kind"),
        "semantic_passage",
        "a semantic match is direct evidence, not a discardable continuation"
    );

    let cap_query = "semantic cap probe";
    sqlx::query("SELECT activate_document_extraction($1,'n3_doc_item','n3_doc_revision','n3_source',digest('n3 source','sha256'),'n3_set_4','fixture-parser','v1','cfg4',ARRAY['p5','p6','p7','p8','p9'],ARRAY['root','root','root','root','root'],ARRAY['none','none','none','none','none'],ARRAY['{\"page\":5}','{\"page\":6}','{\"page\":7}','{\"page\":8}','{\"page\":9}'],ARRAY['cap passage five','cap passage six','cap passage seven','cap passage eight','cap passage nine'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    complete_all(&worker, &deterministic_vector(cap_query)).await;
    let capped = http_search(&runtime, cap_query).await;
    assert_eq!(capped.0, StatusCode::OK);
    assert_eq!(capped.1["items"].as_array().unwrap().len(), 4);
    assert_eq!(capped.1["truncated"], true);
    assert!(capped.1["items"].as_array().unwrap().iter().all(|item| {
        item["reason"] == "semantic"
            && item["citation"]["source_revision_id"] == "n3_source"
            && item["citation"]["extraction_set_id"] == "n3_set_4"
    }));
}

#[allow(clippy::too_many_lines)] // One deterministic job/search tracer covers the shared generation invariant.
async fn deterministic_jobs_are_fenced_and_hybrid_search_is_authorized() {
    let (db, runtime, worker) = pools().await;
    seed_memory(
        &db,
        "n3_semantic_item",
        "n3_semantic_revision",
        "semantic-only zephyr fact",
    )
    .await;
    seed_memory(
        &db,
        "n3_lexical_item",
        "n3_lexical_revision",
        "lexical beacon answer",
    )
    .await;
    sqlx::query("INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document) VALUES ($1,'40000000000000000000000000000002','50000000000000000000000000000002',to_tsvector('simple','closer forbidden semantic evidence'))")
        .bind(TENANT).execute(&db).await.unwrap();

    // Selecting a generation backfills all already-current private lexical inputs atomically.
    select_generation(
        &db,
        "deterministic_g1",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    let null_claim = sqlx::query("SELECT * FROM claim_embedding_jobs(NULL::integer,60)")
        .fetch_all(&worker)
        .await
        .expect_err("restricted worker cannot turn a NULL claim bound into an unbounded claim");
    assert_eq!(
        null_claim
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );
    let mut null_search = runtime.begin().await.unwrap();
    let digest = Sha256::digest(TOKEN.as_bytes());
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&digest)).bind(TENANT).execute(&mut *null_search).await.unwrap();
    let null_limit = sqlx::query("SELECT * FROM search_exact_semantic_candidates($1,$2,'deterministic_g1','[1,0,0]',NULL,NULL::integer)")
        .bind(TENANT).bind(READER_ID).fetch_all(&mut *null_search).await
        .expect_err("restricted runtime cannot turn a NULL exact-search bound into an unbounded scan");
    assert_eq!(
        null_limit
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );
    null_search.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM embedding_jobs WHERE generation_id='deterministic_g1'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        3
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM embedding_jobs j JOIN revisions r ON r.tenant_id=j.tenant_id AND r.item_id=j.item_id AND r.id=j.revision_id WHERE j.input_digest=digest(j.input_recipe_version || E'\\n' || r.content,'sha256')").fetch_one(&db).await.unwrap(),3);

    let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(&worker)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 3);
    let first = &jobs[0];
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT complete_embedding_job($1,$2,NULL::bigint,'[1,0,0]')",
        )
        .bind(TENANT)
        .bind(first.get::<String, _>("job_id"))
        .fetch_one(&worker)
        .await
        .unwrap(),
        "stopped_stale"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT fail_embedding_job($1,$2,NULL::bigint,'provider_failed')",
        )
        .bind(TENANT)
        .bind(first.get::<String, _>("job_id"))
        .fetch_one(&worker)
        .await
        .unwrap(),
        "stopped_stale"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM embedding_jobs WHERE tenant_id=$1 AND id=$2",
        )
        .bind(TENANT)
        .bind(first.get::<String, _>("job_id"))
        .fetch_one(&db)
        .await
        .unwrap(),
        "leased"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM embedding_representations WHERE tenant_id=$1 AND job_id=$2",
        )
        .bind(TENANT)
        .bind(first.get::<String, _>("job_id"))
        .fetch_one(&db)
        .await
        .unwrap(),
        0,
        "NULL attempts neither mutate the lease nor create a representation"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM claim_embedding_jobs(1,60)")
            .fetch_one(&worker)
            .await
            .unwrap(),
        0
    );

    let wrong_dimension =
        sqlx::query_scalar::<_, String>("SELECT complete_embedding_job($1,$2,$3,'[1,0]')")
            .bind(TENANT)
            .bind(first.get::<String, _>("job_id"))
            .bind(first.get::<i64, _>("attempt"))
            .fetch_one(&worker)
            .await
            .expect_err("wrong dimensions rejected");
    assert!(
        wrong_dimension
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .is_some_and(|code| code.starts_with("22"))
    );
    let non_finite =
        sqlx::query_scalar::<_, String>("SELECT complete_embedding_job($1,$2,$3,'[NaN,0,0]')")
            .bind(TENANT)
            .bind(first.get::<String, _>("job_id"))
            .bind(first.get::<i64, _>("attempt"))
            .fetch_one(&worker)
            .await
            .expect_err("non-finite vectors rejected");
    assert!(
        non_finite
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .is_some_and(|code| code.starts_with("22"))
    );

    for job in &jobs {
        let vector = match job.get::<String, _>("item_id").as_str() {
            "40000000000000000000000000000002" => "[1,0,0]",
            "n3_semantic_item" => "[0.99,0.01,0]",
            _ => "[0,1,0]",
        };
        assert_eq!(complete(&worker, job, vector).await, "complete");
    }

    let hybrid = search_tx(&runtime,"SELECT * FROM search_hybrid_current_memories('00000000000000000000000000000001','c0000000000000000000000000000099','00000000-0000-0000-0000-000000000099','lexical beacon',4096,NULL,'deterministic_g1','[1,0,0]')").await;
    assert_eq!(hybrid.len(), 2);
    assert_eq!(hybrid[0].get::<String, _>("item_id"), "n3_lexical_item");
    assert!(
        hybrid
            .iter()
            .any(|row| row.get::<String, _>("item_id") == "n3_semantic_item")
    );
    assert!(
        !hybrid
            .iter()
            .any(|row| row.get::<String, _>("item_id") == "40000000000000000000000000000002"),
        "closer Bob-private semantic evidence must be filtered in DB"
    );

    // A different generation cannot be selected or compared through the old generation.
    select_generation(
        &db,
        "deterministic_g2",
        "deterministic-fixture-v2",
        "deterministic-input-v2",
    )
    .await;
    let mut tx = runtime.begin().await.unwrap();
    let digest = Sha256::digest(TOKEN.as_bytes());
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&digest)).bind(TENANT).execute(&mut *tx).await.unwrap();
    sqlx::query("SELECT * FROM search_exact_semantic_candidates($1,$2,'deterministic_g1','[1,0,0]',NULL,10)")
        .bind(TENANT).bind(READER_ID).fetch_all(&mut *tx).await
        .expect_err("old generation must be rejected before candidates leave DB");
    tx.rollback().await.unwrap();
}

#[allow(clippy::too_many_lines)] // One ordered race tracer avoids fixture reset races across lifecycle cases.
async fn crash_retry_and_lifecycle_changes_cannot_activate_stale_evidence() {
    let (db, runtime, worker) = pools().await;
    seed_memory(
        &db,
        "n3_race_item",
        "n3_race_revision",
        "lexical remains usable during semantic work",
    )
    .await;
    select_generation(
        &db,
        "deterministic_race",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    let first = claim_one(&worker).await;

    // Crash/replay: only the reclaimed fenced attempt can commit.
    sqlx::query("UPDATE embedding_jobs SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT).bind(first.get::<String,_>("job_id")).execute(&db).await.unwrap();
    let replay = claim_one(&worker).await;
    assert_eq!(
        replay.get::<String, _>("job_id"),
        first.get::<String, _>("job_id")
    );
    assert_eq!(replay.get::<i64, _>("attempt"), 2);
    assert_eq!(complete(&worker, &first, "[1,0,0]").await, "stopped_stale");

    // A correction queues its exact new generation; the late old attempt cannot mark it ready.
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_race_item','n3_race_revision_2','corrected lexical remains usable')")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_race_revision_2' WHERE tenant_id=$1 AND id='n3_race_item'")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO lexical_representations VALUES ($1,'n3_race_item','n3_race_revision_2',to_tsvector('simple','corrected lexical remains usable'))")
        .bind(TENANT).execute(&db).await.unwrap();
    assert_eq!(complete(&worker, &replay, "[1,0,0]").await, "stopped_stale");
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM embedding_representations WHERE item_id='n3_race_item' AND revision_id='n3_race_revision'").fetch_one(&db).await.unwrap(),0);

    let mut current = claim_one(&worker).await;
    for expected in ["retry", "retry", "failed"] {
        let result: String =
            sqlx::query_scalar("SELECT fail_embedding_job($1,$2,$3,'provider_timeout')")
                .bind(TENANT)
                .bind(current.get::<String, _>("job_id"))
                .bind(current.get::<i64, _>("attempt"))
                .fetch_one(&worker)
                .await
                .unwrap();
        assert_eq!(result, expected);
        if expected == "retry" {
            sqlx::query("UPDATE embedding_jobs SET available_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2")
                .bind(TENANT).bind(current.get::<String,_>("job_id")).execute(&db).await.unwrap();
            current = claim_one(&worker).await;
        }
    }
    let lexical=search_tx(&runtime,"SELECT * FROM search_current_memories('00000000000000000000000000000001','c0000000000000000000000000000099','00000000-0000-0000-0000-000000000098','corrected lexical',4096,NULL)").await;
    assert_eq!(
        lexical.len(),
        1,
        "lexical fallback remains independently usable"
    );

    // Withdrawal, revoke and forget all fence later processing without content leakage.
    for action in ["withdraw", "revoke", "forget"] {
        seed_memory(
            &db,
            &format!("n3_{action}_item"),
            &format!("n3_{action}_revision"),
            "pending semantic lifecycle",
        )
        .await;
        let job = claim_one(&worker).await;
        match action {
            "withdraw" => {
                sqlx::query("UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2").bind(TENANT).bind(PRIVATE_COLLECTION).execute(&db).await.unwrap();
            }
            "revoke" => {
                sqlx::query("UPDATE principals SET active=false WHERE tenant_id=$1 AND id=$2")
                    .bind(TENANT)
                    .bind(PRINCIPAL)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query("UPDATE items SET active_revision_id=NULL,deleted_at=clock_timestamp(),deletion_generation=deletion_generation+1 WHERE tenant_id=$1 AND id=$2").bind(TENANT).bind(format!("n3_{action}_item")).execute(&db).await.unwrap();
            }
        }
        assert_eq!(complete(&worker, &job, "[1,0,0]").await, "stopped_stale");
        sqlx::query("UPDATE collections SET withdrawn_at=NULL WHERE tenant_id=$1 AND id=$2")
            .bind(TENANT)
            .bind(PRIVATE_COLLECTION)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("UPDATE principals SET active=true WHERE tenant_id=$1 AND id=$2")
            .bind(TENANT)
            .bind(PRINCIPAL)
            .execute(&db)
            .await
            .unwrap();
    }
    seed_memory(
        &db,
        "n3_cancel_item",
        "n3_cancel_revision",
        "cancelled semantic work",
    )
    .await;
    let expired = claim_one(&worker).await;
    sqlx::query("UPDATE embedding_jobs SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT).bind(expired.get::<String,_>("job_id")).execute(&db).await.unwrap();
    let stale_cancel: String = sqlx::query_scalar("SELECT cancel_embedding_job($1,$2,$3)")
        .bind(TENANT)
        .bind(expired.get::<String, _>("job_id"))
        .bind(expired.get::<i64, _>("attempt"))
        .fetch_one(&worker)
        .await
        .unwrap();
    assert_eq!(stale_cancel, "stopped_stale");
    let reclaimed = claim_one(&worker).await;
    let cancelled: String = sqlx::query_scalar("SELECT cancel_embedding_job($1,$2,$3)")
        .bind(TENANT)
        .bind(reclaimed.get::<String, _>("job_id"))
        .bind(reclaimed.get::<i64, _>("attempt"))
        .fetch_one(&worker)
        .await
        .unwrap();
    assert_eq!(cancelled, "cancelled");
    let cancelled_readiness = http_search(&runtime, "cancelled semantic").await;
    assert_eq!(cancelled_readiness.0, StatusCode::OK);
    assert_eq!(
        cancelled_readiness.1["warnings"][1],
        "semantic_processing_failed"
    );
}

async fn claim_rechecks_authority_after_the_tenant_barrier() {
    let (db, _runtime, worker) = pools().await;
    seed_memory(
        &db,
        "n3_claim_race_item",
        "n3_claim_race_revision",
        "must not cross a concurrent revoke",
    )
    .await;
    select_generation(
        &db,
        "deterministic_claim_race",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;

    let mut revoke = db.begin().await.unwrap();
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(TENANT)
        .fetch_one(&mut *revoke)
        .await
        .unwrap();
    let claim_worker = worker.clone();
    let claimant = tokio::spawn(async move {
        sqlx::query("SELECT * FROM claim_embedding_jobs(1,60)")
            .fetch_all(&claim_worker)
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !claimant.is_finished(),
        "claim waits at the authority boundary"
    );
    sqlx::query("UPDATE principals SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT)
        .bind(PRINCIPAL)
        .execute(&mut *revoke)
        .await
        .unwrap();
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(TENANT)
        .execute(&mut *revoke)
        .await
        .unwrap();
    revoke.commit().await.unwrap();
    assert!(claimant.await.unwrap().is_empty());

    let mut restore = db.begin().await.unwrap();
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(TENANT)
        .fetch_one(&mut *restore)
        .await
        .unwrap();
    sqlx::query("UPDATE principals SET active=true WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT)
        .bind(PRINCIPAL)
        .execute(&mut *restore)
        .await
        .unwrap();
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(TENANT)
        .execute(&mut *restore)
        .await
        .unwrap();
    restore.commit().await.unwrap();
    seed_memory(
        &db,
        "n3_claim_positive_item",
        "n3_claim_positive_revision",
        "authorized claim positive",
    )
    .await;
    let positive = claim_one(&worker).await;
    assert_eq!(
        positive.get::<String, _>("content"),
        "authorized claim positive"
    );
}

async fn generation_selection_serializes_with_uncommitted_save_intent() {
    let (db, _runtime, _worker) = pools().await;
    let mut save = db.begin().await.unwrap();
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(TENANT)
        .fetch_one(&mut *save)
        .await
        .unwrap();
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id,active_revision_id) VALUES ($1,'n3_selection_race_item',$2,NULL)")
        .bind(TENANT).bind(PRIVATE_COLLECTION).execute(&mut *save).await.unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_selection_race_item','n3_selection_race_revision','uncommitted save intent')")
        .bind(TENANT).execute(&mut *save).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_selection_race_revision' WHERE tenant_id=$1 AND id='n3_selection_race_item'")
        .bind(TENANT).execute(&mut *save).await.unwrap();
    sqlx::query("INSERT INTO lexical_representations VALUES ($1,'n3_selection_race_item','n3_selection_race_revision',to_tsvector('simple','uncommitted save intent'))")
        .bind(TENANT).execute(&mut *save).await.unwrap();

    let selector = db.clone();
    let selection = tokio::spawn(async move {
        select_generation(
            &selector,
            "deterministic_selection_race",
            "deterministic-fixture-v1",
            "deterministic-input-v1",
        )
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !selection.is_finished(),
        "generation selection waits for the save authority boundary"
    );
    save.commit().await.unwrap();
    selection.await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM embedding_jobs WHERE tenant_id=$1 AND item_id='n3_selection_race_item' AND revision_id='n3_selection_race_revision' AND generation_id='deterministic_selection_race'")
            .bind(TENANT).fetch_one(&db).await.unwrap(),
        1,
        "backfill observes the committed lexical revision exactly once"
    );
}

async fn exhausted_leases_and_absolute_deadlines_terminalize() {
    let (db, _runtime, worker) = pools().await;
    seed_memory(
        &db,
        "n3_final_attempt_item",
        "n3_final_attempt_revision",
        "final attempt crash",
    )
    .await;
    select_generation(
        &db,
        "deterministic_deadlines",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    let final_attempt = claim_one(&worker).await;
    sqlx::query("UPDATE embedding_jobs SET max_attempts=attempt,lease_expires_at=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT).bind(final_attempt.get::<String,_>("job_id")).execute(&db).await.unwrap();
    assert!(
        sqlx::query("SELECT * FROM claim_embedding_jobs(1,60)")
            .fetch_all(&worker)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM embedding_jobs WHERE tenant_id=$1 AND id=$2"
        )
        .bind(TENANT)
        .bind(final_attempt.get::<String, _>("job_id"))
        .fetch_one(&db)
        .await
        .unwrap(),
        "failed"
    );

    seed_memory(
        &db,
        "n3_expired_pending_item",
        "n3_expired_pending_revision",
        "expired before content release",
    )
    .await;
    sqlx::query("UPDATE embedding_jobs SET deadline_at=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND item_id='n3_expired_pending_item'")
        .bind(TENANT).execute(&db).await.unwrap();
    assert!(
        sqlx::query("SELECT * FROM claim_embedding_jobs(1,60)")
            .fetch_all(&worker)
            .await
            .unwrap()
            .is_empty(),
        "expired pending input is terminalized before content release"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM embedding_jobs WHERE tenant_id=$1 AND item_id='n3_expired_pending_item'")
            .bind(TENANT).fetch_one(&db).await.unwrap(),
        "failed"
    );

    seed_memory(
        &db,
        "n3_expired_inflight_item",
        "n3_expired_inflight_revision",
        "expired before representation commit",
    )
    .await;
    let in_flight = claim_one(&worker).await;
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT deadline_at<=created_at+interval '10 minutes' FROM embedding_jobs WHERE tenant_id=$1 AND id=$2")
            .bind(TENANT).bind(in_flight.get::<String,_>("job_id")).fetch_one(&db).await.unwrap()
    );
    sqlx::query("UPDATE embedding_jobs SET deadline_at=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT).bind(in_flight.get::<String,_>("job_id")).execute(&db).await.unwrap();
    assert_eq!(
        complete(&worker, &in_flight, "[1,0,0]").await,
        "stopped_stale"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM embedding_representations WHERE tenant_id=$1 AND job_id=$2"
        )
        .bind(TENANT)
        .bind(in_flight.get::<String, _>("job_id"))
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM embedding_jobs WHERE tenant_id=$1 AND id=$2"
        )
        .bind(TENANT)
        .bind(in_flight.get::<String, _>("job_id"))
        .fetch_one(&db)
        .await
        .unwrap(),
        "failed"
    );
}

async fn oversized_candidate_is_not_reported_as_no_match() {
    let (db, runtime, worker) = pools().await;
    sqlx::query(
        "INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,'n3_oversized_item',$2)",
    )
    .bind(TENANT)
    .bind(PRIVATE_COLLECTION)
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_oversized_item','n3_oversized_revision','document shell')")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_oversized_revision' WHERE tenant_id=$1 AND id='n3_oversized_item'")
        .bind(TENANT).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO source_revisions (tenant_id,item_id,revision_id,id,source_sha256) VALUES ($1,'n3_oversized_item','n3_oversized_revision','n3_oversized_source',digest('oversized source','sha256'))")
        .bind(TENANT).execute(&db).await.unwrap();
    select_generation(
        &db,
        "deterministic_oversized",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    sqlx::query("SELECT activate_document_extraction($1,'n3_oversized_item','n3_oversized_revision','n3_oversized_source',digest('oversized source','sha256'),'n3_oversized_set','fixture-parser','v1','cfg',ARRAY['oversized_passage'],ARRAY['root'],ARRAY['none'],ARRAY['{\"page\":1}'],ARRAY['OVERSIZEDMATCH passage cannot fit the requested byte budget'])")
        .bind(TENANT).fetch_one(&db).await.unwrap();
    complete_all(&worker, &deterministic_vector("OVERSIZEDMATCH")).await;
    let result = http_search_with_limit(&runtime, "OVERSIZEDMATCH", 8).await;
    assert_eq!(result.0, StatusCode::OK);
    assert!(result.1["items"].as_array().unwrap().is_empty());
    assert_eq!(result.1["truncated"], true);
    assert!(
        !result.1["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "no_match"),
        "a retrieved candidate omitted by packing is not a genuine no-match"
    );
}

async fn final_release_refreshes_readiness_after_a_correction() {
    let (db, runtime, worker) = pools().await;
    let query = "fresh correction marker";
    seed_memory(
        &db,
        "n3_refresh_item",
        "n3_refresh_revision_1",
        "old semantic-only payload",
    )
    .await;
    select_generation(
        &db,
        "deterministic_refresh",
        "deterministic-fixture-delayed-test",
        "deterministic-input-v1",
    )
    .await;
    let old_job = claim_one(&worker).await;
    assert_eq!(
        complete(&worker, &old_job, &deterministic_vector(query)).await,
        "complete"
    );

    let request_runtime = runtime.clone();
    let request = tokio::spawn(async move { http_search(&request_runtime, query).await });
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    assert!(
        !request.is_finished(),
        "synthetic query embedding leaves a deterministic correction window"
    );
    let mut correction = db.begin().await.unwrap();
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(TENANT)
        .fetch_one(&mut *correction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'n3_refresh_item','n3_refresh_revision_2',$2)")
        .bind(TENANT).bind(query).execute(&mut *correction).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='n3_refresh_revision_2' WHERE tenant_id=$1 AND id='n3_refresh_item'")
        .bind(TENANT).execute(&mut *correction).await.unwrap();
    sqlx::query("INSERT INTO lexical_representations VALUES ($1,'n3_refresh_item','n3_refresh_revision_2',to_tsvector('simple',$2))")
        .bind(TENANT).bind(query).execute(&mut *correction).await.unwrap();
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(TENANT)
        .execute(&mut *correction)
        .await
        .unwrap();
    correction.commit().await.unwrap();

    let response = request.await.unwrap();
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(
        response.1["items"][0]["revision_id"],
        "n3_refresh_revision_2"
    );
    assert_eq!(response.1["items"][0]["semantic_status"], "pending");
    assert_eq!(
        response.1["warnings"],
        serde_json::json!(["lexical_ready", "semantic_processing_pending"])
    );
    assert!(
        !response.1.to_string().contains("n3_refresh_revision_1"),
        "the stale semantic candidate is not released"
    );
}

#[allow(clippy::too_many_lines)] // One public regression retains each incremental exclusion control.
async fn readiness_uses_only_applicable_current_evidence() {
    let (db, runtime, worker) = pools().await;
    let query = "customer delivery exception";
    sqlx::query("INSERT INTO subjects (tenant_id,id,app_id,kind) VALUES ($1,'n3_customer_a',$2,'customer'),($1,'n3_customer_b',$2,'customer')")
        .bind(TENANT).bind(APP).execute(&db).await.unwrap();
    seed_memory(
        &db,
        "n3_ready_a",
        "n3_ready_a_revision",
        "semantic-only authorized payload",
    )
    .await;
    sqlx::query("INSERT INTO revision_subjects (tenant_id,item_id,revision_id,subject_id) VALUES ($1,'n3_ready_a','n3_ready_a_revision','n3_customer_a')")
        .bind(TENANT).execute(&db).await.unwrap();
    select_generation(
        &db,
        "deterministic_scoped",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    complete_all(&worker, &deterministic_vector(query)).await;

    for state in [
        "pending",
        "failed",
        "extraction",
        "expired",
        "future",
        "other_app",
        "private",
    ] {
        let item = format!("n3_unrelated_{state}");
        let revision = format!("{item}_revision");
        seed_memory(&db, &item, &revision, "unrelated unavailable sentinel").await;
        match state {
            "pending" | "failed" | "extraction" => {
                sqlx::query("INSERT INTO revision_subjects (tenant_id,item_id,revision_id,subject_id) VALUES ($1,$2,$3,'n3_customer_b')")
                    .bind(TENANT).bind(&item).bind(&revision).execute(&db).await.unwrap();
                if state == "failed" {
                    sqlx::query("UPDATE embedding_jobs SET status='failed',error_code='provider_failed' WHERE tenant_id=$1 AND item_id=$2")
                        .bind(TENANT).bind(&item).execute(&db).await.unwrap();
                } else if state == "extraction" {
                    sqlx::query("INSERT INTO source_revisions (tenant_id,item_id,revision_id,id,source_sha256) VALUES ($1,$2,$3,'n3_incomplete_source',digest('incomplete','sha256'))")
                        .bind(TENANT).bind(&item).bind(&revision).execute(&db).await.unwrap();
                }
            }
            "expired" | "future" => {
                let current = format!("{revision}_dated");
                sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content,valid_from,valid_until) VALUES ($1,$2,$3,'unrelated unavailable sentinel',CASE WHEN $4 THEN clock_timestamp()+interval '1 day' END,CASE WHEN NOT $4 THEN clock_timestamp()-interval '1 day' END)")
                    .bind(TENANT).bind(&item).bind(&current).bind(state == "future").execute(&db).await.unwrap();
                sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
                    .bind(TENANT)
                    .bind(&item)
                    .bind(&current)
                    .execute(&db)
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document) VALUES ($1,$2,$3,to_tsvector('simple','unrelated unavailable sentinel'))")
                    .bind(TENANT).bind(&item).bind(&current).execute(&db).await.unwrap();
            }
            "other_app" => {
                sqlx::query(
                    "INSERT INTO apps (tenant_id,id,active) VALUES ($1,'n3_other_app',true)",
                )
                .bind(TENANT)
                .execute(&db)
                .await
                .unwrap();
                sqlx::query("INSERT INTO collections (tenant_id,id,app_id,audience_kind,owner_principal_id) VALUES ($1,'n3_other_collection','n3_other_app','private',$2)")
                    .bind(TENANT).bind(PRINCIPAL).execute(&db).await.unwrap();
                sqlx::query("UPDATE items SET collection_id='n3_other_collection' WHERE tenant_id=$1 AND id=$2")
                    .bind(TENANT).bind(&item).execute(&db).await.unwrap();
            }
            _ => {
                sqlx::query("UPDATE items SET collection_id='30000000000000000000000000000002' WHERE tenant_id=$1 AND id=$2")
                    .bind(TENANT).bind(&item).execute(&db).await.unwrap();
            }
        }
        let response = http_search_with_scope(&runtime, query, 4096, Some("n3_customer_a")).await;
        assert_eq!(response.0, StatusCode::OK, "{state}: {}", response.1);
        assert_eq!(
            response.1["items"][0]["item_id"], "n3_ready_a",
            "{state}: {}",
            response.1
        );
        assert_eq!(
            response.1["warnings"],
            serde_json::json!(["lexical_ready", "semantic_ready"]),
            "{state}"
        );
        assert!(!response.1.to_string().contains("n3_unrelated"));
    }
    let omitted = http_search(&runtime, query).await;
    assert_eq!(omitted.0, StatusCode::OK);
    assert!(omitted.1["items"].as_array().unwrap().is_empty());
    let customer_b = http_search_with_scope(&runtime, query, 4096, Some("n3_customer_b")).await;
    assert_eq!(customer_b.0, StatusCode::OK);
    assert!(customer_b.1["items"].as_array().unwrap().is_empty());
    assert_eq!(customer_b.1["warnings"][1], "extraction_not_ready");
}

async fn seed_extracted_passages(
    db: &PgPool,
    item: &str,
    source: &str,
    set: &str,
    passage_ids: &[&str],
    texts: &[&str],
) {
    let revision = format!("{item}-r1");
    sqlx::query(
        "INSERT INTO items (tenant_id,id,collection_id,active_revision_id) VALUES ($1,$2,$3,NULL)",
    )
    .bind(TENANT)
    .bind(item)
    .bind(PRIVATE_COLLECTION)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,$2,$3,'document shell')",
    )
    .bind(TENANT)
    .bind(item)
    .bind(&revision)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(TENANT)
        .bind(item)
        .bind(&revision)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO source_revisions (tenant_id,item_id,revision_id,id,source_sha256) VALUES ($1,$2,$3,$4,digest($4,'sha256'))",
    )
    .bind(TENANT)
    .bind(item)
    .bind(&revision)
    .bind(source)
    .execute(db)
    .await
    .unwrap();
    let parents = vec!["root"; passage_ids.len()];
    let directions = vec!["none"; passage_ids.len()];
    let locators = (1..=passage_ids.len())
        .map(|page| format!("{{\"page\":{page}}}"))
        .collect::<Vec<_>>();
    sqlx::query(
        "SELECT activate_document_extraction($1,$2,$3,$4,digest($4,'sha256'),$5,'fixture-parser','v1','cfg',$6,$7,$8,$9,$10)",
    )
    .bind(TENANT)
    .bind(item)
    .bind(&revision)
    .bind(source)
    .bind(set)
    .bind(passage_ids)
    .bind(&parents)
    .bind(&directions)
    .bind(&locators)
    .bind(texts)
    .fetch_one(db)
    .await
    .unwrap();
}

fn offset_query_vector(query_vector: &str, offset: u16) -> String {
    let parts = query_vector
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .enumerate()
        .map(|(i, part)| {
            let value: i32 = part.parse().expect("query vector component");
            if i == 2 {
                (value + i32::from(offset) + 1).to_string()
            } else {
                value.to_string()
            }
        })
        .collect::<Vec<_>>();
    format!("[{}]", parts.join(","))
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_source_block_packs_in_window_same_source_span() {
    // en09-p01 shape: first-appearance decoy occupies a rank-first slot; the later
    // same-source span stays in fused 41 but sits after 12/4/2160 item cap unless
    // hybrid emits that source's window rows together.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_sblock",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_sblock_anchor",
        "n3_sblock_anchor_source",
        "n3_sblock_anchor_set",
        &["srcblock-decoy", "srcblock-needed"],
        &[
            "The briefing overlap notes are ready for the anchor.",
            "The later method span belongs to the same briefing source.",
        ],
    )
    .await;
    for index in 0..12 {
        let item = format!("n3_sblock_f{index:02}");
        let source = format!("n3_sblock_f{index:02}_source");
        let set = format!("n3_sblock_f{index:02}_set");
        let passage = format!("sblock-f{index:02}");
        let text = format!("The briefing overlap notes are ready for filler {index:02}.");
        seed_extracted_passages(&db, &item, &source, &set, &[&passage], &[text.as_str()]).await;
    }
    let query = "The briefing overlap notes are ready.";
    let query_vector = deterministic_vector(query);
    let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(&worker)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 14);
    for job in &jobs {
        let passage = job.get::<String, _>("passage_id");
        let vector = if passage == "srcblock-needed" {
            "[999,999,999]".to_owned()
        } else if passage == "srcblock-decoy" {
            query_vector.clone()
        } else {
            let offset: u16 = passage
                .rsplit_once('f')
                .and_then(|(_, rest)| rest.parse().ok())
                .unwrap_or(1);
            offset_query_vector(&query_vector, offset)
        };
        assert_eq!(complete(&worker, job, &vector).await, "complete");
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000201")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_sblock")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    assert!(
        fused_ids.iter().any(|id| id == "srcblock-decoy"),
        "first-appearance decoy must already be in the fused 41: {fused_ids:?}"
    );
    assert!(
        fused_ids.iter().any(|id| id == "srcblock-needed"),
        "needed ID must already be in the fused 41: {fused_ids:?}"
    );

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "srcblock-decoy"),
        "first-appearance decoy must pack under 12/4/2160, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "srcblock-needed"),
        "srcblock-needed is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_lexical_head_packs_later_source_in_window() {
    // en-q1 shape after 0010: three earlier sources dump 4 rows each and fill 12/4/2160;
    // lhead-needed stays in fused 41 (source_first is not 1) with lexical_rank<=4.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_lhead",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    for dump in 0..3 {
        let item = format!("n3_lhead_d{dump}");
        let source = format!("n3_lhead_d{dump}_source");
        let set = format!("n3_lhead_d{dump}_set");
        let ids = [
            format!("lhead-d{dump}-p0"),
            format!("lhead-d{dump}-p1"),
            format!("lhead-d{dump}-p2"),
            format!("lhead-d{dump}-p3"),
        ];
        let texts = [
            format!("The briefing overlap notes are ready for dump {dump}."),
            format!("Unrelated semantic neighbor filler {dump} one."),
            format!("Unrelated semantic neighbor filler {dump} two."),
            format!("Unrelated semantic neighbor filler {dump} three."),
        ];
        seed_extracted_passages(
            &db,
            &item,
            &source,
            &set,
            &[
                ids[0].as_str(),
                ids[1].as_str(),
                ids[2].as_str(),
                ids[3].as_str(),
            ],
            &[
                texts[0].as_str(),
                texts[1].as_str(),
                texts[2].as_str(),
                texts[3].as_str(),
            ],
        )
        .await;
    }
    seed_extracted_passages(
        &db,
        "n3_lhead_need",
        "n3_lhead_need_source",
        "n3_lhead_need_set",
        &["lhead-needed"],
        &["The quenched vanadium serial method span belongs to the later source."],
    )
    .await;
    let query = "The briefing overlap notes are ready. Quenched vanadium serial.";
    let query_vector = deterministic_vector(query);
    let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(&worker)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 13);
    for job in &jobs {
        let passage = job.get::<String, _>("passage_id");
        let vector = if passage == "lhead-needed" {
            "[999,999,999]".to_owned()
        } else if let Some((dump, slot)) = passage.strip_prefix("lhead-d").and_then(|rest| {
            let (dump, slot) = rest.split_once("-p")?;
            Some((dump.parse::<u16>().ok()?, slot.parse::<u16>().ok()?))
        }) {
            offset_query_vector(&query_vector, dump * 4 + slot)
        } else {
            offset_query_vector(&query_vector, 20)
        };
        assert_eq!(complete(&worker, job, &vector).await, "complete");
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000202")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_lhead")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    assert!(
        fused_ids.iter().any(|id| id == "lhead-needed"),
        "needed ID must already be in the fused 41: {fused_ids:?}"
    );

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "lhead-needed"),
        "lhead-needed is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160"
    );
}

fn pad_span(text: &str, bytes: usize) -> String {
    let mut span = text.to_string();
    while span.len() < bytes {
        span.push_str(" Station path distance reference access note.");
    }
    span.truncate(bytes);
    while !span.is_char_boundary(span.len()) {
        span.pop();
    }
    span
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_lex_head_defers_huge_same_clause_decoy() {
    // en-q1 after 0011: a huge same-clause lex-head (en05-p01 shape) still fits
    // 2160 and token-stops before the rest of an in-window complete basis.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_sdefer",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    for dump in 0..2 {
        let item = format!("n3_sdefer_d{dump}");
        let source = format!("n3_sdefer_d{dump}_source");
        let set = format!("n3_sdefer_d{dump}_set");
        let ids = [
            format!("sdefer-d{dump}-p0"),
            format!("sdefer-d{dump}-p1"),
            format!("sdefer-d{dump}-p2"),
            format!("sdefer-d{dump}-p3"),
        ];
        let texts = [
            pad_span(
                &format!("The briefing overlap notes are ready for dump {dump}."),
                520,
            ),
            pad_span(
                &format!("Unrelated semantic neighbor filler {dump} one."),
                520,
            ),
            pad_span(
                &format!("Unrelated semantic neighbor filler {dump} two."),
                520,
            ),
            pad_span(
                &format!("Unrelated semantic neighbor filler {dump} three."),
                520,
            ),
        ];
        seed_extracted_passages(
            &db,
            &item,
            &source,
            &set,
            &[
                ids[0].as_str(),
                ids[1].as_str(),
                ids[2].as_str(),
                ids[3].as_str(),
            ],
            &[
                texts[0].as_str(),
                texts[1].as_str(),
                texts[2].as_str(),
                texts[3].as_str(),
            ],
        )
        .await;
    }
    let huge = pad_span(
        "The briefing overlap notes are ready. Shoreline register. ",
        4356,
    );
    seed_extracted_passages(
        &db,
        "n3_sdefer_huge",
        "n3_sdefer_huge_source",
        "n3_sdefer_huge_set",
        &["sdefer-huge"],
        &[huge.as_str()],
    )
    .await;
    let needed_head = pad_span(
        "The quenched vanadium serial method span belongs to the later source.",
        520,
    );
    let needed_tail = pad_span(
        "The later method span belongs to the same later source.",
        520,
    );
    seed_extracted_passages(
        &db,
        "n3_sdefer_need",
        "n3_sdefer_need_source",
        "n3_sdefer_need_set",
        &["sdefer-needed-head", "sdefer-needed-tail"],
        &[needed_head.as_str(), needed_tail.as_str()],
    )
    .await;
    let query = "The briefing overlap notes are ready. Quenched vanadium serial.";
    let query_vector = deterministic_vector(query);
    let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(&worker)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 11);
    for job in &jobs {
        let passage = job.get::<String, _>("passage_id");
        let vector = if passage == "sdefer-needed-head" {
            "[999,999,999]".to_owned()
        } else if passage == "sdefer-needed-tail" {
            offset_query_vector(&query_vector, 1)
        } else if passage == "sdefer-huge" {
            query_vector.clone()
        } else if let Some((dump, slot)) = passage.strip_prefix("sdefer-d").and_then(|rest| {
            let (dump, slot) = rest.split_once("-p")?;
            Some((dump.parse::<u16>().ok()?, slot.parse::<u16>().ok()?))
        }) {
            offset_query_vector(&query_vector, 4 + dump * 4 + slot)
        } else {
            offset_query_vector(&query_vector, 30)
        };
        assert_eq!(complete(&worker, job, &vector).await, "complete");
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000203")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_sdefer")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    assert!(
        fused_ids.iter().any(|id| id == "sdefer-huge"),
        "huge same-clause decoy must already be in the fused 41: {fused_ids:?}"
    );
    assert!(
        fused_ids.iter().any(|id| id == "sdefer-needed-head"),
        "needed head must already be in the fused 41: {fused_ids:?}"
    );
    assert!(
        fused_ids.iter().any(|id| id == "sdefer-needed-tail"),
        "needed tail must already be in the fused 41: {fused_ids:?}"
    );

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "sdefer-needed-head"),
        "lex-head must still pack the later-source head, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "sdefer-needed-tail"),
        "sdefer-needed-tail is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160 after a huge same-clause lex-head decoy"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_headed_lex_neighbor_packs_in_window_extra() {
    // zh-q3 shape after 0012: k=4 heads pack, then earlier dump sources fill 12/4/2160
    // before the headed source's still-lexical extra (zh10-p01, lex 11). Funès-style
    // one extra of a headed source with lexical_rank<=16 must pack without raising
    // 2160 or the 12/4 caps.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_hneigh",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    for dump in 0..3 {
        let item = format!("n3_hneigh_d{dump}");
        let source = format!("n3_hneigh_d{dump}_source");
        let set = format!("n3_hneigh_d{dump}_set");
        let ids = [
            format!("hneigh-d{dump}-p0"),
            format!("hneigh-d{dump}-p1"),
            format!("hneigh-d{dump}-p2"),
            format!("hneigh-d{dump}-p3"),
        ];
        let texts = [
            format!("The briefing overlap notes are ready for dump {dump}."),
            format!("Unrelated semantic neighbor filler {dump} one."),
            format!("Unrelated semantic neighbor filler {dump} two."),
            format!("Unrelated semantic neighbor filler {dump} three."),
        ];
        seed_extracted_passages(
            &db,
            &item,
            &source,
            &set,
            &[
                ids[0].as_str(),
                ids[1].as_str(),
                ids[2].as_str(),
                ids[3].as_str(),
            ],
            &[
                texts[0].as_str(),
                texts[1].as_str(),
                texts[2].as_str(),
                texts[3].as_str(),
            ],
        )
        .await;
    }
    seed_extracted_passages(
        &db,
        "n3_hneigh_need",
        "n3_hneigh_need_source",
        "n3_hneigh_need_set",
        &["hneigh-needed-head", "hneigh-needed-extra"],
        &[
            "The quenched vanadium serial method span belongs to the later source.",
            "The quenched vanadium serial extra span belongs to the same headed source.",
        ],
    )
    .await;
    let query = "The briefing overlap notes are ready. Quenched vanadium serial.";
    let query_vector = deterministic_vector(query);
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            let passage = job.get::<String, _>("passage_id");
            let vector = if passage == "hneigh-needed-head" {
                "[999,999,999]".to_owned()
            } else if passage == "hneigh-needed-extra" {
                "[998,998,998]".to_owned()
            } else if let Some((dump, slot)) = passage.strip_prefix("hneigh-d").and_then(|rest| {
                let (dump, slot) = rest.split_once("-p")?;
                Some((dump.parse::<u16>().ok()?, slot.parse::<u16>().ok()?))
            }) {
                offset_query_vector(&query_vector, dump * 4 + slot)
            } else {
                offset_query_vector(&query_vector, 20)
            };
            assert_eq!(complete(&worker, job, &vector).await, "complete");
        }
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000205")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_hneigh")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    assert!(
        fused_ids.iter().any(|id| id == "hneigh-needed-head"),
        "needed head must already be in the fused 41: {fused_ids:?}"
    );
    assert!(
        fused_ids.iter().any(|id| id == "hneigh-needed-extra"),
        "needed extra must already be in the fused 41: {fused_ids:?}"
    );

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "hneigh-needed-head"),
        "lex-head must still pack the headed source, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "hneigh-needed-extra"),
        "hneigh-needed-extra is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_headed_extra_seq3_defers_behind_unheaded() {
    // zh-q3 after 0014: k=4 heads and the lex<=16 neighbor pack, extra_seq=2
    // stays, but extra_seq>=3 of that headed source (zh07-p00) still emits in
    // the remainder source-block and fills 12/4/2160 before unheaded zh08-p01.
    // Defer extra_seq>=3 only after a real neighbor; keep the 0011 singleton
    // lex head. Skip-singleton remains rejected.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_htail",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_htail_d0",
        "n3_htail_d0_source",
        "n3_htail_d0_set",
        &["htail-d0-p0", "htail-d0-p1", "htail-d0-p2", "htail-d0-p3"],
        &[
            "The briefing overlap notes are ready for dump 0.",
            "Unrelated semantic neighbor filler 0 one.",
            "Unrelated semantic neighbor filler 0 two.",
            "Unrelated semantic neighbor filler 0 three.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_htail_d1",
        "n3_htail_d1_source",
        "n3_htail_d1_set",
        &["htail-d1-p0", "htail-d1-p1", "htail-d1-p2"],
        &[
            "The briefing overlap notes are ready for dump 1.",
            "Unrelated semantic neighbor filler 1 one.",
            "Unrelated semantic neighbor filler 1 two.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_htail_d2",
        "n3_htail_d2_source",
        "n3_htail_d2_set",
        &["htail-d2-p0"],
        &["The briefing overlap notes are ready for dump 2."],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_htail_need",
        "n3_htail_need_source",
        "n3_htail_need_set",
        &[
            "htail-needed-head",
            "htail-needed-extra",
            "htail-needed-left",
            "htail-needed-stub",
        ],
        &[
            "The quenched vanadium serial method span belongs to the later source.",
            "The quenched vanadium serial extra span belongs to the same headed source.",
            "The quenched vanadium serial leftover span belongs to the same headed source.",
            "Short headed stub.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_htail_gold",
        "n3_htail_gold_source",
        "n3_htail_gold_set",
        &["htail-unheaded-gold"],
        &["Unheaded shoreline register gold span."],
    )
    .await;
    let query = "The briefing overlap notes are ready. Quenched vanadium serial.";
    let query_vector = deterministic_vector(query);
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            let passage = job.get::<String, _>("passage_id");
            let vector = if passage == "htail-needed-head" {
                "[999,999,999]".to_owned()
            } else if passage == "htail-needed-extra" {
                "[998,998,998]".to_owned()
            } else if passage == "htail-needed-left" {
                "[997,997,997]".to_owned()
            } else if passage == "htail-needed-stub" {
                "[996,996,996]".to_owned()
            } else if passage == "htail-unheaded-gold" {
                "[999,999,999]".to_owned()
            } else if let Some((dump, slot)) = passage.strip_prefix("htail-d").and_then(|rest| {
                let (dump, slot) = rest.split_once("-p")?;
                Some((dump.parse::<u16>().ok()?, slot.parse::<u16>().ok()?))
            }) {
                offset_query_vector(&query_vector, dump * 4 + slot)
            } else {
                offset_query_vector(&query_vector, 20)
            };
            assert_eq!(complete(&worker, job, &vector).await, "complete");
        }
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000206")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_htail")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    for id in [
        "htail-needed-head",
        "htail-needed-extra",
        "htail-needed-left",
        "htail-needed-stub",
        "htail-unheaded-gold",
        "htail-d2-p0",
    ] {
        assert!(
            fused_ids.iter().any(|got| got == id),
            "{id} must already be in the fused 41: {fused_ids:?}"
        );
    }

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "htail-needed-head"),
        "lex-head must still pack the headed source, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "htail-d2-p0"),
        "0011 singleton lex head must stay packed, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "htail-needed-extra"),
        "lex<=16 neighbor must still pack, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "htail-needed-left"),
        "extra_seq=2 leftover must stay in the remainder, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "htail-unheaded-gold"),
        "htail-unheaded-gold is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160 because extra_seq>=3 of a neighbored source was not deferred"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_rarest_han_content_gram_admits_sem_tail_without_raising_han_k() {
    // zh-q2 shape: gold is semantic-only at the fused tail (direct 53 analog) because
    // Han local k=4 (and k=8) is flooded by common grams. The name bigram DF is 3.
    // Raising Han k globally would add more lex-only decoys and eject the edge
    // semantic-only required ID; this contract must not pass by that change.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_rhcg",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_rhcg_head",
        "n3_rhcg_head_source",
        "n3_rhcg_head_set",
        &["rhcg-head"],
        &["裴宁完成备用字幕操作培训。"],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_rhcg_mid",
        "n3_rhcg_mid_source",
        "n3_rhcg_mid_set",
        &["rhcg-mid"],
        &["裴宁已有整场审核记录。"],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_rhcg_need",
        "n3_rhcg_need_source",
        "n3_rhcg_need_set",
        &["rhcg-needed"],
        &["裴宁用主文件完成实际屏幕显示检查。"],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_rhcg_edge",
        "n3_rhcg_edge_source",
        "n3_rhcg_edge_set",
        &["rhcg-edge"],
        &["The later method span belongs to the edge source."],
    )
    .await;
    for index in 0..12 {
        let item = format!("n3_rhcg_d{index:02}");
        let source = format!("n3_rhcg_d{index:02}_source");
        let set = format!("n3_rhcg_d{index:02}_set");
        let passage = format!("rhcg-d{index:02}");
        let text = format!("演出当天不能到场的候补屏幕记录{index:02}。");
        seed_extracted_passages(&db, &item, &source, &set, &[&passage], &[text.as_str()]).await;
    }
    for index in 0..2 {
        let item = format!("n3_rhcg_if{index}");
        let source = format!("n3_rhcg_if{index}_source");
        let set = format!("n3_rhcg_if{index}_set");
        let passage = format!("rhcg-if{index}");
        seed_extracted_passages(
            &db,
            &item,
            &source,
            &set,
            &[&passage],
            &["如果候补说明写在别册。"],
        )
        .await;
    }
    let mut filler_ids = Vec::new();
    let mut filler_texts = Vec::new();
    for index in 0..37 {
        filler_ids.push(format!("rhcg-f{index:02}"));
        filler_texts.push(format!(
            "The briefing overlap notes are ready for filler {index:02}."
        ));
    }
    let filler_id_refs = filler_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let filler_text_refs = filler_texts.iter().map(String::as_str).collect::<Vec<_>>();
    seed_extracted_passages(
        &db,
        "n3_rhcg_fill",
        "n3_rhcg_fill_source",
        "n3_rhcg_fill_set",
        &filler_id_refs,
        &filler_text_refs,
    )
    .await;

    let query = "如果演出当天裴宁不能到场。";
    let han_k: Option<i32> =
        sqlx::query_scalar("SELECT local_k FROM lexical_clause_queries_v1($1) LIMIT 1")
            .bind(query)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(han_k, Some(4), "Han local_k stays 4");
    let single_bound: String =
        sqlx::query_scalar("SELECT preparation_status FROM lexical_clause_queries_v1($1)")
            .bind("lexical beacon")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(single_bound, "single_bound");

    let query_vector = deterministic_vector(query);
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            let passage = job.get::<String, _>("passage_id");
            let vector = if passage == "rhcg-head" {
                offset_query_vector(&query_vector, 1)
            } else if passage == "rhcg-mid" {
                offset_query_vector(&query_vector, 2)
            } else if passage == "rhcg-edge" {
                offset_query_vector(&query_vector, 36)
            } else if passage == "rhcg-needed" {
                offset_query_vector(&query_vector, 41)
            } else if let Some(rest) = passage.strip_prefix("rhcg-f") {
                let index: u16 = rest.parse().unwrap();
                let offset = if index < 33 {
                    3 + index
                } else {
                    37 + (index - 33)
                };
                offset_query_vector(&query_vector, offset)
            } else {
                offset_query_vector(&query_vector, 200)
            };
            assert_eq!(complete(&worker, job, &vector).await, "complete");
        }
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000204")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_rhcg")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    assert!(
        fused_ids.iter().any(|id| id == "rhcg-needed"),
        "rarest DF>=3 Han content gram must admit rhcg-needed into fused 41 without raising Han k: {fused_ids:?}"
    );
    assert!(
        fused_ids.iter().any(|id| id == "rhcg-edge"),
        "edge semantic-only required ID must stay in fused 41 (Han k=8 crowding would eject it): {fused_ids:?}"
    );
    assert!(
        fused_ids.iter().any(|id| id == "rhcg-head"),
        "name posting already in-window must stay: {fused_ids:?}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_han_two_head_dump_rr_packs_late_unheaded() {
    // zh-q1 after 0015: a two-head dump keeps extras in the remainder; an early
    // unheaded source-block then fills 12/4/2160 before a later required
    // unheaded row. Han-only: defer two-head extras and extra_seq>=2 without a
    // neighbor, then unheaded extra_seq=1 round-robin. English queries stay 0015.
    // Han local_k stays 4.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_h2rr",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_h2rr_d0",
        "n3_h2rr_d0_source",
        "n3_h2rr_d0_set",
        &["h2rr-d0-p0", "h2rr-d0-p1", "h2rr-d0-p2", "h2rr-d0-p3"],
        &[
            "The briefing overlap notes are ready for dump 0 head a.",
            "The briefing overlap notes are ready for dump 0 head b.",
            "Unrelated semantic neighbor filler 0 two.",
            "Unrelated semantic neighbor filler 0 three.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_h2rr_d1",
        "n3_h2rr_d1_source",
        "n3_h2rr_d1_set",
        &["h2rr-d1-p0", "h2rr-d1-p1", "h2rr-d1-p2"],
        &[
            "The briefing overlap notes are ready for dump 1.",
            "Unrelated semantic neighbor filler 1 one.",
            "Unrelated semantic neighbor filler 1 two.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_h2rr_d2",
        "n3_h2rr_d2_source",
        "n3_h2rr_d2_set",
        &["h2rr-d2-p0"],
        &["The briefing overlap notes are ready for dump 2."],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_h2rr_early",
        "n3_h2rr_early_source",
        "n3_h2rr_early_set",
        &[
            "h2rr-early-p0",
            "h2rr-early-p1",
            "h2rr-early-p2",
            "h2rr-early-p3",
        ],
        &[
            "Unrelated semantic neighbor filler early one.",
            "Unrelated semantic neighbor filler early two.",
            "Unrelated semantic neighbor filler early three.",
            "Unrelated semantic neighbor filler early four.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_h2rr_goldb",
        "n3_h2rr_goldb_source",
        "n3_h2rr_goldb_set",
        &["h2rr-late-goldb"],
        &["巡演简报已就绪 Unheaded shoreline register gold span."],
    )
    .await;
    let query = "巡演简报已就绪. The briefing overlap notes are ready.";
    let han_k: Option<i32> =
        sqlx::query_scalar("SELECT local_k FROM lexical_clause_queries_v1($1) LIMIT 1")
            .bind(query)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(han_k, Some(4), "Han local_k stays 4");
    let query_vector = deterministic_vector(query);
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            let passage = job.get::<String, _>("passage_id");
            let vector = if passage == "h2rr-late-goldb" {
                "[900,900,900]".to_owned()
            } else if let Some(rest) = passage.strip_prefix("h2rr-d") {
                if let Some((dump, slot)) = rest.split_once("-p") {
                    let dump = dump.parse::<u16>().unwrap_or(0);
                    let slot = slot.parse::<u16>().unwrap_or(0);
                    offset_query_vector(&query_vector, dump * 4 + slot)
                } else {
                    offset_query_vector(&query_vector, 20)
                }
            } else if let Some(slot) = passage.strip_prefix("h2rr-early-p") {
                let slot = slot.parse::<u16>().unwrap_or(0);
                offset_query_vector(&query_vector, 40 + slot)
            } else {
                offset_query_vector(&query_vector, 20)
            };
            assert_eq!(complete(&worker, job, &vector).await, "complete");
        }
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000207")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_h2rr")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    for id in ["h2rr-d0-p0", "h2rr-d0-p1", "h2rr-d2-p0", "h2rr-late-goldb"] {
        assert!(
            fused_ids.iter().any(|got| got == id),
            "{id} must already be in the fused 41: {fused_ids:?}"
        );
    }

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "h2rr-d2-p0"),
        "0011 singleton lex head must stay packed, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "h2rr-late-goldb"),
        "h2rr-late-goldb is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160 because two-head dump extras and early unheaded source-block were not yielded"
    );
    let pos = |id: &str| packed.iter().position(|got| got == id);
    assert!(
        pos("h2rr-late-goldb").unwrap() < pos("h2rr-d0-p2").unwrap_or(packed.len()),
        "two-head dump extras must wait behind the late unheaded gold, packed {packed:?}"
    );
    assert!(
        pos("h2rr-d1-p2").is_none() || pos("h2rr-late-goldb").unwrap() < pos("h2rr-d1-p2").unwrap(),
        "extra_seq>=2 of a one-head Han dump without a neighbor must wait behind gold, packed {packed:?}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_han_unheaded_adj_packs_next_passage() {
    // zh-q1 after 0016: unheaded extra_seq=1 of a late source is p00 (lex-only);
    // extra_seq=2 is p04; gold p01 is extra_seq=3. Dual-weak extra_seq=1 rows
    // fill 12/4/2160. Han-only: keep lex-only firsts (sem<=10 OR lex<=6 OR
    // lex-only lex<=16) and emit p00's next passage beside it. English stays
    // 0015/0016. Han local_k stays 4. Singleton k=4 heads stay.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_huan",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_huan_d0",
        "n3_huan_d0_source",
        "n3_huan_d0_set",
        &["huan-d0-p0", "huan-d0-p1", "huan-d0-p2", "huan-d0-p3"],
        &[
            "The briefing overlap notes are ready for dump 0 head a.",
            "The briefing overlap notes are ready for dump 0 head b.",
            "Unrelated semantic neighbor filler 0 two.",
            "Unrelated semantic neighbor filler 0 three.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_huan_d1",
        "n3_huan_d1_source",
        "n3_huan_d1_set",
        &["huan-d1-p0", "huan-d1-p1", "huan-d1-p2"],
        &[
            "The briefing overlap notes are ready for dump 1.",
            "Unrelated semantic neighbor filler 1 one.",
            "Unrelated semantic neighbor filler 1 two.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_huan_d2",
        "n3_huan_d2_source",
        "n3_huan_d2_set",
        &["huan-d2-p0"],
        &["The briefing overlap notes are ready for dump 2."],
    )
    .await;
    for index in 0..8 {
        let item = format!("n3_huan_w{index}");
        let source = format!("n3_huan_w{index}_source");
        let set = format!("n3_huan_w{index}_set");
        let passage = format!("huan-w{index}-p00");
        let text = format!("Unrelated semantic neighbor filler wait {index} span.");
        seed_extracted_passages(&db, &item, &source, &set, &[&passage], &[text.as_str()]).await;
    }
    seed_extracted_passages(
        &db,
        "n3_huan_gold",
        "n3_huan_gold_source",
        "n3_huan_gold_set",
        &["huan-late-p00", "huan-late-p04", "huan-late-p01"],
        &[
            "巡演简报已就绪 late first span.",
            "Unrelated semantic neighbor filler late four.",
            "巡演简报已就绪 late adjacent gold span.",
        ],
    )
    .await;
    let query = "巡演简报已就绪. The briefing overlap notes are ready.";
    let han_k: Option<i32> =
        sqlx::query_scalar("SELECT local_k FROM lexical_clause_queries_v1($1) LIMIT 1")
            .bind(query)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(han_k, Some(4), "Han local_k stays 4");
    let query_vector = deterministic_vector(query);
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            let passage = job.get::<String, _>("passage_id");
            let vector = if passage == "huan-late-p00" {
                "[900,900,900]".to_owned()
            } else if passage == "huan-late-p01" {
                "[880,880,880]".to_owned()
            } else if passage == "huan-late-p04" {
                "[860,860,860]".to_owned()
            } else if let Some(rest) = passage.strip_prefix("huan-d") {
                if let Some((dump, slot)) = rest.split_once("-p") {
                    let dump = dump.parse::<u16>().unwrap_or(0);
                    let slot = slot.parse::<u16>().unwrap_or(0);
                    offset_query_vector(&query_vector, dump * 4 + slot)
                } else {
                    offset_query_vector(&query_vector, 20)
                }
            } else if let Some(rest) = passage.strip_prefix("huan-w") {
                let index: u16 = rest.split('-').next().unwrap_or("0").parse().unwrap_or(0);
                offset_query_vector(&query_vector, 12 + index)
            } else {
                offset_query_vector(&query_vector, 20)
            };
            assert_eq!(complete(&worker, job, &vector).await, "complete");
        }
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000208")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_huan")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    for id in [
        "huan-d2-p0",
        "huan-late-p00",
        "huan-late-p01",
        "huan-late-p04",
    ] {
        assert!(
            fused_ids.iter().any(|got| got == id),
            "{id} must already be in the fused 41: {fused_ids:?}"
        );
    }

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "huan-d2-p0"),
        "0011 singleton lex head must stay packed, packed {packed:?}"
    );
    let pos = |id: &str| packed.iter().position(|got| got == id);
    assert!(
        packed.iter().any(|id| id == "huan-late-p00"),
        "lex-only unheaded first p00 must pack, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "huan-late-p01"),
        "huan-late-p01 is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160 because extra_seq=3 of an unheaded source was not yielded as han_unheaded_adj"
    );
    assert!(
        pos("huan-late-p00").unwrap() < pos("huan-late-p01").unwrap(),
        "adjacent next passage emits after its first, packed {packed:?}"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn hybrid_han_deferred_span_packs_skip_passage() {
    // zh-q2 after 0017: unheaded extra_seq=1 is p00; gold p02/p03 are extra_seq
    // 2-3 and pass+1 (p01) is absent, so 0017 adj does not fire. Seven first_ok
    // decoys plus four k=4 heads fill 12 before extra_seq 2-3. Han-only: pair
    // extra_seq 2-3 of the single best first_ok unheaded source when that
    // source has no adj. Golds omit the query's Han/English lexemes so they
    // stay unheaded (Han local_k stays 4). English emit stays 0015/0016/0017.
    let (db, runtime, worker) = pools().await;
    select_generation(
        &db,
        "deterministic_hspan",
        "deterministic-fixture-v1",
        "deterministic-input-v1",
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_hspan_d0",
        "n3_hspan_d0_source",
        "n3_hspan_d0_set",
        &["hspan-d0-p0", "hspan-d0-p1", "hspan-d0-p2", "hspan-d0-p3"],
        &[
            "The briefing overlap notes are ready for dump 0 head a.",
            "The briefing overlap notes are ready for dump 0 head b.",
            "Unrelated semantic neighbor filler 0 two.",
            "Unrelated semantic neighbor filler 0 three.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_hspan_d1",
        "n3_hspan_d1_source",
        "n3_hspan_d1_set",
        &["hspan-d1-p0", "hspan-d1-p1", "hspan-d1-p2"],
        &[
            "The briefing overlap notes are ready for dump 1.",
            "Unrelated semantic neighbor filler 1 one.",
            "Unrelated semantic neighbor filler 1 two.",
        ],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_hspan_d2",
        "n3_hspan_d2_source",
        "n3_hspan_d2_set",
        &["hspan-d2-p0"],
        &["The briefing overlap notes are ready for dump 2."],
    )
    .await;
    seed_extracted_passages(
        &db,
        "n3_hspan_gold",
        "n3_hspan_gold_source",
        "n3_hspan_gold_set",
        &["hspan-late-p00", "hspan-late-p02", "hspan-late-p03"],
        &[
            "Late shoreline register pair first span.",
            "Late shoreline register pair two span.",
            "Late shoreline register pair three gold span.",
        ],
    )
    .await;
    for index in 0..7 {
        let item = format!("n3_hspan_w{index}");
        let source = format!("n3_hspan_w{index}_source");
        let set = format!("n3_hspan_w{index}_set");
        let passage = format!("hspan-w{index}-p00");
        let text = format!("Unrelated semantic neighbor filler wait {index} span.");
        seed_extracted_passages(&db, &item, &source, &set, &[&passage], &[text.as_str()]).await;
    }
    let query = "巡演简报已就绪. The briefing overlap notes are ready.";
    let han_k: Option<i32> =
        sqlx::query_scalar(
            "SELECT local_k FROM lexical_clause_queries_v1($1) WHERE local_k IS NOT NULL ORDER BY local_k ASC LIMIT 1",
        )
            .bind(query)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(han_k, Some(4), "Han local_k stays 4");
    let query_vector = deterministic_vector(query);
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(&worker)
            .await
            .unwrap();
        if jobs.is_empty() {
            break;
        }
        for job in &jobs {
            let passage = job.get::<String, _>("passage_id");
            let vector = if passage == "hspan-late-p00" {
                offset_query_vector(&query_vector, 0)
            } else if passage == "hspan-late-p02" {
                offset_query_vector(&query_vector, 1)
            } else if passage == "hspan-late-p03" {
                offset_query_vector(&query_vector, 2)
            } else if let Some(rest) = passage.strip_prefix("hspan-w") {
                let index: u16 = rest.split('-').next().unwrap_or("0").parse().unwrap_or(0);
                offset_query_vector(&query_vector, 3 + index)
            } else {
                "[20,20,20]".to_owned()
            };
            assert_eq!(complete(&worker, job, &vector).await, "complete");
        }
    }

    let mut fused_tx = runtime.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.credential_digest',$1,true),set_config('app.operation','search',true),set_config('app.tenant_id',$2,true)")
        .bind(hex(&Sha256::digest(TOKEN.as_bytes())))
        .bind(TENANT)
        .execute(&mut *fused_tx)
        .await
        .unwrap();
    let fused = sqlx::query(
        "SELECT passage_id FROM search_hybrid_current_memories($1,$2,$3,$4,$5,NULL,$6,$7)",
    )
    .bind(TENANT)
    .bind(READER_ID)
    .bind("00000000-0000-0000-0000-000000000209")
    .bind(query)
    .bind(16_384_i32)
    .bind("deterministic_hspan")
    .bind(&query_vector)
    .fetch_all(&mut *fused_tx)
    .await
    .unwrap();
    fused_tx.commit().await.unwrap();
    let fused_ids = fused
        .iter()
        .map(|row| row.get::<String, _>("passage_id"))
        .collect::<Vec<_>>();
    for id in [
        "hspan-d2-p0",
        "hspan-late-p00",
        "hspan-late-p02",
        "hspan-late-p03",
    ] {
        assert!(
            fused_ids.iter().any(|got| got == id),
            "{id} must already be in the fused 41: {fused_ids:?}"
        );
    }

    let input = serde_json::json!({
        "query":query,
        "semantic":true,
        "max_context_bytes":16384,
        "context_token_budget":{
            "tokenizer":"o200k_base:tiktoken-rs-0.12.0",
            "max_tokens":2160
        }
    });
    let response = router(runtime)
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let packed = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["citation"]["passage_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        packed.len() <= 12,
        "native item guard stays 12, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "hspan-d2-p0"),
        "0011 singleton lex head must stay packed, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "hspan-late-p00"),
        "unheaded first_ok p00 must pack, packed {packed:?}"
    );
    assert!(
        packed.iter().any(|id| id == "hspan-late-p02"),
        "hspan-late-p02 is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160 because extra_seq=2 of an unheaded first_ok source without adj was not yielded as han_unheaded_pair"
    );
    assert!(
        packed.iter().any(|id| id == "hspan-late-p03"),
        "hspan-late-p03 is in fused {fused_ids:?} but absent from packed {packed:?} under 12/4/2160 because extra_seq=3 of an unheaded first_ok source without adj was not yielded as han_unheaded_pair"
    );
}

#[tokio::test]
async fn n3_semantic_suite() {
    readiness_uses_only_applicable_current_evidence().await;
    external_vectors_are_generation_bound_and_provider_neutral().await;
    native_search_degrades_to_lexical_with_sanitized_diagnostics().await;
    document_vectors_activate_only_as_a_complete_current_set().await;
    deterministic_jobs_are_fenced_and_hybrid_search_is_authorized().await;
    crash_retry_and_lifecycle_changes_cannot_activate_stale_evidence().await;
    claim_rechecks_authority_after_the_tenant_barrier().await;
    generation_selection_serializes_with_uncommitted_save_intent().await;
    exhausted_leases_and_absolute_deadlines_terminalize().await;
    oversized_candidate_is_not_reported_as_no_match().await;
    final_release_refreshes_readiness_after_a_correction().await;
}
