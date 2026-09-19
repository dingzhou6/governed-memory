//! Named P2 job/authority-race suite.
//!
//! Ledger: `docs/architecture/agentic-memory-p2-job-authority-races.md`.
//! Uses the ordinary synthetic fixture on 55432. Do not point this at 55456–55468.

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
const ALPHA_TENANT: &str = "00000000000000000000000000000001";
const ALPHA_APP: &str = "a0000000000000000000000000000001";
const ALICE_PRIVATE_COLLECTION: &str = "30000000000000000000000000000004";
const ALICE_CREDENTIAL: &str = "c0000000000000000000000000000001";
const ALICE_PRINCIPAL: &str = "10000000000000000000000000000001";
const ALICE_SUBJECT: &str = "s0000000000000000000000000000001";
const WRITER_CREDENTIAL: &str = "c0000000000000000000000000000003";

fn test_database_url(url: &str) -> String {
    match option_env!("MEMORY_TEST_PG_PORT") {
        None | Some("55432") => url.to_owned(),
        Some("55433") => url.replace("127.0.0.1:55432", "127.0.0.1:55433"),
        Some("55434") => url.replace("127.0.0.1:55432", "127.0.0.1:55434"),
        Some(_) => panic!("unsupported synthetic test database port"),
    }
}

#[tokio::test]
async fn p2_http_correct_fences_inflight_embedding_job() {
    let (migrator, runtime, worker, alice, writer) = setup().await;
    select_generation(&migrator, "p2_race_gen", "p2-model-v1", "p2-recipe-v1").await;
    let app = router(runtime);
    let created = create_memory(&app, &writer, "P2_RACE_ORIGINAL unique original").await;
    let item_id = created["item_id"].as_str().expect("item").to_owned();
    let revision_id = created["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let job = claim_one(&worker).await;
    assert_eq!(job.get::<String, _>("item_id"), item_id);
    assert_eq!(job.get::<String, _>("revision_id"), revision_id);

    let corrected = send(
        &app,
        "PUT",
        &format!("/v1/items/{item_id}"),
        Some(&writer),
        format!(
            r#"{{"content":"P2_RACE_CORRECTED unique corrected","expected_revision_id":"{revision_id}","subjects":["{ALICE_SUBJECT}"]}}"#
        )
        .as_bytes(),
        None,
    )
    .await;
    assert_eq!(corrected.0, StatusCode::OK, "{}", corrected.1);
    let new_revision = corrected.1["revision_id"]
        .as_str()
        .expect("new revision")
        .to_owned();
    assert_ne!(new_revision, revision_id);

    let outcome = complete_job(&worker, &job, "[1,0,0]").await;
    assert_eq!(outcome, "stopped_stale");
    assert_eq!(embedding_count(&migrator, &item_id, &revision_id).await, 0);

    let read = send(
        &app,
        "GET",
        &format!("/v1/items/{item_id}"),
        Some(&alice),
        &[],
        None,
    )
    .await;
    assert_eq!(read.0, StatusCode::OK);
    assert_eq!(read.1["content"], "P2_RACE_CORRECTED unique corrected");
    let old_search = search(&app, &alice, "P2_RACE_ORIGINAL").await;
    assert_eq!(old_search.0, StatusCode::OK);
    assert!(!old_search.1.to_string().contains("P2_RACE_ORIGINAL"));
    let new_search = search(&app, &alice, "P2_RACE_CORRECTED").await;
    assert_eq!(new_search.0, StatusCode::OK);
    assert!(new_search.1.to_string().contains("P2_RACE_CORRECTED"));
}

#[tokio::test]
async fn p2_http_forget_and_withdraw_fence_inflight_jobs() {
    let (migrator, runtime, worker, alice, writer) = setup().await;
    select_generation(&migrator, "p2_forget_gen", "p2-model-v1", "p2-recipe-v1").await;
    let app = router(runtime);
    let created = create_memory(&app, &writer, "P2_FORGET_BEACON unique forget").await;
    let item_id = created["item_id"].as_str().expect("item").to_owned();
    let revision_id = created["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let job = claim_one(&worker).await;

    let forgotten = send(
        &app,
        "DELETE",
        &format!("/v1/items/{item_id}"),
        Some(&writer),
        format!(r#"{{"expected_revision_id":"{revision_id}"}}"#).as_bytes(),
        None,
    )
    .await;
    assert_eq!(forgotten.0, StatusCode::OK, "{}", forgotten.1);
    assert_eq!(
        complete_job(&worker, &job, "[1,0,0]").await,
        "stopped_stale"
    );
    assert_eq!(embedding_count(&migrator, &item_id, &revision_id).await, 0);
    let read = send(
        &app,
        "GET",
        &format!("/v1/items/{item_id}"),
        Some(&alice),
        &[],
        None,
    )
    .await;
    assert_eq!(read.0, StatusCode::NOT_FOUND);
    assert_eq!(read.1["code"], "unavailable");
    let searched = search(&app, &alice, "P2_FORGET_BEACON").await;
    assert_eq!(searched.0, StatusCode::OK);
    assert!(!searched.1.to_string().contains("P2_FORGET"));

    let withdrawn = create_memory(&app, &writer, "P2_WITHDRAW_BEACON unique withdraw").await;
    let withdrawn_id = withdrawn["item_id"].as_str().expect("item").to_owned();
    let withdrawn_rev = withdrawn["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let withdrawn_job = claim_one(&worker).await;
    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_PRIVATE_COLLECTION)
    .execute(&migrator)
    .await
    .expect("withdraw Alice private");
    assert_eq!(
        complete_job(&worker, &withdrawn_job, "[1,0,0]").await,
        "stopped_stale"
    );
    assert_eq!(
        embedding_count(&migrator, &withdrawn_id, &withdrawn_rev).await,
        0
    );
    let withdrawn_read = send(
        &app,
        "GET",
        &format!("/v1/items/{withdrawn_id}"),
        Some(&alice),
        &[],
        None,
    )
    .await;
    assert_eq!(withdrawn_read.0, StatusCode::NOT_FOUND);
    let withdrawn_search = search(&app, &alice, "P2_WITHDRAW_BEACON").await;
    assert_eq!(withdrawn_search.0, StatusCode::OK);
    assert!(!withdrawn_search.1.to_string().contains("P2_WITHDRAW"));
}

#[tokio::test]
async fn p2_owner_revoke_fences_inflight_embedding_job() {
    let (migrator, runtime, worker, alice, writer) = setup().await;
    select_generation(&migrator, "p2_revoke_gen", "p2-model-v1", "p2-recipe-v1").await;
    let app = router(runtime);
    let created = create_memory(&app, &writer, "P2_REVOKE_BEACON unique revoke").await;
    let item_id = created["item_id"].as_str().expect("item").to_owned();
    let revision_id = created["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let job = claim_one(&worker).await;
    sqlx::query("UPDATE principals SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ALICE_PRINCIPAL)
        .execute(&migrator)
        .await
        .expect("deactivate owner");
    assert_eq!(
        complete_job(&worker, &job, "[1,0,0]").await,
        "stopped_stale"
    );
    assert_eq!(embedding_count(&migrator, &item_id, &revision_id).await, 0);
    let read = send(
        &app,
        "GET",
        &format!("/v1/items/{item_id}"),
        Some(&alice),
        &[],
        None,
    )
    .await;
    assert_eq!(read.0, StatusCode::UNAUTHORIZED);
    sqlx::query("UPDATE principals SET active=true WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ALICE_PRINCIPAL)
        .execute(&migrator)
        .await
        .expect("restore owner");
}

#[tokio::test]
async fn p2_extract_replace_fences_inflight_passage_job() {
    let (migrator, runtime, worker, alice, _writer) = setup().await;
    select_generation(&migrator, "p2_i02_gen", "p2-model-v1", "p2-recipe-v1").await;
    let app = router(runtime);
    seed_extracted_item(
        &migrator,
        "p2_i02_item",
        "p2_i02_source",
        "p2_i02_set_v1",
        &["p2-i02-old"],
        &["P2_OLD_SPAN_MARKER old extraction"],
    )
    .await;
    let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(&worker)
        .await
        .expect("claim extract jobs");
    let job = jobs
        .iter()
        .find(|row| row.get::<Option<String>, _>("passage_id").as_deref() == Some("p2-i02-old"))
        .expect("old passage job");
    activate_set(
        &migrator,
        "p2_i02_item",
        "p2_i02_item-r1",
        "p2_i02_source",
        "p2_i02_set_v2",
        &["p2-i02-new"],
        &["P2_NEW_SPAN_MARKER new extraction"],
    )
    .await;
    assert_eq!(complete_job(&worker, job, "[1,0,0]").await, "stopped_stale");
    let old_passage_vectors: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embedding_representations
         WHERE tenant_id=$1 AND passage_id='p2-i02-old'",
    )
    .bind(ALPHA_TENANT)
    .fetch_one(&migrator)
    .await
    .expect("count old passage vectors");
    assert_eq!(old_passage_vectors, 0);
    let old_lex = search(&app, &alice, "P2_OLD_SPAN_MARKER").await;
    assert_eq!(old_lex.0, StatusCode::OK);
    assert!(!old_lex.1.to_string().contains("P2_OLD_SPAN_MARKER"));
    let new_lex = search(&app, &alice, "P2_NEW_SPAN_MARKER").await;
    assert_eq!(new_lex.0, StatusCode::OK);
    assert!(new_lex.1.to_string().contains("P2_NEW_SPAN_MARKER"));
}

#[tokio::test]
async fn p2_concurrent_correct_and_duplicate_create_are_single_mutations() {
    let (_migrator, runtime, _worker, alice, writer) = setup().await;
    let app = router(runtime);
    let created = create_memory(&app, &writer, "P2_CONCURRENT_BASE unique base").await;
    let item_id = created["item_id"].as_str().expect("item").to_owned();
    let revision_id = created["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let left_body = format!(
        r#"{{"content":"P2_CONCURRENT_LEFT unique left","expected_revision_id":"{revision_id}","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let right_body = format!(
        r#"{{"content":"P2_CONCURRENT_RIGHT unique right","expected_revision_id":"{revision_id}","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let left_uri = format!("/v1/items/{item_id}");
    let right_uri = left_uri.clone();
    let left_app = app.clone();
    let right_app = app.clone();
    let writer_left = writer.clone();
    let writer_right = writer.clone();
    let (left, right) = tokio::join!(
        send(
            &left_app,
            "PUT",
            &left_uri,
            Some(&writer_left),
            left_body.as_bytes(),
            Some("p2-competing-correct-left01"),
        ),
        send(
            &right_app,
            "PUT",
            &right_uri,
            Some(&writer_right),
            right_body.as_bytes(),
            Some("p2-competing-correct-right1"),
        ),
    );
    let (winner, loser, winner_needle) = if left.0 == StatusCode::OK {
        (&left, &right, "P2_CONCURRENT_LEFT")
    } else {
        (&right, &left, "P2_CONCURRENT_RIGHT")
    };
    assert_eq!(winner.0, StatusCode::OK, "{}", winner.1);
    assert_eq!(loser.0, StatusCode::CONFLICT, "{}", loser.1);
    assert_eq!(loser.1["code"], "stale_context");
    let read = send(
        &app,
        "GET",
        &format!("/v1/items/{item_id}"),
        Some(&alice),
        &[],
        None,
    )
    .await;
    assert_eq!(read.0, StatusCode::OK);
    assert!(
        read.1["content"]
            .as_str()
            .expect("content")
            .contains(winner_needle)
    );
    assert!(!read.1.to_string().contains("P2_CONCURRENT_BASE"));

    let key = "p2-create-idempotency-key01";
    let body =
        format!(r#"{{"content":"P2_IDEM_BEACON unique idem","subjects":["{ALICE_SUBJECT}"]}}"#);
    let first = send(
        &app,
        "POST",
        "/v1/memories",
        Some(&writer),
        body.as_bytes(),
        Some(key),
    )
    .await;
    assert_eq!(first.0, StatusCode::CREATED, "{}", first.1);
    let second = send(
        &app,
        "POST",
        "/v1/memories",
        Some(&writer),
        body.as_bytes(),
        Some(key),
    )
    .await;
    assert_eq!(second.0, StatusCode::OK, "{}", second.1);
    assert_eq!(second.1["item_id"], first.1["item_id"]);
    assert_eq!(second.1["revision_id"], first.1["revision_id"]);
    assert_eq!(second.1["replayed"], true);
}

#[tokio::test]
async fn p2_expired_lease_late_complete_is_stopped_stale() {
    let (migrator, runtime, worker, alice, writer) = setup().await;
    select_generation(&migrator, "p2_crash_gen", "p2-model-v1", "p2-recipe-v1").await;
    let app = router(runtime);
    let created = create_memory(&app, &writer, "P2_CRASH_BEACON unique crash").await;
    let item_id = created["item_id"].as_str().expect("item").to_owned();
    let revision_id = created["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let first = claim_one(&worker).await;
    sqlx::query(
        "UPDATE embedding_jobs SET lease_expires_at=clock_timestamp()-interval '1 second'
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(first.get::<String, _>("job_id"))
    .execute(&migrator)
    .await
    .expect("expire lease");
    let replay = claim_one(&worker).await;
    assert_eq!(
        replay.get::<String, _>("job_id"),
        first.get::<String, _>("job_id")
    );
    assert_eq!(replay.get::<i64, _>("attempt"), 2);
    assert_eq!(
        complete_job(&worker, &first, "[1,0,0]").await,
        "stopped_stale"
    );
    assert_eq!(complete_job(&worker, &replay, "[1,0,0]").await, "complete");
    assert_eq!(embedding_count(&migrator, &item_id, &revision_id).await, 1);
    let read = send(
        &app,
        "GET",
        &format!("/v1/items/{item_id}"),
        Some(&alice),
        &[],
        None,
    )
    .await;
    assert_eq!(read.0, StatusCode::OK);
    assert!(
        read.1["content"]
            .as_str()
            .expect("content")
            .contains("P2_CRASH_BEACON")
    );
}

async fn setup() -> (PgPool, PgPool, PgPool, String, String) {
    let migrator = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url(MIGRATOR_URL))
        .await
        .expect("run `just setup` first");
    let runtime = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("restricted runtime connection");
    let worker = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url(WORKER_URL))
        .await
        .expect("purge worker");
    let mut transaction = migrator.begin().await.expect("begin fixture reset");
    sqlx::raw_sql(include_str!("fixtures/reset.sql"))
        .execute(&mut *transaction)
        .await
        .expect("reset synthetic fixture");
    let alice = random_bearer();
    let bob = random_bearer();
    let writer = random_bearer();
    insert_reader(&mut transaction, ALICE_CREDENTIAL, ALICE_PRINCIPAL, &alice).await;
    insert_reader(
        &mut transaction,
        "c0000000000000000000000000000002",
        "10000000000000000000000000000002",
        &bob,
    )
    .await;
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class, allowed_operations, issued_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'trusted_writer', ARRAY['create', 'correct', 'forget'], clock_timestamp(), clock_timestamp() + interval '24 hours')",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .bind(ALICE_PRINCIPAL)
    .bind(ALPHA_APP)
    .bind(Sha256::digest(writer.as_bytes()).as_slice())
    .execute(&mut *transaction)
    .await
    .expect("insert writer");
    transaction.commit().await.expect("commit fixture");
    (migrator, runtime, worker, alice, writer)
}

async fn insert_reader(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    credential_id: &str,
    principal_id: &str,
    bearer: &str,
) {
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class, allowed_operations, issued_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'agent_reader', ARRAY['list', 'search', 'read'], clock_timestamp(), clock_timestamp() + interval '24 hours')",
    )
    .bind(ALPHA_TENANT)
    .bind(credential_id)
    .bind(principal_id)
    .bind(ALPHA_APP)
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .execute(&mut **transaction)
    .await
    .expect("insert reader");
}

async fn select_generation(pool: &PgPool, generation: &str, model: &str, recipe: &str) {
    sqlx::query("INSERT INTO embedding_generations (tenant_id,id,model_version,input_recipe_version,input_recipe,dimensions) VALUES ($1,$2,$3,$4,'trimmed UTF-8 body prefixed by recipe version',3)")
        .bind(ALPHA_TENANT)
        .bind(generation)
        .bind(model)
        .bind(recipe)
        .execute(pool)
        .await
        .expect("insert generation");
    sqlx::query("INSERT INTO active_embedding_generations (tenant_id,app_id,generation_id) VALUES ($1,$2,$3) ON CONFLICT (tenant_id,app_id) DO UPDATE SET generation_id=excluded.generation_id,selected_at=clock_timestamp()")
        .bind(ALPHA_TENANT)
        .bind(ALPHA_APP)
        .bind(generation)
        .execute(pool)
        .await
        .expect("select generation");
}

async fn create_memory(app: &axum::Router, writer: &str, content: &str) -> Value {
    let (status, body) = send(
        app,
        "POST",
        "/v1/memories",
        Some(writer),
        format!(r#"{{"content":"{content}","subjects":["{ALICE_SUBJECT}"]}}"#).as_bytes(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

async fn search(app: &axum::Router, bearer: &str, query: &str) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        "/v1/search",
        Some(bearer),
        format!(r#"{{"query":"{query}"}}"#).as_bytes(),
        None,
    )
    .await
}

async fn claim_one(worker: &PgPool) -> sqlx::postgres::PgRow {
    let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
        .fetch_all(worker)
        .await
        .expect("claim jobs");
    assert_eq!(
        jobs.len(),
        1,
        "expected one embedding job, got {}",
        jobs.len()
    );
    jobs.into_iter().next().expect("job row")
}

async fn complete_job(worker: &PgPool, job: &sqlx::postgres::PgRow, vector: &str) -> String {
    sqlx::query_scalar("SELECT complete_embedding_job($1,$2,$3,$4)")
        .bind(job.get::<String, _>("tenant_id"))
        .bind(job.get::<String, _>("job_id"))
        .bind(job.get::<i64, _>("attempt"))
        .bind(vector)
        .fetch_one(worker)
        .await
        .expect("complete job")
}

async fn embedding_count(pool: &PgPool, item_id: &str, revision_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM embedding_representations
         WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .fetch_one(pool)
    .await
    .expect("count embeddings")
}

async fn seed_extracted_item(
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
    .bind(ALPHA_TENANT)
    .bind(item)
    .bind(ALICE_PRIVATE_COLLECTION)
    .execute(db)
    .await
    .expect("seed item");
    sqlx::query(
        "INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,$2,$3,'document shell')",
    )
    .bind(ALPHA_TENANT)
    .bind(item)
    .bind(&revision)
    .execute(db)
    .await
    .expect("seed revision");
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(item)
        .bind(&revision)
        .execute(db)
        .await
        .expect("activate revision");
    sqlx::query(
        "INSERT INTO source_revisions (tenant_id,item_id,revision_id,id,source_sha256) VALUES ($1,$2,$3,$4,digest($4,'sha256'))",
    )
    .bind(ALPHA_TENANT)
    .bind(item)
    .bind(&revision)
    .bind(source)
    .execute(db)
    .await
    .expect("seed source revision");
    activate_set(db, item, &revision, source, set, passage_ids, texts).await;
}

async fn activate_set(
    db: &PgPool,
    item: &str,
    revision: &str,
    source: &str,
    set: &str,
    passage_ids: &[&str],
    texts: &[&str],
) {
    let parents = vec!["root"; passage_ids.len()];
    let directions = vec!["none"; passage_ids.len()];
    let locators = (1..=passage_ids.len())
        .map(|page| format!("{{\"page\":{page}}}"))
        .collect::<Vec<_>>();
    sqlx::query(
        "SELECT activate_document_extraction($1,$2,$3,$4,digest($4,'sha256'),$5,'fixture-parser','v1',$5,$6,$7,$8,$9,$10)",
    )
    .bind(ALPHA_TENANT)
    .bind(item)
    .bind(revision)
    .bind(source)
    .bind(set)
    .bind(passage_ids)
    .bind(&parents)
    .bind(&directions)
    .bind(&locators)
    .bind(texts)
    .fetch_one(db)
    .await
    .expect("activate extraction");
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: &[u8],
    idempotency_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if matches!(method, "POST" | "PUT" | "DELETE") {
        builder = builder.header("content-type", "application/json");
        let key = idempotency_key.map_or_else(random_bearer, str::to_owned);
        builder = builder.header("idempotency-key", key);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("valid request"),
        )
        .await
        .expect("router response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded response");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON response")
    };
    (status, body)
}

fn random_bearer() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness for synthetic credential");
    let mut bearer = String::with_capacity(64);
    for byte in bytes {
        write!(bearer, "{byte:02x}").expect("writing to String");
    }
    bearer
}
