//! Named P1 public-operation ACL/tenant suite.
//!
//! Ledger: `docs/architecture/agentic-memory-p1-acl-matrix.md`.
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
const ALLOWED_ITEM: &str = "40000000000000000000000000000001";
const BOB_PRIVATE_ITEM: &str = "40000000000000000000000000000002";
const FOREIGN_ITEM: &str = "40000000000000000000000000000003";
const MISSING_ITEM: &str = "40000000000000000000000000000004";
const RESTRICTED_COLLECTION: &str = "30000000000000000000000000000001";
const BOB_PRIVATE_COLLECTION: &str = "30000000000000000000000000000002";
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
#[allow(clippy::too_many_lines)]
async fn p1_http_auth_and_tenant_grid() {
    let (migrator, runtime, alice, bob, writer) = setup().await;
    index_fixture_revisions(&migrator).await;
    let app = router(runtime);

    for (method, uri, body) in public_routes(ALLOWED_ITEM) {
        let (status, response) = send(&app, method, &uri, None, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "missing bearer {method} {uri}"
        );
        assert_unauthenticated(&response);
    }

    sqlx::query(
        "UPDATE credentials SET revoked_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("revoke Alice");
    for (method, uri, body) in reader_routes(ALLOWED_ITEM) {
        let (status, response) = send(&app, method, &uri, Some(&alice), body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "revoked reader {method} {uri}"
        );
        assert_unauthenticated(&response);
    }
    restore_credential(&migrator, ALICE_CREDENTIAL, &alice).await;

    sqlx::query(
        "UPDATE credentials SET issued_at = clock_timestamp() - interval '2 hours',
                                expires_at = clock_timestamp() - interval '1 hour'
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("expire Alice");
    for (method, uri, body) in reader_routes(ALLOWED_ITEM) {
        let (status, response) = send(&app, method, &uri, Some(&alice), body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "expired reader {method} {uri}"
        );
        assert_unauthenticated(&response);
    }
    restore_credential(&migrator, ALICE_CREDENTIAL, &alice).await;

    let mut denials = Vec::new();
    for item in [BOB_PRIVATE_ITEM, FOREIGN_ITEM, MISSING_ITEM] {
        let (status, mut response) =
            send(&app, "GET", &format!("/v1/items/{item}"), Some(&alice), &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "cross-scope GET {item}");
        assert_unavailable(&response);
        response
            .as_object_mut()
            .expect("unavailable object")
            .remove("request_id");
        denials.push(response);
    }
    assert_eq!(denials[0], denials[1]);
    assert_eq!(denials[1], denials[2]);

    let bob_own = send(
        &app,
        "GET",
        &format!("/v1/items/{BOB_PRIVATE_ITEM}"),
        Some(&bob),
        &[],
    )
    .await;
    assert_eq!(bob_own.0, StatusCode::OK);
    assert_eq!(bob_own.1["content"], "FORBIDDEN_BOB_PRIVATE");

    let alice_search_own = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert_eq!(alice_search_own.0, StatusCode::OK);
    assert_eq!(alice_search_own.1["items"][0]["item_id"], ALLOWED_ITEM);
    assert_eq!(
        alice_search_own.1["items"][0]["excerpt"],
        "ALLOWED_ALPHA_HANDBOOK"
    );

    let alice_search_bob = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"FORBIDDEN_BOB_PRIVATE"}"#,
    )
    .await;
    assert_eq!(alice_search_bob.0, StatusCode::OK);
    assert_eq!(alice_search_bob.1["items"], serde_json::json!([]));
    assert!(!alice_search_bob.1.to_string().contains("FORBIDDEN"));

    let create_denied = send(
        &app,
        "POST",
        "/v1/memories",
        Some(&alice),
        format!(
            r#"{{"content":"P1_AGENT_CREATE","subjects":["{ALICE_SUBJECT}"],"collection_id":"30000000000000000000000000000004"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(create_denied.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&create_denied.1);
    assert!(!create_denied.1.to_string().contains("P1_AGENT_CREATE"));

    let correct_denied = send(
        &app,
        "PUT",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&alice),
        br#"{"content":"P1_AGENT_CORRECT","expected_revision_id":"50000000000000000000000000000001","subjects":["s0000000000000000000000000000001"]}"#,
    )
    .await;
    assert_eq!(correct_denied.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&correct_denied.1);

    let forget_denied = send(
        &app,
        "DELETE",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&alice),
        br#"{"expected_revision_id":"50000000000000000000000000000001"}"#,
    )
    .await;
    assert_eq!(forget_denied.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&forget_denied.1);

    let search_only = random_bearer();
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class,
          allowed_operations, issued_at, expires_at)
         VALUES ($1, 'c00000000000000000000000000000p1', $2, $3, $4, 'agent_reader',
                 ARRAY['search'], clock_timestamp(), clock_timestamp() + interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_PRINCIPAL)
    .bind(ALPHA_APP)
    .bind(Sha256::digest(search_only.as_bytes()).as_slice())
    .execute(&migrator)
    .await
    .expect("insert search-only credential");
    let read_with_search_only = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&search_only),
        &[],
    )
    .await;
    assert_eq!(read_with_search_only.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&read_with_search_only.1);
    let list_with_search_only = send(&app, "GET", "/v1/items", Some(&search_only), &[]).await;
    assert_eq!(list_with_search_only.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&list_with_search_only.1);

    sqlx::query("INSERT INTO apps (tenant_id, id, active) VALUES ($1, 'p1_wrong_app', true)")
        .bind(ALPHA_TENANT)
        .execute(&migrator)
        .await
        .expect("insert same-tenant other app");
    let wrong_app = random_bearer();
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class,
          allowed_operations, issued_at, expires_at)
         VALUES ($1, 'c00000000000000000000000000000p2', $2, 'p1_wrong_app', $3, 'agent_reader',
                 ARRAY['list', 'search', 'read'], clock_timestamp(), clock_timestamp() + interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_PRINCIPAL)
    .bind(Sha256::digest(wrong_app.as_bytes()).as_slice())
    .execute(&migrator)
    .await
    .expect("insert wrong-app credential");
    let listed_collections = send(&app, "GET", "/v1/collections", Some(&wrong_app), &[]).await;
    assert_eq!(listed_collections.0, StatusCode::OK);
    assert_eq!(listed_collections.1["items"], serde_json::json!([]));
    let listed_items = send(&app, "GET", "/v1/items", Some(&wrong_app), &[]).await;
    assert_eq!(listed_items.0, StatusCode::OK);
    assert_eq!(listed_items.1["items"], serde_json::json!([]));
    let wrong_app_read = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&wrong_app),
        &[],
    )
    .await;
    assert_eq!(wrong_app_read.0, StatusCode::NOT_FOUND);
    assert_unavailable(&wrong_app_read.1);
    let wrong_app_search = send(
        &app,
        "POST",
        "/v1/search",
        Some(&wrong_app),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert_eq!(wrong_app_search.0, StatusCode::OK);
    assert_eq!(wrong_app_search.1["items"], serde_json::json!([]));
    assert!(!wrong_app_search.1.to_string().contains("ALLOWED_ALPHA"));

    let wrong_writer = random_bearer();
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class,
          allowed_operations, issued_at, expires_at)
         VALUES ($1, 'c00000000000000000000000000000p4', $2, 'p1_wrong_app', $3, 'trusted_writer',
                 ARRAY['create', 'correct', 'forget'], clock_timestamp(), clock_timestamp() + interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_PRINCIPAL)
    .bind(Sha256::digest(wrong_writer.as_bytes()).as_slice())
    .execute(&migrator)
    .await
    .expect("insert wrong-app writer");
    let wrong_app_create = send(
        &app,
        "POST",
        "/v1/memories",
        Some(&wrong_writer),
        format!(r#"{{"content":"P1_WRONG_APP_CREATE","subjects":["{ALICE_SUBJECT}"]}}"#).as_bytes(),
    )
    .await;
    assert_eq!(
        wrong_app_create.0,
        StatusCode::NOT_FOUND,
        "{}",
        wrong_app_create.1
    );
    assert_unavailable(&wrong_app_create.1);
    assert!(!wrong_app_create.1.to_string().contains("P1_WRONG_APP"));
    let wrong_app_correct = send(
        &app,
        "PUT",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&wrong_writer),
        br#"{"content":"P1_WRONG_APP_CORRECT","expected_revision_id":"50000000000000000000000000000001","subjects":["s0000000000000000000000000000001"]}"#,
    )
    .await;
    assert_eq!(wrong_app_correct.0, StatusCode::NOT_FOUND);
    assert_unavailable(&wrong_app_correct.1);
    let wrong_app_forget = send(
        &app,
        "DELETE",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&wrong_writer),
        br#"{"expected_revision_id":"50000000000000000000000000000001"}"#,
    )
    .await;
    assert_eq!(wrong_app_forget.0, StatusCode::NOT_FOUND);
    assert_unavailable(&wrong_app_forget.1);
    let still_allowed = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(still_allowed.0, StatusCode::OK);
    assert_eq!(still_allowed.1["content"], "ALLOWED_ALPHA_HANDBOOK");

    let unknown = random_bearer();
    for (method, uri, body) in public_routes(ALLOWED_ITEM) {
        let (status, response) = send(&app, method, &uri, Some(&unknown), body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "unknown bearer {method} {uri}"
        );
        assert_unauthenticated(&response);
        assert!(!response.to_string().contains("ALLOWED"));
        assert!(!response.to_string().contains("P1_WRITER"));
    }

    let forged_item = format!("/v1/items/{ALLOWED_ITEM}?tenant_id=FORGED_COMPANY");
    for bearer in [None, Some(unknown.as_str())] {
        let (status, response) = send(&app, "GET", &forged_item, bearer, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_unauthenticated(&response);
        assert!(!response.to_string().contains("FORGED"));
    }
    let forged_list = "/v1/items?principal_id=FORGED_LIST_PRINCIPAL&limit=100";
    for bearer in [None, Some(unknown.as_str())] {
        let (status, response) = send(&app, "GET", forged_list, bearer, &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!response.to_string().contains("FORGED_LIST"));
    }
    let forged_search = send(
        &app,
        "POST",
        "/v1/search?tenant_id=FORGED_COMPANY",
        None,
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK","scope":{"subjects":["FORGED_SCOPE_SUBJECT_01"]}}"#,
    )
    .await;
    assert_eq!(forged_search.0, StatusCode::UNAUTHORIZED);
    assert!(!forged_search.1.to_string().contains("FORGED"));
    sqlx::query(
        "UPDATE credentials SET issued_at = clock_timestamp() - interval '2 hours',
                                expires_at = clock_timestamp() - interval '1 hour'
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("expire Alice for forged query");
    let expired_forged = send(&app, "GET", &forged_item, Some(&alice), &[]).await;
    assert_eq!(expired_forged.0, StatusCode::UNAUTHORIZED);
    assert!(!expired_forged.1.to_string().contains("FORGED"));
    restore_credential(&migrator, ALICE_CREDENTIAL, &alice).await;
    let alice_forged_item = send(&app, "GET", &forged_item, Some(&alice), &[]).await;
    assert_eq!(alice_forged_item.0, StatusCode::BAD_REQUEST);
    assert_eq!(alice_forged_item.1["code"], "malformed");
    assert!(!alice_forged_item.1.to_string().contains("FORGED"));
    let alice_forged_list = send(&app, "GET", forged_list, Some(&alice), &[]).await;
    assert_eq!(alice_forged_list.0, StatusCode::BAD_REQUEST);
    assert_eq!(alice_forged_list.1["code"], "malformed");
    assert!(!alice_forged_list.1.to_string().contains("FORGED_LIST"));

    let unknown_scope = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK","scope":{"subjects":["missing_document_scope"]}}"#,
    )
    .await;
    assert_eq!(
        unknown_scope.0,
        StatusCode::BAD_REQUEST,
        "{}",
        unknown_scope.1
    );
    assert_eq!(unknown_scope.1["code"], "malformed");
    assert!(!unknown_scope.1.to_string().contains("ALLOWED_ALPHA"));
    let empty_scope = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK","scope":{"subjects":[]}}"#,
    )
    .await;
    assert_eq!(empty_scope.0, StatusCode::BAD_REQUEST);
    assert_eq!(empty_scope.1["code"], "malformed");

    let list_only = random_bearer();
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class,
          allowed_operations, issued_at, expires_at)
         VALUES ($1, 'c00000000000000000000000000000p3', $2, $3, $4, 'agent_reader',
                 ARRAY['list'], clock_timestamp(), clock_timestamp() + interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_PRINCIPAL)
    .bind(ALPHA_APP)
    .bind(Sha256::digest(list_only.as_bytes()).as_slice())
    .execute(&migrator)
    .await
    .expect("insert list-only credential");
    let search_with_list_only = send(
        &app,
        "POST",
        "/v1/search",
        Some(&list_only),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert_eq!(search_with_list_only.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&search_with_list_only.1);
    let read_with_list_only = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&list_only),
        &[],
    )
    .await;
    assert_eq!(read_with_list_only.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&read_with_list_only.1);

    sqlx::query("UPDATE apps SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ALPHA_APP)
        .execute(&migrator)
        .await
        .expect("deactivate Alpha app");
    for (method, uri, body) in reader_routes(ALLOWED_ITEM) {
        let (status, response) = send(&app, method, &uri, Some(&alice), body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "inactive app {method} {uri}"
        );
        assert_unauthenticated(&response);
    }
    sqlx::query("UPDATE apps SET active=true WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ALPHA_APP)
        .execute(&migrator)
        .await
        .expect("restore Alpha app");

    let created = send(
        &app,
        "POST",
        "/v1/memories",
        Some(&writer),
        format!(r#"{{"content":"P1_WRITER_TARGET","subjects":["{ALICE_SUBJECT}"]}}"#).as_bytes(),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    let item_id = created.1["item_id"]
        .as_str()
        .expect("created item")
        .to_owned();
    let revision_id = created.1["revision_id"]
        .as_str()
        .expect("created revision")
        .to_owned();
    let correct_body = format!(
        r#"{{"content":"P1_WRITER_CORRECT","expected_revision_id":"{revision_id}","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let forget_body = format!(r#"{{"expected_revision_id":"{revision_id}"}}"#);
    let create_body = format!(r#"{{"content":"P1_WRITER_DENIED","subjects":["{ALICE_SUBJECT}"]}}"#);
    for (expired, revoked, label) in [
        (false, true, "revoked writer"),
        (true, false, "expired writer"),
    ] {
        set_writer_validity(&migrator, expired, revoked).await;
        for (method, uri, body) in [
            ("POST", "/v1/memories".to_owned(), create_body.as_bytes()),
            (
                "PUT",
                format!("/v1/items/{item_id}"),
                correct_body.as_bytes(),
            ),
            (
                "DELETE",
                format!("/v1/items/{item_id}"),
                forget_body.as_bytes(),
            ),
        ] {
            let (status, response) = send(&app, method, &uri, Some(&writer), body).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{label} {method} {uri}");
            assert_unauthenticated(&response);
            assert!(!response.to_string().contains("P1_WRITER"));
        }
        let still_current = send(
            &app,
            "GET",
            &format!("/v1/items/{item_id}"),
            Some(&alice),
            &[],
        )
        .await;
        assert_eq!(still_current.0, StatusCode::OK);
        assert_eq!(still_current.1["content"], "P1_WRITER_TARGET");
        restore_credential(&migrator, WRITER_CREDENTIAL, &writer).await;
    }
}

fn public_routes(item_id: &str) -> [(&'static str, String, &'static [u8]); 7] {
    [
        ("GET", "/v1/collections".to_owned(), &[]),
        ("GET", "/v1/items".to_owned(), &[]),
        ("GET", format!("/v1/items/{item_id}"), &[]),
        ("POST", "/v1/search".to_owned(), br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#),
        (
            "POST",
            "/v1/memories".to_owned(),
            br#"{"content":"x","subjects":["s0000000000000000000000000000001"],"collection_id":"30000000000000000000000000000004"}"#,
        ),
        (
            "PUT",
            format!("/v1/items/{item_id}"),
            br#"{"content":"x","expected_revision_id":"50000000000000000000000000000001","subjects":["s0000000000000000000000000000001"]}"#,
        ),
        (
            "DELETE",
            format!("/v1/items/{item_id}"),
            br#"{"expected_revision_id":"50000000000000000000000000000001"}"#,
        ),
    ]
}

fn reader_routes(item_id: &str) -> [(&'static str, String, &'static [u8]); 4] {
    [
        ("GET", "/v1/collections".to_owned(), &[]),
        ("GET", "/v1/items".to_owned(), &[]),
        ("GET", format!("/v1/items/{item_id}"), &[]),
        (
            "POST",
            "/v1/search".to_owned(),
            br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
        ),
    ]
}

fn assert_unauthenticated(body: &Value) {
    assert_eq!(body["code"], "unauthenticated");
    let mut copy = body.clone();
    copy.as_object_mut()
        .expect("error object")
        .remove("request_id");
    assert_eq!(
        copy,
        serde_json::json!({"status": "unauthenticated", "code": "unauthenticated"})
    );
}

fn assert_unavailable(body: &Value) {
    let mut copy = body.clone();
    copy.as_object_mut()
        .expect("error object")
        .remove("request_id");
    assert_eq!(
        copy,
        serde_json::json!({"status": "unavailable", "code": "unavailable"})
    );
}

fn listed_ids(body: &Value, field: &str) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("list items")
        .iter()
        .map(|item| item[field].as_str().expect("list id").to_owned())
        .collect()
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if matches!(method, "POST" | "PUT" | "DELETE") {
        builder = builder.header("content-type", "application/json");
        builder = builder.header("idempotency-key", random_bearer());
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

async fn restore_credential(pool: &PgPool, credential_id: &str, bearer: &str) {
    sqlx::query(
        "UPDATE credentials SET token_digest=$1, revoked_at=NULL,
            issued_at=clock_timestamp(), expires_at=clock_timestamp()+interval '24 hours'
         WHERE tenant_id=$2 AND id=$3",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .bind(ALPHA_TENANT)
    .bind(credential_id)
    .execute(pool)
    .await
    .expect("restore credential");
}

async fn set_writer_validity(pool: &PgPool, expired: bool, revoked: bool) {
    sqlx::query(
        "UPDATE credentials
         SET issued_at = clock_timestamp() - interval '2 hours',
             expires_at = CASE WHEN $3 THEN clock_timestamp() - interval '1 hour'
                               ELSE clock_timestamp() + interval '24 hours' END,
             revoked_at = CASE WHEN $4 THEN clock_timestamp() ELSE NULL END
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .bind(expired)
    .bind(revoked)
    .execute(pool)
    .await
    .expect("set writer validity");
}

async fn setup() -> (PgPool, PgPool, String, String, String) {
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
    (migrator, runtime, alice, bob, writer)
}

async fn index_fixture_revisions(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT r.tenant_id, r.item_id, r.id, to_tsvector('simple', r.content)
         FROM revisions r
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("index fixture revisions for search ACL");
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

fn random_bearer() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness for synthetic credential");
    let mut bearer = String::with_capacity(64);
    for byte in bytes {
        write!(bearer, "{byte:02x}").expect("writing to String");
    }
    bearer
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn p1_http_grants_withdraw_and_forget_hide_current_evidence() {
    let (migrator, runtime, alice, _bob, writer) = setup().await;
    index_fixture_revisions(&migrator).await;
    let app = router(runtime);

    sqlx::query(
        "UPDATE collection_grants SET can_read=false
         WHERE tenant_id=$1 AND collection_id=$2 AND principal_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(RESTRICTED_COLLECTION)
    .bind(ALICE_PRINCIPAL)
    .execute(&migrator)
    .await
    .expect("revoke restricted grant");
    let denied_read = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(denied_read.0, StatusCode::NOT_FOUND);
    assert_unavailable(&denied_read.1);
    let denied_search = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert_eq!(denied_search.0, StatusCode::OK);
    assert_eq!(denied_search.1["items"], serde_json::json!([]));
    let listed = send(&app, "GET", "/v1/items", Some(&alice), &[]).await;
    assert_eq!(listed.0, StatusCode::OK);
    assert!(!listed_ids(&listed.1, "item_id").contains(&ALLOWED_ITEM.to_owned()));
    let denied_collections = send(&app, "GET", "/v1/collections", Some(&alice), &[]).await;
    assert_eq!(denied_collections.0, StatusCode::OK);
    assert!(
        !listed_ids(&denied_collections.1, "collection_id")
            .contains(&RESTRICTED_COLLECTION.to_owned())
    );
    sqlx::query(
        "UPDATE collection_grants SET can_read=true
         WHERE tenant_id=$1 AND collection_id=$2 AND principal_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(RESTRICTED_COLLECTION)
    .bind(ALICE_PRINCIPAL)
    .execute(&migrator)
    .await
    .expect("restore restricted grant");

    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(RESTRICTED_COLLECTION)
    .execute(&migrator)
    .await
    .expect("withdraw restricted collection");
    let withdrawn_read = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(withdrawn_read.0, StatusCode::NOT_FOUND);
    assert_unavailable(&withdrawn_read.1);
    let withdrawn_search = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert_eq!(withdrawn_search.0, StatusCode::OK);
    assert_eq!(withdrawn_search.1["items"], serde_json::json!([]));
    let withdrawn_list = send(&app, "GET", "/v1/items", Some(&alice), &[]).await;
    assert_eq!(withdrawn_list.0, StatusCode::OK);
    assert!(!listed_ids(&withdrawn_list.1, "item_id").contains(&ALLOWED_ITEM.to_owned()));
    let withdrawn_collections = send(&app, "GET", "/v1/collections", Some(&alice), &[]).await;
    assert_eq!(withdrawn_collections.0, StatusCode::OK);
    assert!(
        !listed_ids(&withdrawn_collections.1, "collection_id")
            .contains(&RESTRICTED_COLLECTION.to_owned())
    );
    sqlx::query("UPDATE collections SET withdrawn_at=NULL WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(RESTRICTED_COLLECTION)
        .execute(&migrator)
        .await
        .expect("restore withdrawn collection");

    sqlx::query(
        "INSERT INTO revisions (tenant_id, item_id, id, content, valid_until)
         VALUES ($1, $2, 'p1_expired_allowed_rev', 'ALLOWED_ALPHA_HANDBOOK',
                 clock_timestamp() - interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALLOWED_ITEM)
    .execute(&migrator)
    .await
    .expect("insert expired allowed revision");
    sqlx::query(
        "UPDATE items SET active_revision_id='p1_expired_allowed_rev'
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALLOWED_ITEM)
    .execute(&migrator)
    .await
    .expect("activate expired allowed revision");
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT r.tenant_id, r.item_id, r.id, to_tsvector('simple', r.content)
         FROM revisions r
         WHERE r.tenant_id=$1 AND r.id='p1_expired_allowed_rev'",
    )
    .bind(ALPHA_TENANT)
    .execute(&migrator)
    .await
    .expect("index expired allowed revision");
    let expired_read = send(
        &app,
        "GET",
        &format!("/v1/items/{ALLOWED_ITEM}"),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(expired_read.0, StatusCode::NOT_FOUND);
    assert_unavailable(&expired_read.1);
    let expired_search = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert_eq!(expired_search.0, StatusCode::OK);
    assert_eq!(expired_search.1["items"], serde_json::json!([]));
    let expired_list = send(&app, "GET", "/v1/items", Some(&alice), &[]).await;
    assert_eq!(expired_list.0, StatusCode::OK);
    assert!(!listed_ids(&expired_list.1, "item_id").contains(&ALLOWED_ITEM.to_owned()));
    sqlx::query(
        "UPDATE items SET active_revision_id='50000000000000000000000000000001'
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALLOWED_ITEM)
    .execute(&migrator)
    .await
    .expect("restore allowed current revision");

    let created = send(
        &app,
        "POST",
        "/v1/memories",
        Some(&writer),
        format!(r#"{{"content":"P1_FORGET_BEACON","subjects":["{ALICE_SUBJECT}"]}}"#).as_bytes(),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    let item_id = created.1["item_id"].as_str().expect("item").to_owned();
    let revision_id = created.1["revision_id"]
        .as_str()
        .expect("revision")
        .to_owned();
    let forgotten = send(
        &app,
        "DELETE",
        &format!("/v1/items/{item_id}"),
        Some(&writer),
        format!(r#"{{"expected_revision_id":"{revision_id}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(forgotten.0, StatusCode::OK, "{}", forgotten.1);
    let forgotten_read = send(
        &app,
        "GET",
        &format!("/v1/items/{item_id}"),
        Some(&alice),
        &[],
    )
    .await;
    assert_eq!(forgotten_read.0, StatusCode::NOT_FOUND);
    assert_unavailable(&forgotten_read.1);
    let forgotten_search = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"P1_FORGET_BEACON"}"#,
    )
    .await;
    assert_eq!(forgotten_search.0, StatusCode::OK);
    assert_eq!(forgotten_search.1["items"], serde_json::json!([]));
    assert!(!forgotten_search.1.to_string().contains("P1_FORGET"));
    let forgotten_list = send(&app, "GET", "/v1/items", Some(&alice), &[]).await;
    assert_eq!(forgotten_list.0, StatusCode::OK);
    assert!(!listed_ids(&forgotten_list.1, "item_id").contains(&item_id));
}

#[tokio::test]
async fn p1_http_list_and_search_keep_alice_bob_and_beta_isolated() {
    let (migrator, runtime, alice, bob, _writer) = setup().await;
    index_fixture_revisions(&migrator).await;
    let app = router(runtime);

    let alice_items = send(&app, "GET", "/v1/items", Some(&alice), &[]).await;
    assert_eq!(alice_items.0, StatusCode::OK, "{}", alice_items.1);
    let alice_item_ids = listed_ids(&alice_items.1, "item_id");
    assert!(alice_item_ids.iter().any(|id| id == ALLOWED_ITEM));
    assert!(!alice_item_ids.iter().any(|id| id == BOB_PRIVATE_ITEM));
    assert!(!alice_item_ids.iter().any(|id| id == FOREIGN_ITEM));
    assert!(!alice_items.1.to_string().contains("FORBIDDEN"));

    let bob_items = send(&app, "GET", "/v1/items", Some(&bob), &[]).await;
    assert_eq!(bob_items.0, StatusCode::OK, "{}", bob_items.1);
    let bob_item_ids = listed_ids(&bob_items.1, "item_id");
    assert!(bob_item_ids.iter().any(|id| id == BOB_PRIVATE_ITEM));
    assert!(!bob_item_ids.iter().any(|id| id == ALLOWED_ITEM));
    assert!(!bob_item_ids.iter().any(|id| id == FOREIGN_ITEM));
    assert!(!bob_items.1.to_string().contains("ALLOWED_ALPHA"));
    assert!(!bob_items.1.to_string().contains("FORBIDDEN_BETA"));

    let alice_collections = send(&app, "GET", "/v1/collections", Some(&alice), &[]).await;
    assert_eq!(alice_collections.0, StatusCode::OK);
    let alice_collection_ids = listed_ids(&alice_collections.1, "collection_id");
    assert!(
        alice_collection_ids
            .iter()
            .any(|id| id == RESTRICTED_COLLECTION)
    );
    assert!(
        alice_collection_ids
            .iter()
            .any(|id| id == ALICE_PRIVATE_COLLECTION)
    );
    assert!(
        !alice_collection_ids
            .iter()
            .any(|id| id == BOB_PRIVATE_COLLECTION)
    );

    let bob_collections = send(&app, "GET", "/v1/collections", Some(&bob), &[]).await;
    assert_eq!(bob_collections.0, StatusCode::OK);
    let bob_collection_ids = listed_ids(&bob_collections.1, "collection_id");
    assert!(
        bob_collection_ids
            .iter()
            .any(|id| id == BOB_PRIVATE_COLLECTION)
    );
    assert!(
        !bob_collection_ids
            .iter()
            .any(|id| id == ALICE_PRIVATE_COLLECTION)
    );
    assert!(
        !bob_collection_ids
            .iter()
            .any(|id| id == RESTRICTED_COLLECTION)
    );

    let beta_search = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"FORBIDDEN_BETA_COMPANY"}"#,
    )
    .await;
    assert_eq!(beta_search.0, StatusCode::OK);
    assert_eq!(beta_search.1["items"], serde_json::json!([]));
    assert!(!beta_search.1.to_string().contains("FORBIDDEN"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn p1_hybrid_search_does_not_leak_and_replacing_extraction_hides_old_spans() {
    let (migrator, runtime, alice, bob, _writer) = setup().await;
    let worker = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_database_url(WORKER_URL))
        .await
        .expect("purge worker");
    let app = router(runtime);
    select_generation(&migrator, "p1_acl_hybrid", "p1-model-v1", "p1-recipe-v1").await;
    seed_extracted_item(
        &migrator,
        ALICE_PRIVATE_COLLECTION,
        "p1_alpha_sem_item",
        "p1_alpha_source",
        "p1_alpha_set",
        &["p1-alpha-passage"],
        &["P1_ALPHA_SEMANTIC_BEACON unique alpha span"],
    )
    .await;
    seed_extracted_item(
        &migrator,
        BOB_PRIVATE_COLLECTION,
        "p1_bob_sem_item",
        "p1_bob_source",
        "p1_bob_set",
        &["p1-bob-passage"],
        &["P1_BOB_SEMANTIC_BEACON unique bob span"],
    )
    .await;
    complete_pending_jobs(&worker, "[1,0,0]").await;

    let query = "p1hybridabsentquerytoken";
    let missing_hybrid = send(
        &app,
        "POST",
        "/v1/search",
        None,
        br#"{"query":"p1hybridabsentquerytoken","semantic":true}"#,
    )
    .await;
    assert_eq!(missing_hybrid.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&missing_hybrid.1);

    let alice_hits = hybrid_search(&app, &alice, query, "p1_acl_hybrid", &[1.0, 0.0, 0.0]).await;
    assert_eq!(alice_hits.0, StatusCode::OK, "{}", alice_hits.1);
    let alice_text = alice_hits.1.to_string();
    assert!(
        alice_text.contains("P1_ALPHA_SEMANTIC_BEACON"),
        "{alice_text}"
    );
    assert!(
        !alice_text.contains("P1_BOB_SEMANTIC_BEACON"),
        "{alice_text}"
    );

    let bob_hits = hybrid_search(&app, &bob, query, "p1_acl_hybrid", &[1.0, 0.0, 0.0]).await;
    assert_eq!(bob_hits.0, StatusCode::OK, "{}", bob_hits.1);
    let bob_text = bob_hits.1.to_string();
    assert!(bob_text.contains("P1_BOB_SEMANTIC_BEACON"), "{bob_text}");
    assert!(!bob_text.contains("P1_ALPHA_SEMANTIC_BEACON"), "{bob_text}");

    sqlx::query(
        "INSERT INTO apps (tenant_id, id, active) VALUES ($1, 'p1_hybrid_wrong_app', true)",
    )
    .bind(ALPHA_TENANT)
    .execute(&migrator)
    .await
    .expect("insert hybrid wrong app");
    let wrong_hybrid = random_bearer();
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class,
          allowed_operations, issued_at, expires_at)
         VALUES ($1, 'c00000000000000000000000000000p5', $2, 'p1_hybrid_wrong_app', $3, 'agent_reader',
                 ARRAY['list', 'search', 'read'], clock_timestamp(), clock_timestamp() + interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_PRINCIPAL)
    .bind(Sha256::digest(wrong_hybrid.as_bytes()).as_slice())
    .execute(&migrator)
    .await
    .expect("insert hybrid wrong-app reader");
    let wrong_app_hybrid = hybrid_search(
        &app,
        &wrong_hybrid,
        query,
        "p1_acl_hybrid",
        &[1.0, 0.0, 0.0],
    )
    .await;
    assert_eq!(wrong_app_hybrid.0, StatusCode::OK, "{}", wrong_app_hybrid.1);
    let wrong_app_text = wrong_app_hybrid.1.to_string();
    assert!(
        !wrong_app_text.contains("P1_ALPHA_SEMANTIC_BEACON"),
        "{wrong_app_text}"
    );
    assert!(
        !wrong_app_text.contains("P1_BOB_SEMANTIC_BEACON"),
        "{wrong_app_text}"
    );

    sqlx::query(
        "UPDATE credentials SET revoked_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("revoke Alice for hybrid");
    let revoked = hybrid_search(&app, &alice, query, "p1_acl_hybrid", &[1.0, 0.0, 0.0]).await;
    assert_eq!(revoked.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&revoked.1);
    restore_credential(&migrator, ALICE_CREDENTIAL, &alice).await;

    sqlx::query(
        "UPDATE credentials SET issued_at = clock_timestamp() - interval '2 hours',
                                expires_at = clock_timestamp() - interval '1 hour'
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("expire Alice for hybrid");
    let expired_hybrid =
        hybrid_search(&app, &alice, query, "p1_acl_hybrid", &[1.0, 0.0, 0.0]).await;
    assert_eq!(expired_hybrid.0, StatusCode::UNAUTHORIZED);
    assert_unauthenticated(&expired_hybrid.1);
    restore_credential(&migrator, ALICE_CREDENTIAL, &alice).await;

    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(BOB_PRIVATE_COLLECTION)
    .execute(&migrator)
    .await
    .expect("withdraw Bob collection");
    let withdrawn_bob = hybrid_search(&app, &bob, query, "p1_acl_hybrid", &[1.0, 0.0, 0.0]).await;
    assert_eq!(withdrawn_bob.0, StatusCode::OK, "{}", withdrawn_bob.1);
    assert!(
        !withdrawn_bob
            .1
            .to_string()
            .contains("P1_BOB_SEMANTIC_BEACON")
    );
    sqlx::query("UPDATE collections SET withdrawn_at=NULL WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(BOB_PRIVATE_COLLECTION)
        .execute(&migrator)
        .await
        .expect("restore Bob collection");

    sqlx::query(
        "UPDATE items SET active_revision_id=NULL, deleted_at=clock_timestamp(),
            deletion_generation=deletion_generation+1
         WHERE tenant_id=$1 AND id='p1_alpha_sem_item'",
    )
    .bind(ALPHA_TENANT)
    .execute(&migrator)
    .await
    .expect("forget Alice semantic item");
    let forgotten_hybrid =
        hybrid_search(&app, &alice, query, "p1_acl_hybrid", &[1.0, 0.0, 0.0]).await;
    assert_eq!(forgotten_hybrid.0, StatusCode::OK, "{}", forgotten_hybrid.1);
    assert!(
        !forgotten_hybrid
            .1
            .to_string()
            .contains("P1_ALPHA_SEMANTIC_BEACON")
    );

    seed_extracted_item(
        &migrator,
        ALICE_PRIVATE_COLLECTION,
        "p1_i02_item",
        "p1_i02_source",
        "p1_i02_set_v1",
        &["p1-i02-old"],
        &["P1_OLD_SPAN_MARKER old extraction"],
    )
    .await;
    complete_pending_jobs(&worker, "[0,1,0]").await;
    let old_lex = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"P1_OLD_SPAN_MARKER"}"#,
    )
    .await;
    assert_eq!(old_lex.0, StatusCode::OK, "{}", old_lex.1);
    assert!(old_lex.1.to_string().contains("P1_OLD_SPAN_MARKER"));
    let old_hybrid = hybrid_search(
        &app,
        &alice,
        "p1i02absentquerytoken",
        "p1_acl_hybrid",
        &[0.0, 1.0, 0.0],
    )
    .await;
    assert_eq!(old_hybrid.0, StatusCode::OK, "{}", old_hybrid.1);
    assert!(old_hybrid.1.to_string().contains("P1_OLD_SPAN_MARKER"));

    activate_set(
        &migrator,
        "p1_i02_item",
        "p1_i02_item-r1",
        "p1_i02_source",
        "p1_i02_set_v2",
        &["p1-i02-new"],
        &["P1_NEW_SPAN_MARKER new extraction"],
    )
    .await;
    let incomplete_hybrid = hybrid_search(
        &app,
        &alice,
        "p1i02absentquerytoken",
        "p1_acl_hybrid",
        &[0.0, 1.0, 0.0],
    )
    .await;
    assert_eq!(
        incomplete_hybrid.0,
        StatusCode::OK,
        "{}",
        incomplete_hybrid.1
    );
    let incomplete_text = incomplete_hybrid.1.to_string();
    assert!(
        !incomplete_text.contains("P1_OLD_SPAN_MARKER"),
        "{incomplete_text}"
    );
    assert!(
        !incomplete_text.contains("P1_NEW_SPAN_MARKER"),
        "{incomplete_text}"
    );
    complete_pending_jobs(&worker, "[0,0,1]").await;
    let new_lex = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"P1_OLD_SPAN_MARKER"}"#,
    )
    .await;
    assert_eq!(new_lex.0, StatusCode::OK, "{}", new_lex.1);
    assert!(!new_lex.1.to_string().contains("P1_OLD_SPAN_MARKER"));
    let new_lex_hit = send(
        &app,
        "POST",
        "/v1/search",
        Some(&alice),
        br#"{"query":"P1_NEW_SPAN_MARKER"}"#,
    )
    .await;
    assert_eq!(new_lex_hit.0, StatusCode::OK, "{}", new_lex_hit.1);
    assert!(new_lex_hit.1.to_string().contains("P1_NEW_SPAN_MARKER"));
    let stale_hybrid = hybrid_search(
        &app,
        &alice,
        "p1i02absentquerytoken",
        "p1_acl_hybrid",
        &[0.0, 1.0, 0.0],
    )
    .await;
    assert_eq!(stale_hybrid.0, StatusCode::OK, "{}", stale_hybrid.1);
    assert!(!stale_hybrid.1.to_string().contains("P1_OLD_SPAN_MARKER"));
    let new_hybrid = hybrid_search(
        &app,
        &alice,
        "p1i02absentquerytoken",
        "p1_acl_hybrid",
        &[0.0, 0.0, 1.0],
    )
    .await;
    assert_eq!(new_hybrid.0, StatusCode::OK, "{}", new_hybrid.1);
    assert!(new_hybrid.1.to_string().contains("P1_NEW_SPAN_MARKER"));
}

async fn hybrid_search(
    app: &axum::Router,
    bearer: &str,
    query: &str,
    generation: &str,
    vector: &[f32],
) -> (StatusCode, Value) {
    let body = serde_json::json!({
        "query": query,
        "semantic": true,
        "semantic_query": {
            "generation_id": generation,
            "model_version": "p1-model-v1",
            "input_recipe_version": "p1-recipe-v1",
            "dimensions": vector.len(),
            "original_query_sha256": hex(&Sha256::digest(query.as_bytes())),
            "vector": vector
        },
        "max_context_bytes": 4096
    })
    .to_string();
    send(app, "POST", "/v1/search", Some(bearer), body.as_bytes()).await
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

async fn seed_extracted_item(
    db: &PgPool,
    collection: &str,
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
    .bind(collection)
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

async fn complete_pending_jobs(worker: &PgPool, vector: &str) {
    loop {
        let jobs = sqlx::query("SELECT * FROM claim_embedding_jobs(16,60)")
            .fetch_all(worker)
            .await
            .expect("claim jobs");
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            let outcome: String = sqlx::query_scalar("SELECT complete_embedding_job($1,$2,$3,$4)")
                .bind(job.get::<String, _>("tenant_id"))
                .bind(job.get::<String, _>("job_id"))
                .bind(job.get::<i64, _>("attempt"))
                .bind(vector)
                .fetch_one(worker)
                .await
                .expect("complete job");
            assert_eq!(outcome, "complete");
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut value, byte| {
        write!(value, "{byte:02x}").expect("hex nibble");
        value
    })
}
