use axum::{
    body::{Body, Bytes, HttpBody, to_bytes},
    http::{Request, StatusCode},
};
use governed_memory::router;
use http_body::Frame;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{fmt::Write, pin::Pin, task::Poll};
use tower::ServiceExt;

const MIGRATOR_URL: &str =
    "postgres://agentic_memory_migrator:synthetic-migrator-only@127.0.0.1:55432/agentic_memory";
const RUNTIME_URL: &str =
    "postgres://agentic_memory_runtime:synthetic-runtime-only@127.0.0.1:55432/agentic_memory";
const PURGE_WORKER_URL: &str = "postgres://agentic_memory_purge_worker:synthetic-purge-worker-only@127.0.0.1:55432/agentic_memory";
const ALPHA_TENANT: &str = "00000000000000000000000000000001";
const BETA_TENANT: &str = "00000000000000000000000000000002";
const ALLOWED_ITEM: &str = "40000000000000000000000000000001";
const BOB_PRIVATE_ITEM: &str = "40000000000000000000000000000002";
const FOREIGN_ITEM: &str = "40000000000000000000000000000003";
const MISSING_ITEM: &str = "40000000000000000000000000000004";
const ALICE_CREDENTIAL: &str = "c0000000000000000000000000000001";
const WRITER_CREDENTIAL: &str = "c0000000000000000000000000000003";
const ALICE_SUBJECT: &str = "s0000000000000000000000000000001";
const BOB_SUBJECT: &str = "s0000000000000000000000000000002";

fn test_database_url(url: &str) -> String {
    match option_env!("MEMORY_TEST_PG_PORT") {
        None | Some("55432") => url.to_owned(),
        Some("55433") => url.replace("127.0.0.1:55432", "127.0.0.1:55433"),
        Some("55434") => url.replace("127.0.0.1:55432", "127.0.0.1:55434"),
        Some(_) => panic!("unsupported synthetic test database port"),
    }
}

#[tokio::test]
#[ignore = "owned empty token-budget test fixture 55456 only"]
#[allow(clippy::too_many_lines)]
async fn search_token_budget_is_enforced_locally() {
    let migrator = PgPoolOptions::new().max_connections(2)
        .connect("postgres://agentic_memory_migrator:synthetic-migrator-only@127.0.0.1:55456/n4_fixed_long_context_v1")
        .await.expect("owned token fixture");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&migrator)
        .await
        .unwrap();
    assert_eq!(count, 2, "reuse only seeded red fixture; never reset");
    let original: String = sqlx::query_scalar("SELECT content FROM revisions WHERE tenant_id=$1 AND item_id=$2 AND id='50000000000000000000000000000001'")
        .bind(ALPHA_TENANT).bind(ALLOWED_ITEM).fetch_one(&migrator).await.unwrap();
    assert_eq!(original, "ALLOWED_ALPHA_HANDBOOK");
    let runtime = PgPoolOptions::new().max_connections(2)
        .connect("postgres://agentic_memory_runtime:synthetic-runtime-only@127.0.0.1:55456/n4_fixed_long_context_v1")
        .await.expect("restricted token runtime");
    let reader = random_bearer();
    let updated=sqlx::query("UPDATE credentials SET token_digest=$1, revoked_at=NULL, expires_at=clock_timestamp()+interval '24 hours' WHERE tenant_id=$2 AND id=$3 AND credential_class='agent_reader'")
        .bind(Sha256::digest(reader.as_bytes()).as_slice()).bind(ALPHA_TENANT).bind(ALICE_CREDENTIAL)
        .execute(&migrator).await.unwrap();
    assert_eq!(updated.rows_affected(), 1);
    let app = router(runtime);
    let core = tiktoken_rs::o200k_base().unwrap();
    let request = |query: &str, limit: u16| {
        serde_json::json!({"query":query,"context_token_budget":{"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":limit}}).to_string()
    };
    let check = |body: &Value, limit: u16| {
        let text = body["context"]["text"].as_str().unwrap();
        let count = core
            .encode(text, &std::collections::HashSet::new())
            .unwrap()
            .0
            .len();
        assert_eq!(body["context"]["token_count"], count);
        assert!(count <= usize::from(limit));
        let evidence: Value = serde_json::from_str(text).unwrap();
        let expected=body["items"].as_array().unwrap().iter().map(|item| serde_json::json!({"item_id":item["item_id"],"revision_id":item["revision_id"],"text":item["excerpt"],"citation":item.get("citation").unwrap_or(&Value::Null)})).collect::<Vec<_>>();
        assert_eq!(evidence, serde_json::json!({"evidence":expected}));
    };
    let result = search(
        &app,
        Some(&reader),
        request("ALLOWED_ALPHA_HANDBOOK", 128).as_bytes(),
    )
    .await;
    assert_eq!(result.0, StatusCode::OK);
    check(&result.1, 128);
    let plain = search(
        &app,
        Some(&reader),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK"}"#,
    )
    .await;
    assert!(plain.1.get("context").is_none());
    for query in ["FORBIDDEN_BOB_PRIVATE", "FORBIDDEN_BETA_COMPANY"] {
        let response = search(&app, Some(&reader), request(query, 1024).as_bytes()).await;
        assert_eq!(response.0, StatusCode::OK);
        check(&response.1, 1024);
        assert!(
            response.1["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["item_id"] != BOB_PRIVATE_ITEM && item["item_id"] != FOREIGN_ITEM)
        );
    }
    let contents = vec![
        format!("TOKENWHOLEV4 English {}", "word ".repeat(140)),
        format!("TOKENWHOLEV4 中文 {}", "中文".repeat(140)),
        format!(
            "TOKENWHOLEV4 mixed {} <|endoftext|>",
            "English中文 ".repeat(70)
        ),
    ];
    seed_search_item_revisions(
        &migrator,
        "30000000000000000000000000000001",
        "token-whole-item-v4",
        &["token-whole-r4"],
    )
    .await;
    activate_search_extraction(
        &migrator,
        "token-whole-item-v4",
        "token-whole-r4",
        "token-whole-source-v4",
        &Sha256::digest(b"token-whole-source-v4"),
        "token-whole-set-v4",
        &contents,
    )
    .await;
    let full = search(
        &app,
        Some(&reader),
        request("TOKENWHOLEV4", 4096).as_bytes(),
    )
    .await;
    assert_eq!(full.0, StatusCode::OK);
    check(&full.1, 4096);
    let returned = full.1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["excerpt"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(returned, contents.iter().map(String::as_str).collect());
    let small = search(&app, Some(&reader), request("TOKENWHOLEV4", 128).as_bytes()).await;
    assert_eq!(small.0, StatusCode::OK);
    check(&small.1, 128);
    assert!(small.1["items"].as_array().unwrap().len() < 3);
    assert_eq!(small.1["truncated"], true);
    // Four earlier token-rejected spans cannot consume the source's four credits.
    let mut credit_contents = (0..4)
        .map(|_| format!("{} {}", "TOKENCREDITV4 ".repeat(20), "中".repeat(400)))
        .collect::<Vec<_>>();
    credit_contents.push("TOKENCREDITV4 small complete span".into());
    seed_search_item_revisions(
        &migrator,
        "30000000000000000000000000000001",
        "token-credit-item-v4",
        &["token-credit-r4"],
    )
    .await;
    activate_search_extraction(
        &migrator,
        "token-credit-item-v4",
        "token-credit-r4",
        "token-credit-source-v4",
        &Sha256::digest(b"token-credit-source-v4"),
        "token-credit-set-v4",
        &credit_contents,
    )
    .await;
    let control = search(
        &app,
        Some(&reader),
        br#"{"query":"TOKENCREDITV4","max_context_bytes":16384}"#,
    )
    .await;
    assert_eq!(control.0, StatusCode::OK);
    assert_eq!(control.1["items"].as_array().unwrap().len(), 4);
    assert!(
        control.1["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["excerpt"].as_str().unwrap().len() > 1000)
    );
    let token = search(
        &app,
        Some(&reader),
        request("TOKENCREDITV4", 220).as_bytes(),
    )
    .await;
    assert_eq!(token.0, StatusCode::OK);
    check(&token.1, 220);
    assert_eq!(token.1["items"].as_array().unwrap().len(), 1);
    assert_eq!(token.1["items"][0]["excerpt"], credit_contents[4]);
    // Manual records also remain whole only in the opt-in path.
    let manual = format!("TOKENMANUALV4 {}", "English中文 ".repeat(80));
    let mut seed = migrator.begin().await.unwrap();
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,'token-manual-item-v4','30000000000000000000000000000001')").bind(ALPHA_TENANT).execute(&mut *seed).await.unwrap();
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,'token-manual-item-v4','token-manual-r4',$2)").bind(ALPHA_TENANT).bind(&manual).execute(&mut *seed).await.unwrap();
    sqlx::query("UPDATE items SET active_revision_id='token-manual-r4' WHERE tenant_id=$1 AND id='token-manual-item-v4'").bind(ALPHA_TENANT).execute(&mut *seed).await.unwrap();
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
         VALUES ($1,'token-manual-item-v4','token-manual-r4',to_tsvector('simple',$2))",
    )
    .bind(ALPHA_TENANT)
    .bind(&manual)
    .execute(&mut *seed)
    .await
    .unwrap();
    seed.commit().await.unwrap();
    let whole = search(
        &app,
        Some(&reader),
        request("TOKENMANUALV4", 4096).as_bytes(),
    )
    .await;
    assert_eq!(whole.0, StatusCode::OK);
    check(&whole.1, 4096);
    assert_eq!(whole.1["items"][0]["excerpt"], manual);
    let legacy = search(&app, Some(&reader), br#"{"query":"TOKENMANUALV4"}"#).await;
    assert!(legacy.1["items"][0]["excerpt"].as_str().unwrap().len() <= 512);
    // A replacement extraction is authoritative; tokenization cannot restore old spans.
    activate_search_extraction(
        &migrator,
        "token-whole-item-v4",
        "token-whole-r4",
        "token-whole-source-v4",
        &Sha256::digest(b"token-whole-source-v4"),
        "token-new-set-v4",
        &["TOKENCURRENTV4 new supported passage".into()],
    )
    .await;
    let stale = search(
        &app,
        Some(&reader),
        request("TOKENWHOLEV4", 4096).as_bytes(),
    )
    .await;
    assert_eq!(stale.0, StatusCode::OK);
    assert!(stale.1["items"].as_array().unwrap().is_empty());
    let current = search(
        &app,
        Some(&reader),
        request("TOKENCURRENTV4", 4096).as_bytes(),
    )
    .await;
    assert_eq!(current.0, StatusCode::OK);
    check(&current.1, 4096);
    assert_eq!(
        current.1["items"][0]["citation"]["extraction_set_id"],
        "token-new-set-v4"
    );
    for (value, code) in [
        (
            serde_json::json!({"tokenizer":"other","max_tokens":128}),
            "unsupported_context_tokenizer",
        ),
        (
            serde_json::json!({"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":0}),
            "invalid_context_budget",
        ),
        (
            serde_json::json!({"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":1}),
            "invalid_context_budget",
        ),
        (Value::Null, "malformed"),
        (
            serde_json::json!({"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":65536}),
            "malformed",
        ),
    ] {
        let body = serde_json::json!({"query":"q","context_token_budget":value}).to_string();
        let bad = search(&app, Some(&reader), body.as_bytes()).await;
        assert_eq!(bad.0, StatusCode::BAD_REQUEST);
        assert_eq!(bad.1["code"], code);
        assert_eq!(
            search(&app, None, body.as_bytes()).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    sqlx::query("UPDATE credentials SET revoked_at=now() WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ALICE_CREDENTIAL)
        .execute(&migrator)
        .await
        .unwrap();
    assert_eq!(
        search(
            &app,
            Some(&reader),
            request("TOKENWHOLEV4", 4096).as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
#[ignore = "owned empty token-primary test fixture 55458 only"]
#[allow(clippy::too_many_lines)]
async fn search_token_primary_omitted_bytes_uses_operator_max() {
    let migrator = PgPoolOptions::new()
        .max_connections(2)
        .connect("postgres://agentic_memory_migrator:synthetic-migrator-only@127.0.0.1:55458/n4_fixed_long_context_v1")
        .await
        .expect("owned token-primary fixture");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&migrator)
        .await
        .unwrap();
    let reader = random_bearer();
    if count == 0 {
        let mut transaction = migrator.begin().await.expect("seed token-primary fixture");
        sqlx::raw_sql(include_str!("fixtures/reset.sql"))
            .execute(&mut *transaction)
            .await
            .expect("initialize empty 55458");
        insert_reader(
            &mut transaction,
            ALICE_CREDENTIAL,
            "10000000000000000000000000000001",
            &reader,
        )
        .await;
        sqlx::query(
            "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
             SELECT tenant_id,item_id,id,to_tsvector('simple',content) FROM revisions",
        )
        .execute(&mut *transaction)
        .await
        .expect("index seeded revisions");
        transaction.commit().await.expect("commit 55458 seed");
    } else {
        assert_eq!(count, 2, "reuse only seeded 55458; never reset");
        let updated = sqlx::query("UPDATE credentials SET token_digest=$1, revoked_at=NULL, expires_at=clock_timestamp()+interval '24 hours' WHERE tenant_id=$2 AND id=$3 AND credential_class='agent_reader'")
            .bind(Sha256::digest(reader.as_bytes()).as_slice())
            .bind(ALPHA_TENANT)
            .bind(ALICE_CREDENTIAL)
            .execute(&migrator)
            .await
            .unwrap();
        assert_eq!(updated.rows_affected(), 1);
    }
    let runtime = PgPoolOptions::new()
        .max_connections(2)
        .connect("postgres://agentic_memory_runtime:synthetic-runtime-only@127.0.0.1:55458/n4_fixed_long_context_v1")
        .await
        .expect("restricted token-primary runtime");
    let app = router(runtime);
    let contents = (0..4)
        .map(|i| format!("TOKENPRIMARY {i} {}", "block ".repeat(500)))
        .collect::<Vec<_>>();
    assert!(contents.iter().map(String::len).sum::<usize>() > 8192);
    assert!(contents.iter().map(String::len).sum::<usize>() < 16384);
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM items WHERE tenant_id=$1 AND id='token-primary-item-v2')",
    )
    .bind(ALPHA_TENANT)
    .fetch_one(&migrator)
    .await
    .unwrap();
    if !exists {
        seed_search_item_revisions(
            &migrator,
            "30000000000000000000000000000001",
            "token-primary-item-v2",
            &["token-primary-r2"],
        )
        .await;
        activate_search_extraction(
            &migrator,
            "token-primary-item-v2",
            "token-primary-r2",
            "token-primary-source-v2",
            &Sha256::digest(b"token-primary-source-v2"),
            "token-primary-set-v2",
            &contents,
        )
        .await;
    }
    let core = tiktoken_rs::o200k_base().unwrap();
    let omitted = search(
        &app,
        Some(&reader),
        br#"{"query":"TOKENPRIMARY","context_token_budget":{"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":8192}}"#,
    )
    .await;
    assert_eq!(omitted.0, StatusCode::OK);
    let omitted_bytes = omitted.1["context_bytes"].as_u64().unwrap();
    let text = omitted.1["context"]["text"].as_str().unwrap();
    let count = core
        .encode(text, &std::collections::HashSet::new())
        .unwrap()
        .0
        .len();
    assert_eq!(omitted.1["context"]["token_count"], count);
    assert!(count <= 8192);
    assert!(
        omitted_bytes > 8192,
        "token-primary omitted bytes should use operator max, not 8KB; context_bytes={} items={}",
        omitted_bytes,
        omitted.1["items"].as_array().map_or(0, Vec::len)
    );
    assert!(omitted_bytes <= 16384);
    assert_eq!(omitted.1["items"].as_array().unwrap().len(), 4);
    let dual = search(
        &app,
        Some(&reader),
        br#"{"query":"TOKENPRIMARY","max_context_bytes":8192,"context_token_budget":{"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":8192}}"#,
    )
    .await;
    assert_eq!(dual.0, StatusCode::OK);
    assert!(dual.1["context_bytes"].as_u64().unwrap() <= 8192);
    assert!(dual.1["items"].as_array().unwrap().len() < 4);
    let byte_only = search(&app, Some(&reader), br#"{"query":"TOKENPRIMARY"}"#).await;
    assert_eq!(byte_only.0, StatusCode::OK);
    assert!(byte_only.1.get("context").is_none());
    assert!(byte_only.1["context_bytes"].as_u64().unwrap() <= 8192);
    for query in ["FORBIDDEN_BOB_PRIVATE", "FORBIDDEN_BETA_COMPANY"] {
        let body = serde_json::json!({"query":query,"context_token_budget":{"tokenizer":"o200k_base:tiktoken-rs-0.12.0","max_tokens":1024}}).to_string();
        let response = search(&app, Some(&reader), body.as_bytes()).await;
        assert_eq!(response.0, StatusCode::OK);
        assert!(
            response.1["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["item_id"] != BOB_PRIVATE_ITEM && item["item_id"] != FOREIGN_ITEM)
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn search_clause_union_recovers_and_blocked_distinctive_spans() {
    let (migrator, runtime, _purge, alice, _bob, _writer) = setup().await;
    seed_search_item_revisions(
        &migrator,
        "30000000000000000000000000000001",
        "clause-en-item",
        &["clause-en-r1"],
    )
    .await;
    let en_target =
        "Current sample-bottle plan: use one 0.75-litre bottle at each shoreline station."
            .to_string();
    let en_decoy =
        "April briefing for people who have only seen the Rowan booking messages.".to_string();
    activate_search_extraction(
        &migrator,
        "clause-en-item",
        "clause-en-r1",
        "clause-en-source",
        &Sha256::digest(b"clause-en-source"),
        "clause-en-set",
        &[en_decoy.clone(), en_target.clone()],
    )
    .await;
    seed_search_item_revisions(
        &migrator,
        "30000000000000000000000000000001",
        "clause-zh-item",
        &["clause-zh-r1"],
    )
    .await;
    let zh_target = "C4包包含逐句字幕主文件、预录字幕备用文件和一份版本核对表。".to_string();
    let zh_decoy = "石桥厅旧海报草稿与仓库群里的两家剧社消息。".to_string();
    activate_search_extraction(
        &migrator,
        "clause-zh-item",
        "clause-zh-r1",
        "clause-zh-source",
        &Sha256::digest(b"clause-zh-source"),
        "clause-zh-set",
        &[zh_decoy.clone(), zh_target.clone()],
    )
    .await;

    let en_question = concat!(
        "I'm preparing the briefing for people who have only seen the April plan or the shared Rowan booking messages. ",
        "Also state the current sample-bottle plan, including duplicates."
    );
    let zh_question = concat!(
        "我手上有石桥厅旧海报草稿、仓库群里的两家剧社消息。",
        "当前应使用哪套简体中文字幕。"
    );

    let mut transaction = runtime.begin().await.expect("begin clause-union search");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(alice.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set clause-union digest");
    set_local(&mut transaction, "app.operation", "search").await;
    set_local(&mut transaction, "app.tenant_id", ALPHA_TENANT).await;

    let v1_en =
        sqlx::query("SELECT passage_id, content FROM search_current_memories($1,$2,$3,$4,$5,$6)")
            .bind(ALPHA_TENANT)
            .bind(ALICE_CREDENTIAL)
            .bind("00000000-0000-0000-0000-000000000801")
            .bind(en_question)
            .bind(4096_i32)
            .bind(Option::<Vec<String>>::None)
            .fetch_all(&mut *transaction)
            .await
            .expect("bounded v1 english search");
    assert!(
        v1_en
            .iter()
            .all(|row| row.get::<String, _>("content") != en_target),
        "full-question AND must not retrieve the distinctive sample-bottle span"
    );

    let union_en = sqlx::query(
        "SELECT passage_id, content FROM search_current_memories_clause_union_v1($1,$2,$3,$4,$5,$6)",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .bind("00000000-0000-0000-0000-000000000802")
    .bind(en_question)
    .bind(4096_i32)
    .bind(Option::<Vec<String>>::None)
    .fetch_all(&mut *transaction)
    .await
    .expect("clause-union english search");
    assert!(
        union_en
            .iter()
            .any(|row| row.get::<String, _>("content") == en_target),
        "clause-union must recover the sample-bottle span"
    );

    let v1_zh =
        sqlx::query("SELECT passage_id, content FROM search_current_memories($1,$2,$3,$4,$5,$6)")
            .bind(ALPHA_TENANT)
            .bind(ALICE_CREDENTIAL)
            .bind("00000000-0000-0000-0000-000000000803")
            .bind(zh_question)
            .bind(4096_i32)
            .bind(Option::<Vec<String>>::None)
            .fetch_all(&mut *transaction)
            .await
            .expect("bounded v1 chinese search");
    assert!(
        v1_zh
            .iter()
            .all(|row| row.get::<String, _>("content") != zh_target),
        "clause-string AND must not retrieve the C4 subtitle span"
    );

    let union_zh = sqlx::query(
        "SELECT passage_id, content FROM search_current_memories_clause_union_v1($1,$2,$3,$4,$5,$6)",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .bind("00000000-0000-0000-0000-000000000804")
    .bind(zh_question)
    .bind(4096_i32)
    .bind(Option::<Vec<String>>::None)
    .fetch_all(&mut *transaction)
    .await
    .expect("clause-union chinese search");
    assert!(
        union_zh
            .iter()
            .any(|row| row.get::<String, _>("content") == zh_target),
        "han-bigram clause-union must recover the C4 subtitle span"
    );

    transaction
        .commit()
        .await
        .expect("commit clause-union searches");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn search_clause_union_recovers_han_comma_split_subtitle_span() {
    let (migrator, runtime, _purge, alice, _bob, _writer) = setup().await;
    seed_search_item_revisions(
        &migrator,
        "30000000000000000000000000000001",
        "clause-zh-comma-item",
        &["clause-zh-comma-r1"],
    )
    .await;
    let zh_target = "C4包包含逐句字幕主文件、预录字幕备用文件和一份版本核对表。".to_string();
    let zh_decoys = [
        "石桥厅旧海报草稿仍贴在仓库墙上，剧社巡演简报也放在导演桌上。",
        "仓库群里的两家剧社消息写进巡演简报，导演核对旧海报草稿。",
        "巡演简报和导演备注放在仓库，剧社海报草稿尚未回收。",
        "旧海报草稿、剧社消息和仓库巡演简报由导演汇总。",
        "导演把石桥厅海报草稿和仓库剧社巡演简报一起归档。",
        "两家剧社在仓库传阅海报草稿和巡演简报，导演未改场次。",
        "海报、剧社、仓库、巡演、简报、导演六项仍按旧稿执行。",
        "石桥厅仓库剧社海报巡演简报导演备注的合订本。",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let mut zh_contents = zh_decoys;
    zh_contents.push(zh_target.clone());
    activate_search_extraction(
        &migrator,
        "clause-zh-comma-item",
        "clause-zh-comma-r1",
        "clause-zh-comma-source",
        &Sha256::digest(b"clause-zh-comma-source"),
        "clause-zh-comma-set",
        &zh_contents,
    )
    .await;

    let zh_question = concat!(
        "我手上有石桥厅旧海报草稿、仓库群里的两家剧社消息，",
        "还有巡演简报和导演备注，",
        "当前应使用哪套简体中文字幕。"
    );
    let subtitle_at = zh_question
        .find("字幕")
        .expect("zh-q1-shaped question includes the subtitle ask");
    assert!(
        !zh_question[..subtitle_at].contains('。'),
        "the subtitle ask must sit in a later comma clause with no period before it"
    );
    assert_eq!(zh_question.matches('。').count(), 1);
    assert!(zh_question.ends_with('。'));

    let clause_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lexical_clause_queries_v1($1)")
            .bind(zh_question)
            .fetch_one(&migrator)
            .await
            .expect("migrator may inspect clause split; runtime cannot");
    assert_eq!(
        clause_count, 4,
        "ideographic and enumeration commas must yield four Han clauses"
    );
    let two_char: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lexical_clause_queries_v1(E'甲乙，丙丁。')")
            .fetch_one(&migrator)
            .await
            .expect("migrator two-character comma split");
    assert_eq!(two_char, 2, "comma split must emit two Han bigram clauses");

    let mut transaction = runtime.begin().await.expect("begin han-comma search");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(alice.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set han-comma digest");
    set_local(&mut transaction, "app.operation", "search").await;
    set_local(&mut transaction, "app.tenant_id", ALPHA_TENANT).await;

    let v1_zh =
        sqlx::query("SELECT passage_id, content FROM search_current_memories($1,$2,$3,$4,$5,$6)")
            .bind(ALPHA_TENANT)
            .bind(ALICE_CREDENTIAL)
            .bind("00000000-0000-0000-0000-000000000811")
            .bind(zh_question)
            .bind(4096_i32)
            .bind(Option::<Vec<String>>::None)
            .fetch_all(&mut *transaction)
            .await
            .expect("bounded v1 han-comma search");
    assert!(
        v1_zh
            .iter()
            .all(|row| row.get::<String, _>("content") != zh_target),
        "v1 AND must not retrieve the C4 subtitle span from a comma-joined Han question"
    );

    let union_zh = sqlx::query(
        "SELECT passage_id, content FROM search_current_memories_clause_union_v1($1,$2,$3,$4,$5,$6)",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .bind("00000000-0000-0000-0000-000000000812")
    .bind(zh_question)
    .bind(4096_i32)
    .bind(Option::<Vec<String>>::None)
    .fetch_all(&mut *transaction)
    .await
    .expect("clause-union han-comma search");
    assert!(
        union_zh
            .iter()
            .any(|row| row.get::<String, _>("content") == zh_target),
        "comma-split Han clause-union must recover the C4 subtitle span inside local k=4"
    );

    transaction
        .commit()
        .await
        .expect("commit han-comma searches");
}

#[tokio::test]
#[ignore = "requires exclusively owned budget test fixture on port 55454"]
#[allow(clippy::too_many_lines)] // One isolated public contract, preserving the red fixture.
async fn search_context_budget_accepts_explicit_eight_kib() {
    let migrator = PgPoolOptions::new()
        .max_connections(2)
        .connect("postgres://agentic_memory_migrator:synthetic-migrator-only@127.0.0.1:55454/n4_fixed_long_context_v1")
        .await
        .expect("owned budget test fixture");
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&migrator)
        .await
        .expect("owned fixture emptiness");
    assert_eq!(
        tenants, 2,
        "green requires exact previously initialized red fixture"
    );
    let original: String = sqlx::query_scalar("SELECT content FROM revisions WHERE tenant_id=$1 AND item_id=$2 AND id='50000000000000000000000000000001'")
        .bind(ALPHA_TENANT).bind(ALLOWED_ITEM).fetch_one(&migrator).await.expect("red fixture identity");
    assert_eq!(original, "ALLOWED_ALPHA_HANDBOOK");
    let runtime = PgPoolOptions::new()
        .max_connections(2)
        .connect("postgres://agentic_memory_runtime:synthetic-runtime-only@127.0.0.1:55454/n4_fixed_long_context_v1")
        .await
        .expect("owned restricted runtime");
    let mut transaction = migrator.begin().await.expect("fixture transaction");
    let reader = random_bearer();
    let updated = sqlx::query("UPDATE credentials SET token_digest=$1 WHERE tenant_id=$2 AND id=$3 AND credential_class='agent_reader'")
        .bind(Sha256::digest(reader.as_bytes()).as_slice()).bind(ALPHA_TENANT).bind(ALICE_CREDENTIAL)
        .execute(&mut *transaction).await.expect("rotate dedicated test credential only");
    assert_eq!(updated.rows_affected(), 1);
    transaction.commit().await.expect("fixture commit");
    let app = router(runtime.clone());
    let result = search(
        &app,
        Some(&reader),
        br#"{"query":"ALLOWED_ALPHA_HANDBOOK","max_context_bytes":8192}"#,
    )
    .await;
    assert_eq!(result.0, StatusCode::OK, "explicit 8192-byte allowance");
    assert!(result.1["context_bytes"].as_u64().unwrap() <= 8192);
    let contents = vec![
        format!("BUDGETWHOLE {}", "中".repeat(1400)),
        format!("BUDGETWHOLE {}", "文".repeat(1000)),
    ];
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM items WHERE tenant_id=$1 AND id='budget-whole-item')",
    )
    .bind(ALPHA_TENANT)
    .fetch_one(&migrator)
    .await
    .unwrap();
    if !exists {
        seed_search_item_revisions(
            &migrator,
            "30000000000000000000000000000001",
            "budget-whole-item",
            &["budget-whole-r1"],
        )
        .await;
        activate_search_extraction(
            &migrator,
            "budget-whole-item",
            "budget-whole-r1",
            "budget-whole-source",
            &Sha256::digest(b"budget-whole-source"),
            "budget-whole-set",
            &contents,
        )
        .await;
    }
    let large = search(
        &app,
        Some(&reader),
        br#"{"query":"BUDGETWHOLE","max_context_bytes":8192}"#,
    )
    .await;
    assert_eq!(large.0, StatusCode::OK);
    assert_eq!(
        large.1["context_bytes"],
        contents.iter().map(String::len).sum::<usize>()
    );
    assert!(large.1["context_bytes"].as_u64().unwrap() > 4096);
    let returned = large.1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["excerpt"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(returned, contents.iter().map(String::as_str).collect());
    let legacy = governed_memory::router_with_search_limits(
        runtime.clone(),
        governed_memory::SearchLimits::new(4096, 4096).unwrap(),
    );
    for mode in ["bounded_v1", "disjunctive_v2", "disjunctive_v3"] {
        let body = serde_json::json!({"query":"ALLOWED_ALPHA_HANDBOOK","max_context_bytes":4096,"query_mode":mode}).to_string();
        let mut old = search(&legacy, Some(&reader), body.as_bytes()).await;
        let mut current = search(&app, Some(&reader), body.as_bytes()).await;
        assert_eq!(old.0, StatusCode::OK);
        old.1["request_id"] = Value::Null;
        current.1["request_id"] = Value::Null;
        assert_eq!(old, current);
    }
    for body in [
        r#"{"query":"q","max_context_bytes":null}"#,
        r#"{"query":"q","max_context_bytes":0}"#,
        r#"{"query":"q","max_context_bytes":16385}"#,
        r#"{"query":"q","max_context_bytes":1.5}"#,
    ] {
        assert_eq!(
            search(&app, Some(&reader), body.as_bytes()).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            search(&app, None, body.as_bytes()).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        search(
            &legacy,
            Some(&reader),
            br#"{"query":"q","max_context_bytes":4097}"#
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut omitted = search(&app, Some(&reader), br#"{"query":"BUDGETWHOLE"}"#).await;
    let mut explicit = large;
    omitted.1["request_id"] = Value::Null;
    explicit.1["request_id"] = Value::Null;
    assert_eq!(
        omitted, explicit,
        "omitted8192 must retain more than4096 bytes"
    );
    let configured = governed_memory::router_with_search_limits(
        runtime.clone(),
        governed_memory::SearchLimits::new(6000, 8192).unwrap(),
    );
    let mut configured_omitted =
        search(&configured, Some(&reader), br#"{"query":"BUDGETWHOLE"}"#).await;
    let mut configured_explicit = search(
        &configured,
        Some(&reader),
        br#"{"query":"BUDGETWHOLE","max_context_bytes":6000}"#,
    )
    .await;
    assert_eq!(configured_omitted.0, StatusCode::OK);
    assert!(
        configured_omitted.1["context_bytes"].as_u64().unwrap()
            < u64::try_from(contents.iter().map(String::len).sum::<usize>()).unwrap()
    );
    configured_omitted.1["request_id"] = Value::Null;
    configured_explicit.1["request_id"] = Value::Null;
    assert_eq!(configured_omitted, configured_explicit);
    assert_ne!(configured_omitted, omitted);
    assert_eq!(
        search(
            &configured,
            Some(&reader),
            br#"{"query":"BUDGETWHOLE","max_context_bytes":8193}"#
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        search(
            &app,
            Some(&reader),
            br#"{"query":"BUDGETWHOLE","max_context_bytes":16384}"#
        )
        .await
        .0,
        StatusCode::OK
    );
    let maximum = governed_memory::router_with_search_limits(
        runtime.clone(),
        governed_memory::SearchLimits::new(65535, 65535).unwrap(),
    );
    assert_eq!(
        search(
            &maximum,
            Some(&reader),
            br#"{"query":"BUDGETWHOLE","max_context_bytes":65535}"#
        )
        .await
        .0,
        StatusCode::OK
    );
    let over_query =
        serde_json::json!({"query":"x".repeat(4097),"max_context_bytes":8192}).to_string();
    assert_eq!(
        search(&app, Some(&reader), over_query.as_bytes()).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end tracer deliberately keeps all public-operation assertions together.
async fn current_read_enforces_the_database_and_http_security_contract() {
    let (migrator, runtime, purge_worker, alice_reader, bob_reader, alice_writer) = setup().await;
    let app = router(runtime.clone());

    let allowed = request(&app, &alice_reader, ALLOWED_ITEM).await;
    assert_eq!(allowed.0, StatusCode::OK);
    assert_eq!(allowed.1["status"], "ready");
    assert_eq!(allowed.1["item_id"], ALLOWED_ITEM);
    assert_eq!(allowed.1["revision_id"], "50000000000000000000000000000001");
    assert_eq!(allowed.1["content"], "ALLOWED_ALPHA_HANDBOOK");
    assert_eq!(allowed.1["validity_status"], "unknown");
    let expected_allowed = request_expected(
        &app,
        &alice_reader,
        ALLOWED_ITEM,
        "50000000000000000000000000000001",
    )
    .await;
    assert_eq!(expected_allowed.0, StatusCode::OK);
    assert_eq!(
        expected_allowed.1["revision_id"],
        "50000000000000000000000000000001"
    );

    wait_for_idle_pool(&runtime).await;

    let query_cases = [
        "?".to_owned(),
        "?expected_revision_id=".to_owned(),
        "?expected_revision_id=50000000000000000000000000000001&expected_revision_id=50000000000000000000000000000001".to_owned(),
        "?unknown=FORGED_QUERY_FIELD".to_owned(),
        "?expected_revision_id=%350000000000000000000000000000001".to_owned(),
        format!("?expected_revision_id={}", "X".repeat(65)),
    ];
    let mut malformed_envelopes = Vec::new();
    for query in query_cases {
        malformed_envelopes.push(
            request_raw(
                &app,
                Some(&alice_reader),
                &format!("/v1/items/{ALLOWED_ITEM}{query}"),
                &[],
            )
            .await,
        );
    }
    let mut oversize = b"OVERSIZE_PAYLOAD_SENTINEL".to_vec();
    oversize.resize(16 * 1024 + 1, b'X');
    let body_cases = [
        b"{".to_vec(),
        br#"{"principal_id":"FORGED_DUPLICATE_A","principal_id":"FORGED_DUPLICATE_B"}"#
            .to_vec(),
        br#"{"principal_id":"FORGED_PRINCIPAL","tenant_id":"FORGED_COMPANY","user_confirmed":true,"confirmation":"FORGED_CONFIRMATION","scope":"unknown","payload":"FORBIDDEN_BODY_SENTINEL"}"#
            .to_vec(),
        b"RAW_PAYLOAD_SENTINEL".to_vec(),
        oversize,
    ];
    for body in &body_cases {
        malformed_envelopes.push(
            request_raw(
                &app,
                Some(&alice_reader),
                &format!("/v1/items/{ALLOWED_ITEM}"),
                body,
            )
            .await,
        );
    }
    let mut envelope_request_ids = Vec::new();
    let mut first_malformed_request_id = None;
    let mut normalized_envelopes = Vec::new();
    for (status, mut body) in malformed_envelopes {
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["status"], "malformed");
        assert_eq!(body["code"], "malformed");
        let serialized = body.to_string();
        for forbidden in [
            ALLOWED_ITEM,
            "FORGED_PRINCIPAL",
            "FORGED_COMPANY",
            "FORGED_CONFIRMATION",
            "unknown",
            "FORBIDDEN_BODY_SENTINEL",
            "RAW_PAYLOAD_SENTINEL",
            "OVERSIZE_PAYLOAD_SENTINEL",
        ] {
            assert!(!serialized.contains(forbidden));
        }
        envelope_request_ids.push(request_id(&body).to_owned());
        first_malformed_request_id.get_or_insert_with(|| request_id(&body).to_owned());
        body.as_object_mut()
            .expect("malformed envelope object")
            .remove("request_id");
        normalized_envelopes.push(body);
    }
    envelope_request_ids.sort();
    envelope_request_ids.dedup();
    assert_eq!(envelope_request_ids.len(), 11);
    assert!(
        normalized_envelopes
            .windows(2)
            .all(|pair| pair[0] == pair[1])
    );
    assert_read_rejection_audit(
        &migrator,
        first_malformed_request_id
            .as_deref()
            .expect("malformed request ID"),
        "malformed",
    )
    .await;

    for adversarial_body in [
        AdversarialBody::Pending,
        AdversarialBody::ByteThenPending { sent: false },
        AdversarialBody::BodyError,
    ] {
        let bounded = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            request_body(
                &app,
                Some(&alice_reader),
                &format!("/v1/items/{ALLOWED_ITEM}"),
                Body::new(adversarial_body),
            ),
        )
        .await
        .expect("body rejection must not hang the response");
        assert_eq!(bounded.0, StatusCode::BAD_REQUEST);
        assert_eq!(bounded.1["code"], "malformed");
        assert!(!bounded.1.to_string().contains("FORBIDDEN"));
    }

    let owner = request(&app, &bob_reader, BOB_PRIVATE_ITEM).await;
    assert_eq!(owner.0, StatusCode::OK);
    assert_eq!(owner.1["content"], "FORBIDDEN_BOB_PRIVATE");

    let mut denials = Vec::new();
    for item in [BOB_PRIVATE_ITEM, FOREIGN_ITEM, MISSING_ITEM] {
        let denied = request_expected(
            &app,
            &alice_reader,
            item,
            "50000000000000000000000000000001",
        )
        .await;
        assert_sanitized_unavailable(&denied);
        assert_eq!(denied.1["status"], "unavailable");
        assert_eq!(denied.1["code"], "unavailable");
        assert!(!denied.1.to_string().contains("FORBIDDEN"));
        denials.push(denied.1);
    }
    let denial_ids: Vec<_> = denials.iter().map(request_id).collect();
    assert_ne!(denial_ids[0], denial_ids[1]);
    assert_ne!(denial_ids[1], denial_ids[2]);
    for denial in &mut denials {
        denial
            .as_object_mut()
            .expect("error object")
            .remove("request_id");
    }
    assert_eq!(denials[0], denials[1]);
    assert_eq!(denials[1], denials[2]);

    let malformed = request_with_auth(&app, Some(&alice_reader), "bad%20id").await;
    assert_eq!(malformed.0, StatusCode::BAD_REQUEST);
    assert_eq!(malformed.1["code"], "malformed");
    let malformed_utf8 = request_with_auth(&app, Some(&alice_reader), "%FF").await;
    assert_eq!(malformed_utf8.0, StatusCode::BAD_REQUEST);
    assert_eq!(malformed_utf8.1["code"], "malformed");

    let mut held_audit = migrator.begin().await.expect("begin audit lock");
    sqlx::query("LOCK TABLE read_audit IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *held_audit)
        .await
        .expect("hold audit table lock");
    let audit_response_bound = std::time::Duration::from_millis(500);
    let audit_lock_start = tokio::time::Instant::now();
    let bounded_audit = tokio::time::timeout(
        audit_response_bound,
        request_with_auth(&app, Some(&alice_reader), "locked%20audit"),
    )
    .await
    .expect("best-effort rejection audit must not hang the response");
    assert_eq!(bounded_audit.0, StatusCode::BAD_REQUEST);
    assert!(
        audit_lock_start.elapsed() < audit_response_bound,
        "best-effort rejection audit must not hold the response"
    );
    held_audit
        .rollback()
        .await
        .expect("release audit table lock");

    let missing_with_forged_query = request_raw(
        &app,
        None,
        &format!("/v1/items/{ALLOWED_ITEM}?tenant_id=FORGED_COMPANY"),
        &[],
    )
    .await;
    assert_eq!(missing_with_forged_query.0, StatusCode::UNAUTHORIZED);
    let malformed_with_forged_body = request_raw(
        &app,
        Some("short"),
        &format!("/v1/items/{ALLOWED_ITEM}"),
        br#"{"confirmation":"FORGED_CONFIRMATION"}"#,
    )
    .await;
    assert_eq!(malformed_with_forged_body.0, StatusCode::UNAUTHORIZED);
    let missing_auth = request_with_auth(&app, None, ALLOWED_ITEM).await;
    assert_eq!(missing_auth.0, StatusCode::UNAUTHORIZED);
    let invalid_with_forged_query = request_raw(
        &app,
        Some(&"0".repeat(64)),
        &format!("/v1/items/{ALLOWED_ITEM}?scope=unknown"),
        &[],
    )
    .await;
    assert_eq!(invalid_with_forged_query.0, StatusCode::UNAUTHORIZED);
    let invalid_auth = request(&app, &"0".repeat(64), ALLOWED_ITEM).await;
    assert_eq!(invalid_auth.0, StatusCode::UNAUTHORIZED);

    sqlx::query(
        "UPDATE credentials SET issued_at = clock_timestamp() - interval '2 hours',
                                expires_at = clock_timestamp() - interval '1 hour'
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("expire Alice credential");
    let expired_with_forged_query = request_raw(
        &app,
        Some(&alice_reader),
        &format!("/v1/items/{ALLOWED_ITEM}?principal_id=FORGED_PRINCIPAL"),
        &[],
    )
    .await;
    assert_eq!(
        expired_with_forged_query.0,
        StatusCode::UNAUTHORIZED,
        "credential validity must take precedence over attacker-controlled query scope"
    );
    assert_eq!(
        request(&app, &alice_reader, ALLOWED_ITEM).await.0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(&migrator).await;

    sqlx::query(
        "UPDATE credentials SET revoked_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&migrator)
    .await
    .expect("revoke Alice credential");
    let revoked_with_forged_body = request_raw(
        &app,
        Some(&alice_reader),
        &format!("/v1/items/{ALLOWED_ITEM}"),
        br#"{"principal_id":"FORGED_PRINCIPAL"}"#,
    )
    .await;
    assert_eq!(revoked_with_forged_body.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        request(&app, &alice_reader, ALLOWED_ITEM).await.0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(&migrator).await;

    let mut revocation_lock = migrator.begin().await.expect("begin revocation race");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id = $1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *revocation_lock)
        .await
        .expect("hold authority during revocation");
    let waiting_app = app.clone();
    let waiting_bearer = alice_reader.clone();
    let waiting_read =
        tokio::spawn(async move { request(&waiting_app, &waiting_bearer, ALLOWED_ITEM).await });
    wait_for_authority_lock_wait(&migrator).await;
    sqlx::query(
        "UPDATE credentials SET revoked_at = clock_timestamp() WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&mut *revocation_lock)
    .await
    .expect("revoke while authority read waits");
    revocation_lock
        .commit()
        .await
        .expect("release revoked authority");
    let revoked_after_wait = waiting_read.await.expect("waiting HTTP task");
    assert_eq!(revoked_after_wait.0, StatusCode::UNAUTHORIZED);
    restore_alice_credential(&migrator).await;

    assert_runtime_role(&runtime).await;
    assert_purge_worker_role(&purge_worker).await;
    assert_lock_rejects_unbound_tenant(&runtime, &alice_reader).await;

    let mut held = migrator.begin().await.expect("begin authority lock");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id = $1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *held)
        .await
        .expect("hold exclusive authority lock");
    let timed_out = request(&app, &alice_reader, ALLOWED_ITEM).await;
    assert_eq!(timed_out.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(timed_out.1["status"], "storage_unavailable");
    assert_eq!(timed_out.1["code"], "storage_unavailable");
    assert!(!timed_out.1.to_string().contains("tenant_authority"));
    held.rollback().await.expect("release authority lock");
    assert_eq!(
        request(&app, &alice_reader, ALLOWED_ITEM).await.0,
        StatusCode::OK
    );

    let closed_pool = PgPoolOptions::new()
        .connect_lazy(&test_database_url(RUNTIME_URL))
        .expect("lazy closed test pool");
    closed_pool.close().await;
    let closed_app = router(closed_pool);
    let closed_query = request_raw(
        &closed_app,
        Some(&alice_reader),
        &format!("/v1/items/{ALLOWED_ITEM}?principal_id=FORGED_PRINCIPAL"),
        &[],
    )
    .await;
    let closed_body = request_raw(
        &closed_app,
        Some(&alice_reader),
        &format!("/v1/items/{ALLOWED_ITEM}"),
        b"FORBIDDEN_CLOSED_POOL_PAYLOAD",
    )
    .await;
    assert_eq!(closed_query.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(closed_body.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_ne!(request_id(&closed_query.1), request_id(&closed_body.1));
    let closed_one = request(&closed_app, &alice_reader, ALLOWED_ITEM).await;
    let closed_two = request(&closed_app, &alice_reader, ALLOWED_ITEM).await;
    assert_eq!(closed_one.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(closed_two.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_ne!(request_id(&closed_one.1), request_id(&closed_two.1));

    let audit = sqlx::query(
        "SELECT outcome, count(*) AS count FROM read_audit GROUP BY outcome ORDER BY outcome",
    )
    .fetch_all(&migrator)
    .await
    .expect("read sanitized audit");
    assert_eq!(audit.len(), 5);
    assert_eq!(
        audit[0].try_get::<String, _>("outcome").expect("outcome"),
        "malformed"
    );
    assert_eq!(audit[0].try_get::<i64, _>("count").expect("count"), 16);
    assert_eq!(
        audit[1].try_get::<String, _>("outcome").expect("outcome"),
        "released"
    );
    assert_eq!(audit[1].try_get::<i64, _>("count").expect("count"), 4);
    assert_eq!(
        audit[2].try_get::<String, _>("outcome").expect("outcome"),
        "storage_unavailable"
    );
    assert_eq!(audit[2].try_get::<i64, _>("count").expect("count"), 1);
    assert_eq!(
        audit[3].try_get::<String, _>("outcome").expect("outcome"),
        "unauthenticated"
    );
    assert_eq!(audit[3].try_get::<i64, _>("count").expect("count"), 10);
    assert_eq!(
        audit[4].try_get::<String, _>("outcome").expect("outcome"),
        "unavailable"
    );
    assert_eq!(audit[4].try_get::<i64, _>("count").expect("count"), 3);
    let unique_audits =
        sqlx::query_scalar::<_, i64>("SELECT count(DISTINCT request_id) FROM read_audit")
            .fetch_one(&migrator)
            .await
            .expect("unique audit request IDs");
    assert_eq!(unique_audits, 34);
    let normalized_rejections = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM read_audit WHERE outcome <> 'released' AND target_id IS NULL",
    )
    .fetch_one(&migrator)
    .await
    .expect("normalized rejection audit");
    assert_eq!(normalized_rejections, 30);
    let sanitized_malformed = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM read_audit
         WHERE outcome = 'malformed' AND target_id IS NULL
           AND tenant_id IS NOT NULL AND principal_id IS NOT NULL
           AND app_id IS NOT NULL AND credential_id IS NOT NULL",
    )
    .fetch_one(&migrator)
    .await
    .expect("malformed request audit is sanitized");
    assert_eq!(sanitized_malformed, 16);
    let unattributed_rejections = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM read_audit
         WHERE outcome = 'unauthenticated'
           AND tenant_id IS NULL AND principal_id IS NULL
           AND app_id IS NULL AND credential_id IS NULL",
    )
    .fetch_one(&migrator)
    .await
    .expect("unverified rejection identities remain absent");
    assert_eq!(unattributed_rejections, 5);

    assert_trusted_private_create(
        &app,
        &migrator,
        &runtime,
        &purge_worker,
        &alice_reader,
        &bob_reader,
        &alice_writer,
    )
    .await;
    assert_step4_independent_grant_revocation_and_withdrawal(
        &migrator,
        &runtime,
        &alice_reader,
        &bob_reader,
    )
    .await;
    assert_step4_session_failure_and_tenant_reuse(&migrator, &alice_reader, &alice_writer).await;
    assert_step4_cooperating_host_discards_stale_evidence(
        &app,
        &migrator,
        &alice_reader,
        &alice_writer,
    )
    .await;
    assert_document_derived_storage_foundation(&migrator, &runtime, &alice_reader).await;
    assert_direct_document_passage_search(&app, &migrator, &runtime, &alice_reader, &bob_reader)
        .await;
}

#[allow(clippy::too_many_lines)]
async fn assert_direct_document_passage_search(
    app: &axum::Router,
    migrator: &PgPool,
    runtime: &PgPool,
    alice_reader: &str,
    bob_reader: &str,
) {
    const COLLECTION_A: &str = "30000000000000000000000000000001";
    const COLLECTION_B: &str = "30000000000000000000000000000004";
    const ITEM_A: &str = "document_search_item_a";
    const ITEM_B: &str = "document_search_item_b";
    const MEMORY_ITEM: &str = "document_search_memory";
    const UTF8_MEMORY_ITEM: &str = "document_search_utf8_memory";
    const EXPIRED_ITEM: &str = "document_search_expired";
    const REVISION_A: &str = "document_search_revision_a";
    const REVISION_A_2: &str = "document_search_revision_a2";
    const REVISION_B: &str = "document_search_revision_b";
    const MEMORY_REVISION: &str = "document_search_memory_revision";
    const UTF8_MEMORY_REVISION: &str = "document_search_utf8_memory_revision";
    const EXPIRED_REVISION: &str = "document_search_expired_revision";
    const SOURCE: &str = "document_search_source";
    const SET_A: &str = "document_search_set_a";
    const SET_A_2: &str = "document_search_set_a2";
    const SET_B: &str = "document_search_set_b";
    const ALTERNATE_ITEM: &str = "document_search_alternate_parent";
    const ALTERNATE_REVISION: &str = "document_search_alternate_revision";
    const ALTERNATE_SOURCE: &str = "document_search_alternate_source";
    const ALTERNATE_SET: &str = "document_search_alternate_set";
    const ALTERNATE_MEMORY: &str = "document_search_alternate_memory";
    const ALTERNATE_MEMORY_REVISION: &str = "document_search_alternate_memory_revision";
    const IDENTITY_ITEM: &str = "document_search_identity_item";
    const IDENTITY_REVISION: &str = "document_search_identity_revision";
    const IDENTITY_INACTIVE_REVISION: &str = "document_search_identity_inactive_revision";
    const IDENTITY_SOURCE: &str = "document_search_identity_source";
    const IDENTITY_SET: &str = "document_search_identity_set";
    const IDENTITY_OLD_SET: &str = "document_search_identity_old_set";
    const IDENTITY_INACTIVE_SET: &str = "document_search_identity_inactive_set";
    const IDENTITY_OTHER_ITEM: &str = "document_search_identity_other_item";
    const IDENTITY_OTHER_REVISION: &str = "document_search_identity_other_revision";
    let source_sha256 = Sha256::digest(b"same document-search bytes");

    sqlx::query(
        "INSERT INTO items (tenant_id,id,collection_id) VALUES
           ($1,$2,$6),($1,$3,$7),($1,$4,$6),($1,$5,$6)",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(ITEM_B)
    .bind(MEMORY_ITEM)
    .bind(EXPIRED_ITEM)
    .bind(COLLECTION_A)
    .bind(COLLECTION_B)
    .execute(migrator)
    .await
    .expect("seed direct document-search items");
    sqlx::query(
        "INSERT INTO revisions (tenant_id,item_id,id,content,valid_until) VALUES
           ($1,$2,$6,'DOCUMENT A DESCRIPTOR',NULL),
           ($1,$2,$7,'DOCUMENT A INACTIVE',NULL),
           ($1,$3,$8,'DOCUMENT B DESCRIPTOR',NULL),
           ($1,$4,$9,'MIXEDKEY ordinary memory',NULL),
           ($1,$5,$10,'EXPIRED DOCUMENT DESCRIPTOR','2020-01-01T00:00:00Z')",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(ITEM_B)
    .bind(MEMORY_ITEM)
    .bind(EXPIRED_ITEM)
    .bind(REVISION_A)
    .bind(REVISION_A_2)
    .bind(REVISION_B)
    .bind(MEMORY_REVISION)
    .bind(EXPIRED_REVISION)
    .execute(migrator)
    .await
    .expect("seed direct document-search revisions");
    sqlx::query(
        "UPDATE items SET active_revision_id=CASE id
           WHEN $2 THEN $6 WHEN $3 THEN $7 WHEN $4 THEN $8 ELSE $9 END
         WHERE tenant_id=$1 AND id IN ($2,$3,$4,$5)",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(ITEM_B)
    .bind(MEMORY_ITEM)
    .bind(EXPIRED_ITEM)
    .bind(REVISION_A)
    .bind(REVISION_B)
    .bind(MEMORY_REVISION)
    .bind(EXPIRED_REVISION)
    .execute(migrator)
    .await
    .expect("activate direct document-search revisions");
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
         VALUES ($1,$2,$3,to_tsvector('simple','MIXEDKEY ordinary memory'))",
    )
    .bind(ALPHA_TENANT)
    .bind(MEMORY_ITEM)
    .bind(MEMORY_REVISION)
    .execute(migrator)
    .await
    .expect("index ordinary mixed-search memory");
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,$2,$3)")
        .bind(ALPHA_TENANT)
        .bind(UTF8_MEMORY_ITEM)
        .bind(COLLECTION_A)
        .execute(migrator)
        .await
        .expect("seed multibyte search memory");
    sqlx::query(
        "INSERT INTO revisions (tenant_id,item_id,id,content)
         VALUES ($1,$2,$3,'é K K')",
    )
    .bind(ALPHA_TENANT)
    .bind(UTF8_MEMORY_ITEM)
    .bind(UTF8_MEMORY_REVISION)
    .execute(migrator)
    .await
    .expect("seed multibyte search memory revision");
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(UTF8_MEMORY_ITEM)
        .bind(UTF8_MEMORY_REVISION)
        .execute(migrator)
        .await
        .expect("activate multibyte search memory");
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
         VALUES ($1,$2,$3,to_tsvector('simple','K K'))",
    )
    .bind(ALPHA_TENANT)
    .bind(UTF8_MEMORY_ITEM)
    .bind(UTF8_MEMORY_REVISION)
    .execute(migrator)
    .await
    .expect("index multibyte search memory");

    let a_contents = vec![
        "CAPKEY MIXEDKEY FIRSTBOUNDARY café one".to_owned(),
        "CAPKEY LIFECYCLEKEY two".to_owned(),
        "CAPKEY three".to_owned(),
        "CAPKEY four".to_owned(),
        "CAPKEY five".to_owned(),
        "CAPKEY six".to_owned(),
        format!("{}oversized", "SKIPKEY ".repeat(20)),
        "OLDSETKEY obsolete".to_owned(),
        format!("{}oversized one", "BUDGETKEY ".repeat(20)),
        format!("{}oversized two", "BUDGETKEY ".repeat(19)),
        format!("{}oversized three", "BUDGETKEY ".repeat(18)),
        format!("{}oversized four", "BUDGETKEY ".repeat(17)),
        "BUDGETKEY fits".to_owned(),
        format!("{}oversized one", "NOFITKEY ".repeat(20)),
        format!("{}oversized two", "NOFITKEY ".repeat(19)),
        format!("{}oversized three", "NOFITKEY ".repeat(18)),
        format!("{}oversized four", "NOFITKEY ".repeat(17)),
        format!("{}oversized five", "NOFITKEY ".repeat(16)),
        "TEN TEN xx".to_owned(),
        "TEN TEN xy".to_owned(),
        "TEN TEN xz".to_owned(),
        "TEN TEN xw".to_owned(),
        "TEN".to_owned(),
        "JSONKEY exact numeric locator".to_owned(),
        "K".to_owned(),
        "CONT_PREVIOUS supporting evidence".to_owned(),
        "CONTKEY direct center".to_owned(),
        "CONT_NEXT supporting evidence".to_owned(),
        "BOUNDARY_FIRST direct only".to_owned(),
        "BOUNDARY_LAST direct only".to_owned(),
        "DUPKEY left direct".to_owned(),
        "shared continuation evidence".to_owned(),
        "DUPKEY right direct".to_owned(),
        "DIRECTDEDUP left".to_owned(),
        "DIRECTDEDUP right".to_owned(),
        "x".repeat(100),
        "LATERFITKEY".to_owned(),
        "fit".to_owned(),
        "EXPANDCAP A".to_owned(),
        "cap neighbor A".to_owned(),
        "EXPANDCAP B".to_owned(),
        "cap neighbor B".to_owned(),
        "EXPANDCAP C".to_owned(),
        "cap neighbor C LASTBOUNDARY".to_owned(),
    ];
    let mut a_parents = (0..a_contents.len())
        .map(|index| format!("search-section-{index:02}"))
        .collect::<Vec<_>>();
    let mut a_directions = vec!["none"; a_contents.len()];
    for (range, parent, directions) in [
        (25..=27, "center-span", ["to_next", "both", "from_previous"]),
        (
            30..=32,
            "duplicate-span",
            ["to_next", "both", "from_previous"],
        ),
        (
            35..=37,
            "later-fit-span",
            ["to_next", "both", "from_previous"],
        ),
    ] {
        for (offset, index) in range.enumerate() {
            parent.clone_into(&mut a_parents[index]);
            a_directions[index] = directions[offset];
        }
    }
    for (range, parent) in [
        (33..=34, "direct-dedup-span"),
        (38..=39, "cap-span-a"),
        (40..=41, "cap-span-b"),
        (42..=43, "cap-span-c"),
    ] {
        let start = *range.start();
        let end = *range.end();
        for index in range {
            parent.clone_into(&mut a_parents[index]);
        }
        a_directions[start] = "to_next";
        a_directions[end] = "from_previous";
    }
    activate_search_extraction_with_metadata(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE,
        &source_sha256,
        SET_A,
        &a_contents,
        &a_parents,
        &a_directions,
    )
    .await;
    let b_contents = vec![
        "CAPKEY CAPKEY MIXEDKEY second source".to_owned(),
        "SKIPKEY ok".to_owned(),
        "CORRUPTKEY surviving".to_owned(),
        "corrupt expansion only".to_owned(),
    ];
    activate_search_extraction_with_metadata(
        migrator,
        ITEM_B,
        REVISION_B,
        SOURCE,
        &source_sha256,
        SET_B,
        &b_contents,
        &[
            "search-section-b0".to_owned(),
            "search-section-b1".to_owned(),
            "corrupt-span".to_owned(),
            "corrupt-span".to_owned(),
        ],
        &["none", "none", "to_next", "from_previous"],
    )
    .await;
    seed_search_item_revisions(
        migrator,
        COLLECTION_A,
        IDENTITY_ITEM,
        &[IDENTITY_INACTIVE_REVISION, IDENTITY_REVISION],
    )
    .await;
    activate_search_extraction_with_metadata(
        migrator,
        IDENTITY_ITEM,
        IDENTITY_INACTIVE_REVISION,
        "document_search_identity_inactive_source",
        &Sha256::digest(b"identity inactive bytes"),
        IDENTITY_INACTIVE_SET,
        &[
            "ISOKEY inactive parent".to_owned(),
            "INACTIVE NEIGHBOR".to_owned(),
        ],
        &["collision-parent".to_owned(), "collision-parent".to_owned()],
        &["to_next", "from_previous"],
    )
    .await;
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(IDENTITY_ITEM)
        .bind(IDENTITY_REVISION)
        .execute(migrator)
        .await
        .expect("switch identity fixture revision");
    for (set_id, parent, neighbor) in [
        (IDENTITY_OLD_SET, "ISOKEY old parent", "OLD NEIGHBOR"),
        (IDENTITY_SET, "ISOKEY current parent", "CURRENT NEIGHBOR"),
    ] {
        activate_search_extraction_with_metadata(
            migrator,
            IDENTITY_ITEM,
            IDENTITY_REVISION,
            IDENTITY_SOURCE,
            &Sha256::digest(b"identity current bytes"),
            set_id,
            &[parent.to_owned(), neighbor.to_owned()],
            &["collision-parent".to_owned(), "collision-parent".to_owned()],
            &["to_next", "from_previous"],
        )
        .await;
    }
    seed_search_item_revisions(
        migrator,
        COLLECTION_A,
        IDENTITY_OTHER_ITEM,
        &[IDENTITY_OTHER_REVISION],
    )
    .await;
    activate_search_extraction_with_metadata(
        migrator,
        IDENTITY_OTHER_ITEM,
        IDENTITY_OTHER_REVISION,
        IDENTITY_SOURCE,
        &Sha256::digest(b"identity other bytes"),
        IDENTITY_SET,
        &[
            "OTHERKEY other parent".to_owned(),
            "OTHER NEIGHBOR".to_owned(),
        ],
        &["collision-parent".to_owned(), "collision-parent".to_owned()],
        &["to_next", "from_previous"],
    )
    .await;
    sqlx::query(
        "INSERT INTO items (tenant_id,id,collection_id) VALUES
           ($1,$2,$3),($1,$4,$3)",
    )
    .bind(ALPHA_TENANT)
    .bind(ALTERNATE_ITEM)
    .bind(COLLECTION_A)
    .bind(ALTERNATE_MEMORY)
    .execute(migrator)
    .await
    .expect("seed alternate-parent items");
    sqlx::query(
        "INSERT INTO revisions (tenant_id,item_id,id,content) VALUES
           ($1,$2,$3,'DOCUMENT ALTERNATE'),($1,$4,$5,'Q Q Q Q Q!')",
    )
    .bind(ALPHA_TENANT)
    .bind(ALTERNATE_ITEM)
    .bind(ALTERNATE_REVISION)
    .bind(ALTERNATE_MEMORY)
    .bind(ALTERNATE_MEMORY_REVISION)
    .execute(migrator)
    .await
    .expect("seed alternate-parent revisions");
    sqlx::query(
        "UPDATE items SET active_revision_id = CASE id WHEN $2 THEN $3 ELSE $5 END
         WHERE tenant_id=$1 AND id IN ($2,$4)",
    )
    .bind(ALPHA_TENANT)
    .bind(ALTERNATE_ITEM)
    .bind(ALTERNATE_REVISION)
    .bind(ALTERNATE_MEMORY)
    .bind(ALTERNATE_MEMORY_REVISION)
    .execute(migrator)
    .await
    .expect("activate alternate-parent revisions");
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
         VALUES ($1,$2,$3,to_tsvector('simple','Q Q Q Q Q'))",
    )
    .bind(ALPHA_TENANT)
    .bind(ALTERNATE_MEMORY)
    .bind(ALTERNATE_MEMORY_REVISION)
    .execute(migrator)
    .await
    .expect("index alternate-parent memory");
    activate_search_extraction_with_metadata(
        migrator,
        ALTERNATE_ITEM,
        ALTERNATE_REVISION,
        ALTERNATE_SOURCE,
        &Sha256::digest(b"alternate parent bytes"),
        ALTERNATE_SET,
        &["Q Q 123456".to_owned(), "N".to_owned(), "Q x".to_owned()],
        &[
            "alternate-span".to_owned(),
            "alternate-span".to_owned(),
            "alternate-span".to_owned(),
        ],
        &["to_next", "both", "from_previous"],
    )
    .await;
    activate_search_extraction(
        migrator,
        EXPIRED_ITEM,
        EXPIRED_REVISION,
        SOURCE,
        &Sha256::digest(b"expired document bytes"),
        "document_search_expired_set",
        &["EXPIRYKEY must stay hidden".to_owned()],
    )
    .await;

    let search_audits_before = search_audit_count(migrator).await;
    let read_audits_before = read_operation_audit_count(migrator).await;
    let mixed = search(
        app,
        Some(alice_reader),
        br#"{"query":"MIXEDKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(mixed.0, StatusCode::OK);
    assert_eq!(search_audit_count(migrator).await, search_audits_before + 1);
    assert_eq!(
        read_operation_audit_count(migrator).await,
        read_audits_before
    );
    let mixed_items = mixed.1["items"].as_array().expect("mixed search items");
    assert_eq!(mixed_items.len(), 3);
    assert_eq!(mixed_items[0]["item_id"], ITEM_A);
    assert_eq!(mixed_items[0]["revision_id"], REVISION_A);
    assert_eq!(
        mixed_items[0]["excerpt"],
        "CAPKEY MIXEDKEY FIRSTBOUNDARY café one"
    );
    assert_eq!(mixed_items[0]["citation"]["source_revision_id"], SOURCE);
    assert_eq!(mixed_items[0]["citation"]["extraction_set_id"], SET_A);
    assert_eq!(
        mixed_items[0]["citation"]["passage_id"],
        "search-passage-00"
    );
    assert_eq!(
        mixed_items[0]["citation"]["locator"],
        serde_json::json!({"block":0})
    );
    assert_eq!(mixed_items[1]["item_id"], ITEM_B);
    assert_eq!(mixed_items[1]["citation"]["source_revision_id"], SOURCE);
    assert_ne!(mixed_items[0]["item_id"], mixed_items[1]["item_id"]);
    assert_eq!(mixed_items[2]["item_id"], MEMORY_ITEM);
    assert!(mixed_items[2].get("citation").is_none());
    let exact_mixed_bytes = mixed_items
        .iter()
        .map(|item| item["excerpt"].as_str().expect("search excerpt").len())
        .sum::<usize>();
    assert_eq!(mixed.1["context_bytes"], exact_mixed_bytes);
    for forbidden in [
        "tenant_id",
        "collection_id",
        "structural_parent_id",
        "passage_order",
        "continuation_direction",
        "fits_global_budget",
        "source_cap_omitted",
        "candidate_omitted",
        "parent_passage_ids",
        "authority_epoch",
        "audience_kind",
    ] {
        assert!(!mixed.1.to_string().contains(forbidden));
    }

    let continued = search(
        app,
        Some(alice_reader),
        br#"{"query":"CONTKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(continued.0, StatusCode::OK);
    assert_eq!(
        continued.1["items"]
            .as_array()
            .expect("reciprocal continuation items")
            .iter()
            .map(|item| (
                item["excerpt"].as_str().expect("continuation excerpt"),
                item["reason"].as_str().expect("continuation reason"),
            ))
            .collect::<Vec<_>>(),
        [
            ("CONTKEY direct center", "lexical"),
            ("CONT_PREVIOUS supporting evidence", "adjacent_continuation"),
            ("CONT_NEXT supporting evidence", "adjacent_continuation"),
        ]
    );
    assert_eq!(continued.1["truncated"], false);

    let identity_isolated = search(
        app,
        Some(alice_reader),
        br#"{"query":"ISOKEY","max_context_bytes":4096}"#,
    )
    .await;
    let identity_items = identity_isolated.1["items"]
        .as_array()
        .expect("identity-isolated continuation items");
    assert_eq!(identity_items.len(), 2);
    assert_eq!(identity_items[0]["excerpt"], "ISOKEY current parent");
    assert_eq!(identity_items[1]["excerpt"], "CURRENT NEIGHBOR");
    assert_eq!(identity_items[1]["reason"], "adjacent_continuation");
    assert!(identity_items.iter().all(|item| {
        item["item_id"] == IDENTITY_ITEM
            && item["revision_id"] == IDENTITY_REVISION
            && item["citation"]["source_revision_id"] == IDENTITY_SOURCE
            && item["citation"]["extraction_set_id"] == IDENTITY_SET
    }));
    for forbidden in ["OLD NEIGHBOR", "INACTIVE NEIGHBOR", "OTHER NEIGHBOR"] {
        assert!(!identity_isolated.1.to_string().contains(forbidden));
    }
    let other_identity = search(
        app,
        Some(alice_reader),
        br#"{"query":"OTHERKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(other_identity.1["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(other_identity.1["items"][1]["excerpt"], "OTHER NEIGHBOR");
    assert_eq!(other_identity.1["items"][1]["item_id"], IDENTITY_OTHER_ITEM);

    let alternate_parent = search(
        app,
        Some(alice_reader),
        br#"{"query":"Q","max_context_bytes":17}"#,
    )
    .await;
    assert_eq!(
        alternate_parent.1["items"]
            .as_array()
            .expect("alternate-parent items")
            .iter()
            .map(|item| (
                item["excerpt"].as_str().expect("search excerpt"),
                item["reason"].as_str().expect("search reason"),
            ))
            .collect::<Vec<_>>(),
        [
            ("Q Q Q Q Q!", "lexical"),
            ("Q x", "lexical"),
            ("N", "adjacent_continuation")
        ]
    );
    assert_eq!(alternate_parent.1["context_bytes"], 14);
    assert_eq!(alternate_parent.1["truncated"], true);
    let both_parents = search(
        app,
        Some(alice_reader),
        br#"{"query":"Q","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(
        both_parents.1["items"]
            .as_array()
            .expect("both-parent items")
            .iter()
            .filter(|item| item["excerpt"] == "N")
            .count(),
        1
    );
    assert_eq!(
        both_parents.1["items"][3]["reason"],
        "adjacent_continuation"
    );

    let first_boundary = search(
        app,
        Some(alice_reader),
        br#"{"query":"FIRSTBOUNDARY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(first_boundary.1["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(first_boundary.1["items"][0]["reason"], "lexical");

    let last_boundary = search(
        app,
        Some(alice_reader),
        br#"{"query":"LASTBOUNDARY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(
        last_boundary.1["items"]
            .as_array()
            .expect("final passage expansion")
            .iter()
            .map(|item| item["reason"].as_str().expect("search reason"))
            .collect::<Vec<_>>(),
        ["lexical", "adjacent_continuation"]
    );
    assert!(last_boundary.1.to_string().contains("EXPANDCAP C"));

    set_search_passage_metadata(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE,
        SET_A,
        "search-passage-27",
        None,
        Some("none"),
    )
    .await;
    let one_sided = search(
        app,
        Some(alice_reader),
        br#"{"query":"CONTKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(one_sided.1["items"].as_array().map(Vec::len), Some(2));
    assert!(one_sided.1.to_string().contains("CONT_PREVIOUS"));
    assert!(!one_sided.1.to_string().contains("CONT_NEXT"));
    set_search_passage_metadata(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE,
        SET_A,
        "search-passage-27",
        None,
        Some("from_previous"),
    )
    .await;

    set_search_passage_metadata(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE,
        SET_A,
        "search-passage-27",
        Some("other-section"),
        None,
    )
    .await;
    let cross_parent = search(
        app,
        Some(alice_reader),
        br#"{"query":"CONTKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(cross_parent.1["items"].as_array().map(Vec::len), Some(2));
    assert!(!cross_parent.1.to_string().contains("CONT_NEXT"));
    set_search_passage_metadata(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE,
        SET_A,
        "search-passage-27",
        Some("center-span"),
        None,
    )
    .await;

    let duplicate_neighbor = search(
        app,
        Some(alice_reader),
        br#"{"query":"DUPKEY","max_context_bytes":4096}"#,
    )
    .await;
    let duplicate_items = duplicate_neighbor.1["items"]
        .as_array()
        .expect("deduplicated continuation items");
    assert_eq!(duplicate_items.len(), 3);
    assert_eq!(duplicate_items[0]["reason"], "lexical");
    assert_eq!(duplicate_items[1]["reason"], "lexical");
    assert_eq!(duplicate_items[2]["reason"], "adjacent_continuation");
    assert_eq!(
        duplicate_items[2]["excerpt"],
        "shared continuation evidence"
    );

    let direct_neighbor = search(
        app,
        Some(alice_reader),
        br#"{"query":"DIRECTDEDUP","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(direct_neighbor.1["items"].as_array().map(Vec::len), Some(2));
    assert!(
        direct_neighbor.1["items"]
            .as_array()
            .expect("direct neighbor items")
            .iter()
            .all(|item| item["reason"] == "lexical")
    );

    let later_fit = search(
        app,
        Some(alice_reader),
        br#"{"query":"LATERFITKEY","max_context_bytes":14}"#,
    )
    .await;
    assert_eq!(
        later_fit.1["items"]
            .as_array()
            .expect("later fitting continuation")
            .iter()
            .map(|item| item["excerpt"].as_str().expect("continuation excerpt"))
            .collect::<Vec<_>>(),
        ["LATERFITKEY", "fit"]
    );
    assert_eq!(later_fit.1["context_bytes"], 14);
    assert_eq!(later_fit.1["truncated"], true);

    let expansion_capped = search(
        app,
        Some(alice_reader),
        br#"{"query":"EXPANDCAP","max_context_bytes":4096}"#,
    )
    .await;
    let expansion_capped_items = expansion_capped.1["items"]
        .as_array()
        .expect("source-capped continuation items");
    assert_eq!(expansion_capped_items.len(), 4);
    assert_eq!(
        expansion_capped_items
            .iter()
            .filter(|item| item["reason"] == "lexical")
            .count(),
        3
    );
    assert_eq!(expansion_capped.1["truncated"], true);

    let capped = search(
        app,
        Some(alice_reader),
        br#"{"query":"CAPKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(capped.0, StatusCode::OK);
    let capped_items = capped.1["items"].as_array().expect("capped search items");
    assert_eq!(capped_items.len(), 5);
    assert_eq!(capped.1["truncated"], true);
    assert_eq!(capped_items[0]["item_id"], ITEM_B);
    assert_eq!(
        capped_items
            .iter()
            .filter(|item| item["item_id"] == ITEM_A)
            .count(),
        4
    );
    assert_eq!(
        capped_items
            .iter()
            .map(|item| {
                (
                    item["item_id"].as_str().expect("capped item id"),
                    item["citation"]["locator"]["block"]
                        .as_u64()
                        .expect("capped block locator"),
                )
            })
            .collect::<std::collections::HashSet<_>>()
            .len(),
        5
    );
    assert_eq!(
        capped_items[1]["citation"]["passage_id"],
        "search-passage-00"
    );
    assert_eq!(
        capped_items[4]["citation"]["passage_id"],
        "search-passage-03"
    );

    let skipped = search(
        app,
        Some(alice_reader),
        br#"{"query":"SKIPKEY","max_context_bytes":10}"#,
    )
    .await;
    assert_eq!(skipped.0, StatusCode::OK);
    assert_eq!(skipped.1["items"].as_array().expect("skip items").len(), 1);
    assert_eq!(skipped.1["items"][0]["item_id"], ITEM_B);
    assert_eq!(skipped.1["items"][0]["excerpt"], "SKIPKEY ok");
    assert_eq!(skipped.1["context_bytes"], 10);
    assert_eq!(skipped.1["truncated"], true);

    let fitting_after_four_oversized = search(
        app,
        Some(alice_reader),
        br#"{"query":"BUDGETKEY","max_context_bytes":14}"#,
    )
    .await;
    assert_eq!(fitting_after_four_oversized.0, StatusCode::OK);
    assert_eq!(
        fitting_after_four_oversized.1["items"][0]["excerpt"],
        "BUDGETKEY fits"
    );
    assert_eq!(fitting_after_four_oversized.1["context_bytes"], 14);
    assert_eq!(fitting_after_four_oversized.1["truncated"], true);

    let no_fit = search(
        app,
        Some(alice_reader),
        br#"{"query":"NOFITKEY","max_context_bytes":10}"#,
    )
    .await;
    assert_eq!(no_fit.0, StatusCode::OK);
    assert_eq!(no_fit.1["items"], serde_json::json!([]));
    assert_eq!(no_fit.1["context_bytes"], 0);
    assert_eq!(no_fit.1["truncated"], true);

    let accepted_contributions = search(
        app,
        Some(alice_reader),
        br#"{"query":"TEN","max_context_bytes":23}"#,
    )
    .await;
    assert_eq!(accepted_contributions.0, StatusCode::OK);
    assert_eq!(
        accepted_contributions.1["items"]
            .as_array()
            .expect("accepted source contributions")
            .iter()
            .map(|item| item["excerpt"].as_str().expect("accepted excerpt"))
            .collect::<Vec<_>>(),
        ["TEN TEN xx", "TEN TEN xy", "TEN"]
    );
    assert_eq!(accepted_contributions.1["context_bytes"], 23);
    assert_eq!(accepted_contributions.1["truncated"], true);
    assert!(
        accepted_contributions.1["items"]
            .as_array()
            .expect("bounded source contributions")
            .len()
            <= 4
    );

    let memory_scalar_skip = search(
        app,
        Some(alice_reader),
        br#"{"query":"K","max_context_bytes":1}"#,
    )
    .await;
    assert_eq!(memory_scalar_skip.0, StatusCode::OK);
    assert_eq!(
        memory_scalar_skip.1["items"]
            .as_array()
            .expect("mixed K hits")
            .len(),
        1
    );
    assert_eq!(memory_scalar_skip.1["items"][0]["item_id"], ITEM_A);
    assert_eq!(memory_scalar_skip.1["items"][0]["excerpt"], "K");
    assert!(memory_scalar_skip.1["items"][0].get("citation").is_some());
    assert_eq!(memory_scalar_skip.1["context_bytes"], 1);
    assert_eq!(memory_scalar_skip.1["truncated"], true);

    let exact_numbers = search(
        app,
        Some(alice_reader),
        br#"{"query":"JSONKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(exact_numbers.0, StatusCode::OK);
    let exact_locator = &exact_numbers.1["items"][0]["citation"]["locator"];
    assert_eq!(
        exact_locator.to_string(),
        r#"{"block":23,"decimal":0.12345678901234567890123456789,"integer":18446744073709551617}"#
    );
    let round_trip: Value =
        serde_json::from_str(&exact_locator.to_string()).expect("round-trip exact locator JSON");
    assert_eq!(round_trip, *exact_locator);

    activate_search_extraction_with_metadata(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE,
        &source_sha256,
        SET_A_2,
        &[
            "OLDSETKEY current replacement".to_owned(),
            "replacement continuation".to_owned(),
        ],
        &["replacement-span".to_owned(), "replacement-span".to_owned()],
        &["to_next", "from_previous"],
    )
    .await;
    let reprocessed = search(
        app,
        Some(alice_reader),
        br#"{"query":"OLDSETKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(
        reprocessed.1["items"]
            .as_array()
            .expect("reprocessed items")
            .len(),
        2
    );
    assert_eq!(
        reprocessed.1["items"][0]["citation"]["extraction_set_id"],
        SET_A_2
    );
    assert_eq!(
        reprocessed.1["items"][0]["excerpt"],
        "OLDSETKEY current replacement"
    );
    assert_eq!(
        reprocessed.1["items"][1]["excerpt"],
        "replacement continuation"
    );
    assert_eq!(reprocessed.1["items"][1]["reason"], "adjacent_continuation");
    let old_continuation = search(
        app,
        Some(alice_reader),
        br#"{"query":"CONTKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(old_continuation.1["items"], serde_json::json!([]));

    let corrupt_before = search(
        app,
        Some(alice_reader),
        br#"{"query":"CORRUPTKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(corrupt_before.1["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        corrupt_before.1["items"][1]["excerpt"],
        "corrupt expansion only"
    );
    assert_eq!(
        corrupt_before.1["items"][1]["reason"],
        "adjacent_continuation"
    );

    sqlx::query(
        "DELETE FROM source_passages WHERE tenant_id=$1 AND item_id=$2
         AND revision_id=$3 AND source_revision_id=$4 AND extraction_set_id=$5
         AND id='search-passage-03'",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_B)
    .bind(REVISION_B)
    .bind(SOURCE)
    .bind(SET_B)
    .execute(migrator)
    .await
    .expect("corrupt searchable extraction set");
    let corrupt = search(
        app,
        Some(alice_reader),
        br#"{"query":"CORRUPTKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(corrupt.1["items"], serde_json::json!([]));

    let bob = search(
        app,
        Some(bob_reader),
        br#"{"query":"OLDSETKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(bob.0, StatusCode::OK);
    assert_eq!(bob.1["items"], serde_json::json!([]));
    assert_wrong_tenant_document_search(runtime, alice_reader).await;
    assert_document_search_lifecycle_exclusion(app, migrator, alice_reader, ITEM_A, REVISION_A)
        .await;
    let invalid_scope = search(
        app,
        Some(alice_reader),
        br#"{"query":"OLDSETKEY","scope":{"subjects":["missing_document_scope"]},"max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(invalid_scope.0, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_scope.1["code"], "malformed");
    let expired = search(
        app,
        Some(alice_reader),
        br#"{"query":"EXPIRYKEY","max_context_bytes":4096}"#,
    )
    .await;
    assert_eq!(expired.1["items"], serde_json::json!([]));

    let fixture_items = vec![
        ITEM_A,
        ITEM_B,
        MEMORY_ITEM,
        UTF8_MEMORY_ITEM,
        EXPIRED_ITEM,
        ALTERNATE_ITEM,
        ALTERNATE_MEMORY,
        IDENTITY_ITEM,
        IDENTITY_OTHER_ITEM,
    ];
    sqlx::query("UPDATE items SET active_revision_id=NULL WHERE tenant_id=$1 AND id=ANY($2)")
        .bind(ALPHA_TENANT)
        .bind(&fixture_items)
        .execute(migrator)
        .await
        .expect("deactivate document-search fixtures");
    sqlx::query("DELETE FROM lexical_representations WHERE tenant_id=$1 AND item_id=ANY($2)")
        .bind(ALPHA_TENANT)
        .bind(&fixture_items)
        .execute(migrator)
        .await
        .expect("remove document-search lexical rows");
    sqlx::query("DELETE FROM revisions WHERE tenant_id=$1 AND item_id=ANY($2)")
        .bind(ALPHA_TENANT)
        .bind(&fixture_items)
        .execute(migrator)
        .await
        .expect("remove document-search revisions");
    sqlx::query("DELETE FROM items WHERE tenant_id=$1 AND id=ANY($2)")
        .bind(ALPHA_TENANT)
        .bind(&fixture_items)
        .execute(migrator)
        .await
        .expect("remove document-search fixtures");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM items WHERE tenant_id=$1 AND id=ANY($2)",
        )
        .bind(ALPHA_TENANT)
        .bind(&fixture_items)
        .fetch_one(migrator)
        .await
        .expect("verify document-search fixture cleanup"),
        0
    );
}

async fn activate_search_extraction(
    pool: &PgPool,
    item_id: &str,
    revision_id: &str,
    source_revision_id: &str,
    source_sha256: &[u8],
    extraction_set_id: &str,
    contents: &[String],
) {
    let structural_parent_ids = (0..contents.len())
        .map(|index| format!("search-section-{index:02}"))
        .collect::<Vec<_>>();
    let continuation_directions = vec!["none"; contents.len()];
    activate_search_extraction_with_metadata(
        pool,
        item_id,
        revision_id,
        source_revision_id,
        source_sha256,
        extraction_set_id,
        contents,
        &structural_parent_ids,
        &continuation_directions,
    )
    .await;
}

async fn seed_search_item_revisions(
    pool: &PgPool,
    collection_id: &str,
    item_id: &str,
    revision_ids: &[&str],
) {
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,$2,$3)")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(collection_id)
        .execute(pool)
        .await
        .expect("seed search identity item");
    for revision_id in revision_ids {
        sqlx::query(
            "INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,$2,$3,'DOCUMENT')",
        )
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(revision_id)
        .execute(pool)
        .await
        .expect("seed search identity revision");
    }
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(revision_ids[0])
        .execute(pool)
        .await
        .expect("activate search identity revision");
}

#[allow(clippy::too_many_arguments)] // Mirrors the trusted extraction activation contract.
async fn activate_search_extraction_with_metadata(
    pool: &PgPool,
    item_id: &str,
    revision_id: &str,
    source_revision_id: &str,
    source_sha256: &[u8],
    extraction_set_id: &str,
    contents: &[String],
    structural_parent_ids: &[String],
    continuation_directions: &[&str],
) {
    let passage_ids = (0..contents.len())
        .map(|index| format!("search-passage-{index:02}"))
        .collect::<Vec<_>>();
    let locators = (0..contents.len())
        .map(|block| {
            if contents[block].contains("JSONKEY") {
                format!(
                    r#"{{"block":{block},"integer":18446744073709551617,"decimal":0.12345678901234567890123456789}}"#
                )
            } else {
                format!(r#"{{"block":{block}}}"#)
            }
        })
        .collect::<Vec<_>>();
    sqlx::query_scalar::<_, String>(
        "SELECT activate_document_extraction(
           $1,$2,$3,$4,$5,$6,'synthetic-parser','search-v1',$6,$7,$8,$9,$10,$11)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .bind(source_revision_id)
    .bind(source_sha256)
    .bind(extraction_set_id)
    .bind(passage_ids)
    .bind(structural_parent_ids)
    .bind(continuation_directions)
    .bind(locators)
    .bind(contents)
    .fetch_one(pool)
    .await
    .expect("activate searchable extraction");
}

async fn search_audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM read_audit WHERE operation='search'")
        .fetch_one(pool)
        .await
        .expect("count search audits")
}

#[allow(clippy::too_many_arguments)] // Targets the complete governed passage identity.
async fn set_search_passage_metadata(
    pool: &PgPool,
    item_id: &str,
    revision_id: &str,
    source_revision_id: &str,
    extraction_set_id: &str,
    passage_id: &str,
    structural_parent_id: Option<&str>,
    continuation_direction: Option<&str>,
) {
    let mut transaction = pool.begin().await.expect("begin trusted passage mutation");
    sqlx::query("ALTER TABLE source_passages DISABLE TRIGGER source_passages_are_immutable")
        .execute(&mut *transaction)
        .await
        .expect("disable passage immutability trigger");
    sqlx::query(
        "UPDATE source_passages
         SET structural_parent_id = coalesce($2, structural_parent_id),
             continuation_direction = coalesce($3, continuation_direction)
         WHERE tenant_id = $1 AND item_id = $4 AND revision_id = $5
           AND source_revision_id = $6 AND extraction_set_id = $7 AND id = $8",
    )
    .bind(ALPHA_TENANT)
    .bind(structural_parent_id)
    .bind(continuation_direction)
    .bind(item_id)
    .bind(revision_id)
    .bind(source_revision_id)
    .bind(extraction_set_id)
    .bind(passage_id)
    .execute(&mut *transaction)
    .await
    .expect("mutate trusted passage metadata");
    sqlx::query("ALTER TABLE source_passages ENABLE TRIGGER source_passages_are_immutable")
        .execute(&mut *transaction)
        .await
        .expect("restore passage immutability trigger");
    transaction
        .commit()
        .await
        .expect("commit trusted passage mutation");
}

async fn read_operation_audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM read_audit WHERE operation='read'")
        .fetch_one(pool)
        .await
        .expect("count read-operation audits")
}

async fn assert_wrong_tenant_document_search(runtime: &PgPool, alice_reader: &str) {
    let mut transaction = runtime.begin().await.expect("begin wrong-tenant search");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(alice_reader.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set wrong-tenant credential digest");
    set_local(&mut transaction, "app.operation", "search").await;
    set_local(&mut transaction, "app.tenant_id", BETA_TENANT).await;
    let error = sqlx::query("SELECT * FROM search_current_memories($1,$2,$3,$4,$5,$6)")
        .bind(BETA_TENANT)
        .bind(ALICE_CREDENTIAL)
        .bind("00000000-0000-0000-0000-000000000299")
        .bind("OLDSETKEY")
        .bind(4096_i32)
        .bind(Option::<Vec<String>>::None)
        .fetch_all(&mut *transaction)
        .await
        .expect_err("cross-tenant search must reject before searching passages");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
    transaction
        .rollback()
        .await
        .expect("rollback wrong-tenant search");
}

#[allow(clippy::too_many_lines)]
async fn assert_document_search_lifecycle_exclusion(
    app: &axum::Router,
    migrator: &PgPool,
    alice_reader: &str,
    item_id: &str,
    revision_id: &str,
) {
    const COLLECTION: &str = "30000000000000000000000000000001";
    const PRINCIPAL: &str = "10000000000000000000000000000001";
    const APP: &str = "a0000000000000000000000000000001";
    let assert_hidden = async || {
        let response = search(
            app,
            Some(alice_reader),
            br#"{"query":"OLDSETKEY","max_context_bytes":4096}"#,
        )
        .await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1["items"], serde_json::json!([]));
    };

    sqlx::query(
        "UPDATE collection_grants SET can_read=false
         WHERE tenant_id=$1 AND collection_id=$2 AND principal_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .bind(PRINCIPAL)
    .execute(migrator)
    .await
    .expect("revoke searchable collection");
    assert_hidden().await;
    sqlx::query(
        "UPDATE collection_grants SET can_read=true
         WHERE tenant_id=$1 AND collection_id=$2 AND principal_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .bind(PRINCIPAL)
    .execute(migrator)
    .await
    .expect("restore searchable collection");

    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .execute(migrator)
    .await
    .expect("withdraw searchable collection");
    assert_hidden().await;
    sqlx::query("UPDATE collections SET withdrawn_at=NULL WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(COLLECTION)
        .execute(migrator)
        .await
        .expect("restore searchable collection");

    sqlx::query("UPDATE items SET active_revision_id='document_search_revision_a2' WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .execute(migrator)
        .await
        .expect("make document revision inactive");
    assert_hidden().await;
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(revision_id)
        .execute(migrator)
        .await
        .expect("restore active document revision");

    sqlx::query("UPDATE items SET active_revision_id=NULL,deleted_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .execute(migrator)
        .await
        .expect("delete searchable document");
    assert_hidden().await;
    sqlx::query(
        "UPDATE items SET deleted_at=NULL,active_revision_id=$3 WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .execute(migrator)
    .await
    .expect("restore searchable document");

    sqlx::query(
        "INSERT INTO revision_subjects (tenant_id,item_id,revision_id,subject_id)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .bind(BOB_SUBJECT)
    .execute(migrator)
    .await
    .expect("make searchable document inapplicable to Alice");
    assert_hidden().await;
    sqlx::query(
        "DELETE FROM revision_subjects
         WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3 AND subject_id=$4",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .bind(BOB_SUBJECT)
    .execute(migrator)
    .await
    .expect("restore searchable document applicability");

    revoke_alice_credential(migrator).await;
    assert_eq!(
        search(
            app,
            Some(alice_reader),
            br#"{"query":"OLDSETKEY","max_context_bytes":4096}"#,
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(migrator).await;
    sqlx::query("UPDATE apps SET active=false WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(APP)
        .execute(migrator)
        .await
        .expect("disable search app");
    assert_eq!(
        search(
            app,
            Some(alice_reader),
            br#"{"query":"OLDSETKEY","max_context_bytes":4096}"#,
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE apps SET active=true WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(APP)
        .execute(migrator)
        .await
        .expect("restore search app");
}

#[allow(clippy::too_many_lines)]
async fn assert_document_derived_storage_foundation(
    migrator: &PgPool,
    runtime: &PgPool,
    alice_reader: &str,
) {
    const COLLECTION_A: &str = "30000000000000000000000000000001";
    const COLLECTION_B: &str = "30000000000000000000000000000004";
    const ITEM_A: &str = "doc_foundation_item_a";
    const ITEM_B: &str = "doc_foundation_item_b";
    const REVISION_A: &str = "doc_foundation_revision_a";
    const REVISION_A_2: &str = "doc_foundation_revision_a2";
    const REVISION_B: &str = "doc_foundation_revision_b";
    const SOURCE_REVISION: &str = "source_revision_same_bytes";
    const SET_A_1: &str = "extraction_set_a_v1";
    const SET_A_2: &str = "extraction_set_a_v2";
    const SET_B_1: &str = "extraction_set_b_v1";
    const SET_B_LIMIT: &str = "extraction_set_b_limit";
    const SET_B_OVER_LIMIT: &str = "extraction_set_b_over_limit";
    const SET_B_CONCURRENT_1: &str = "extraction_set_b_concurrent_1";
    const SET_B_CONCURRENT_2: &str = "extraction_set_b_concurrent_2";
    let source_sha256 = Sha256::digest(b"identical synthetic document bytes");
    let authority_before = authority_epoch(migrator).await;
    let mutation_audits_before =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mutation_audit")
            .fetch_one(migrator)
            .await
            .expect("count user mutation audits before internal extraction");

    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,$2,$3),($1,$4,$5)")
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind(COLLECTION_A)
        .bind(ITEM_B)
        .bind(COLLECTION_B)
        .execute(migrator)
        .await
        .expect("seed document items");
    sqlx::query(
        "INSERT INTO revisions (tenant_id,item_id,id,content) VALUES
           ($1,$2,$4,'DOCUMENT A REVISION ONE'),
           ($1,$2,$5,'DOCUMENT A REVISION TWO'),
           ($1,$3,$6,'DOCUMENT B REVISION ONE')",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(ITEM_B)
    .bind(REVISION_A)
    .bind(REVISION_A_2)
    .bind(REVISION_B)
    .execute(migrator)
    .await
    .expect("seed document revisions");
    sqlx::query(
        "UPDATE items SET active_revision_id=CASE id WHEN $2 THEN $4 ELSE $5 END
         WHERE tenant_id=$1 AND id IN ($2,$3)",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(ITEM_B)
    .bind(REVISION_A)
    .bind(REVISION_B)
    .execute(migrator)
    .await
    .expect("activate document revisions");

    activate_test_extraction(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        &source_sha256,
        SET_A_1,
        "1.0.0",
        "config-v1",
        "A FIRST PASSAGE",
        "A SECOND PASSAGE",
    )
    .await
    .expect("activate complete document extraction");
    assert_eq!(authority_epoch(migrator).await, authority_before + 1);
    activate_test_extraction(
        migrator,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        &source_sha256,
        SET_B_1,
        "1.0.0",
        "config-v1",
        "B FIRST PASSAGE",
        "B SECOND PASSAGE",
    )
    .await
    .expect("activate same bytes under independent document lifecycle");
    assert_eq!(authority_epoch(migrator).await, authority_before + 2);

    let epoch_before_identity_mismatch = authority_epoch(migrator).await;
    let mismatched_sha256 = Sha256::digest(b"different synthetic document bytes");
    let identity_error = activate_test_extraction(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        &mismatched_sha256,
        "identity_mismatch",
        "1.0.0",
        "identity-mismatch",
        "MISMATCH FIRST",
        "MISMATCH SECOND",
    )
    .await
    .expect_err("reject changed bytes for one source revision identity");
    assert_eq!(
        identity_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );
    assert_eq!(
        active_extraction_set(migrator, ITEM_A, REVISION_A).await,
        SET_A_1
    );
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_identity_mismatch
    );
    let identity_id_error = activate_test_extraction(
        migrator,
        ITEM_A,
        REVISION_A,
        "different_source_revision_id",
        &source_sha256,
        "identity_id_mismatch",
        "1.0.0",
        "identity-id-mismatch",
        "MISMATCH FIRST",
        "MISMATCH SECOND",
    )
    .await
    .expect_err("reject a second source revision ID for one document revision");
    assert_eq!(
        identity_id_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );
    assert_eq!(
        active_extraction_set(migrator, ITEM_A, REVISION_A).await,
        SET_A_1
    );
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_identity_mismatch
    );

    let limit_ids = (0..64)
        .map(|index| format!("limit-{index}"))
        .collect::<Vec<_>>();
    let limit_id_refs = limit_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let limit_parents = vec![Some("limit-section"); 64];
    let limit_directions = vec!["none"; 64];
    let limit_locators = (0..64)
        .map(|index| serde_json::json!({"block":index}))
        .collect::<Vec<_>>();
    let limit_contents = vec!["x".repeat(32_768); 64];
    let epoch_before_limit = authority_epoch(migrator).await;
    activate_test_extraction_rows(
        migrator,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        &source_sha256,
        SET_B_LIMIT,
        "limit",
        "exact-2-mib",
        &limit_id_refs,
        &limit_parents,
        &limit_directions,
        &limit_locators,
        &limit_contents,
    )
    .await
    .expect("accept exact two MiB normalized extraction output");
    assert_eq!(authority_epoch(migrator).await, epoch_before_limit + 1);

    let mut over_ids = limit_ids;
    over_ids.push("over-limit".to_owned());
    let over_id_refs = over_ids.iter().map(String::as_str).collect::<Vec<_>>();
    let mut over_parents = limit_parents;
    over_parents.push(Some("limit-section"));
    let mut over_directions = limit_directions;
    over_directions.push("none");
    let mut over_locators = limit_locators;
    over_locators.push(serde_json::json!({"block":64}));
    let mut over_contents = limit_contents;
    over_contents.push("x".to_owned());
    let epoch_before_over_limit = authority_epoch(migrator).await;
    let over_limit_error = activate_test_extraction_rows(
        migrator,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        &source_sha256,
        SET_B_OVER_LIMIT,
        "limit",
        "over-2-mib",
        &over_id_refs,
        &over_parents,
        &over_directions,
        &over_locators,
        &over_contents,
    )
    .await
    .expect_err("reject extraction output above two MiB");
    assert_eq!(
        over_limit_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );
    assert_eq!(
        active_extraction_set(migrator, ITEM_B, REVISION_B).await,
        SET_B_LIMIT
    );
    assert_eq!(authority_epoch(migrator).await, epoch_before_over_limit);

    let epoch_before_concurrent = authority_epoch(migrator).await;
    let first_concurrent = activate_test_extraction(
        migrator,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        &source_sha256,
        SET_B_CONCURRENT_1,
        "concurrent-1",
        "concurrent-1",
        "B CONCURRENT ONE FIRST",
        "B ONE SECOND",
    );
    let second_concurrent = activate_test_extraction(
        migrator,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        &source_sha256,
        SET_B_CONCURRENT_2,
        "concurrent-2",
        "concurrent-2",
        "B CONCURRENT TWO FIRST",
        "B TWO SECOND",
    );
    let (first_result, second_result) = tokio::join!(first_concurrent, second_concurrent);
    first_result.expect("complete first concurrent activation");
    second_result.expect("complete second concurrent activation");
    assert_eq!(authority_epoch(migrator).await, epoch_before_concurrent + 2);
    let concurrent_active = active_extraction_set(migrator, ITEM_B, REVISION_B).await;
    assert!([SET_B_CONCURRENT_1, SET_B_CONCURRENT_2,].contains(&concurrent_active.as_str()));
    let concurrent_search = search(
        &router(runtime.clone()),
        Some(alice_reader),
        br#"{"query":"CONCURRENT","max_context_bytes":4096}"#,
    )
    .await;
    let concurrent_items = concurrent_search.1["items"]
        .as_array()
        .expect("concurrent activation search items")
        .iter()
        .filter(|item| item["item_id"] == ITEM_B)
        .collect::<Vec<_>>();
    assert_eq!(concurrent_items.len(), 2);
    assert_eq!(concurrent_items[0]["reason"], "lexical");
    assert_eq!(concurrent_items[1]["reason"], "adjacent_continuation");
    assert!(
        concurrent_items
            .iter()
            .all(|item| { item["citation"]["extraction_set_id"] == concurrent_active })
    );
    let concurrent_stale = if concurrent_active == SET_B_CONCURRENT_1 {
        SET_B_CONCURRENT_2
    } else {
        SET_B_CONCURRENT_1
    };
    let stale_error = eligible_test_passages(
        runtime,
        alice_reader,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        concurrent_stale,
        "00000000-0000-0000-0000-000000000191",
    )
    .await
    .expect_err("concurrent activation must stale an earlier set identity");
    assert_eq!(
        stale_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("P0003")
    );
    let stale_source_error = eligible_test_passages(
        runtime,
        alice_reader,
        ITEM_B,
        REVISION_B,
        "stale_source_revision",
        &concurrent_active,
        "00000000-0000-0000-0000-000000000199",
    )
    .await
    .expect_err("active extraction must reject a stale source identity");
    assert_eq!(
        stale_source_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("P0003")
    );
    assert_eq!(
        eligible_test_passages(
            runtime,
            alice_reader,
            ITEM_B,
            REVISION_B,
            SOURCE_REVISION,
            &concurrent_active,
            "00000000-0000-0000-0000-000000000192",
        )
        .await
        .expect("current concurrent extraction remains selectable")
        .len(),
        2
    );

    let structure = sqlx::query(
        "SELECT id,structural_parent_id,passage_order,continuation_direction,locator::text AS locator
         FROM source_passages WHERE tenant_id=$1 AND item_id=$2
           AND revision_id=$3 AND extraction_set_id=$4 ORDER BY passage_order",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(REVISION_A)
    .bind(SET_A_1)
    .fetch_all(migrator)
    .await
    .expect("read stored passage structure");
    assert_eq!(structure.len(), 2);
    assert_eq!(
        structure[0].try_get::<String, _>("id").unwrap(),
        "passage-1"
    );
    assert_eq!(structure[0].try_get::<i32, _>("passage_order").unwrap(), 1);
    assert_eq!(
        structure[0]
            .try_get::<String, _>("continuation_direction")
            .unwrap(),
        "to_next"
    );
    assert_eq!(
        structure[0]
            .try_get::<String, _>("structural_parent_id")
            .unwrap(),
        "section-1"
    );
    assert_eq!(
        structure[1]
            .try_get::<String, _>("structural_parent_id")
            .unwrap(),
        "section-1"
    );
    assert_eq!(structure[1].try_get::<i32, _>("passage_order").unwrap(), 2);
    assert_eq!(
        structure[1].try_get::<String, _>("locator").unwrap(),
        r#"{"section": 2}"#
    );

    let independent_sources = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM source_revisions
         WHERE tenant_id=$1 AND source_sha256=$2 AND item_id IN ($3,$4)",
    )
    .bind(ALPHA_TENANT)
    .bind(source_sha256.as_slice())
    .bind(ITEM_A)
    .bind(ITEM_B)
    .fetch_one(migrator)
    .await
    .expect("count collection-isolated source identities");
    assert_eq!(independent_sources, 2);

    for (set_id, passage_ids, parents, directions, locators, contents) in [
        (
            "invalid_duplicate",
            vec!["duplicate", "duplicate"],
            vec![Some("section-3"), Some("section-3")],
            vec!["none", "none"],
            vec![
                serde_json::json!({"section":3}),
                serde_json::json!({"section":4}),
            ],
            vec!["DUPLICATE ONE".to_owned(), "DUPLICATE TWO".to_owned()],
        ),
        (
            "invalid_cross_parent_continuation",
            vec!["child", "parent"],
            vec![Some("section-5"), Some("section-6")],
            vec!["to_next", "from_previous"],
            vec![
                serde_json::json!({"section":5}),
                serde_json::json!({"section":6}),
            ],
            vec!["CHILD".to_owned(), "PARENT".to_owned()],
        ),
        (
            "invalid_direction",
            vec!["direction"],
            vec![Some("section-7")],
            vec!["sideways"],
            vec![serde_json::json!({"section":7})],
            vec!["DIRECTION".to_owned()],
        ),
        (
            "invalid_first_from_previous",
            vec!["first"],
            vec![Some("section-9")],
            vec!["from_previous"],
            vec![serde_json::json!({"section":9})],
            vec!["FIRST".to_owned()],
        ),
        (
            "invalid_last_to_next",
            vec!["last"],
            vec![Some("section-10")],
            vec!["to_next"],
            vec![serde_json::json!({"section":10})],
            vec!["LAST".to_owned()],
        ),
        (
            "invalid_nonreciprocal",
            vec!["one", "two"],
            vec![Some("section-11"), Some("section-11")],
            vec!["to_next", "none"],
            vec![
                serde_json::json!({"section":11}),
                serde_json::json!({"section":12}),
            ],
            vec!["ONE".to_owned(), "TWO".to_owned()],
        ),
        (
            "invalid_budget",
            vec!["oversized"],
            vec![Some("section-8")],
            vec!["none"],
            vec![serde_json::json!({"section":8})],
            vec!["x".repeat(32_769)],
        ),
    ] {
        let epoch_before_failure = authority_epoch(migrator).await;
        let error = activate_test_extraction_rows(
            migrator,
            ITEM_A,
            REVISION_A,
            SOURCE_REVISION,
            &source_sha256,
            set_id,
            "1.0.0",
            set_id,
            &passage_ids,
            &parents,
            &directions,
            &locators,
            &contents,
        )
        .await
        .expect_err("invalid extraction must roll back");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22023")
        );
        assert_eq!(
            active_extraction_set(migrator, ITEM_A, REVISION_A).await,
            SET_A_1
        );
        assert_eq!(extraction_set_count(migrator, ITEM_A, set_id).await, 0);
        assert_eq!(authority_epoch(migrator).await, epoch_before_failure);
    }
    sqlx::query(
        "ALTER TABLE source_passages ADD CONSTRAINT forced_second_passage_failure
         CHECK (id <> 'forced-second-failure') NOT VALID",
    )
    .execute(migrator)
    .await
    .expect("install second-passage insertion failure");
    let epoch_before_second_insert_failure = authority_epoch(migrator).await;
    let second_insert_error = activate_test_extraction_rows(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        &source_sha256,
        "forced_second_insert_failure",
        "forced-failure",
        "forced-failure",
        &["forced-first", "forced-second-failure"],
        &[Some("forced-section"), Some("forced-section")],
        &["to_next", "from_previous"],
        &[
            serde_json::json!({"section":21}),
            serde_json::json!({"section":22}),
        ],
        &["FORCED FIRST".to_owned(), "FORCED SECOND".to_owned()],
    )
    .await
    .expect_err("second passage insertion must roll back its first passage");
    sqlx::query("ALTER TABLE source_passages DROP CONSTRAINT forced_second_passage_failure")
        .execute(migrator)
        .await
        .expect("remove second-passage insertion failure");
    assert_eq!(
        second_insert_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("23514")
    );
    assert_eq!(
        active_extraction_set(migrator, ITEM_A, REVISION_A).await,
        SET_A_1
    );
    assert_eq!(
        extraction_set_count(migrator, ITEM_A, "forced_second_insert_failure").await,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM source_passages
             WHERE tenant_id=$1 AND item_id=$2 AND extraction_set_id=$3",
        )
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind("forced_second_insert_failure")
        .fetch_one(migrator)
        .await
        .expect("count rolled-back passage rows"),
        0
    );
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_second_insert_failure
    );
    let epoch_before_insert_failure = authority_epoch(migrator).await;
    let insert_error = activate_test_extraction(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        &source_sha256,
        "duplicate_pipeline_identity",
        "1.0.0",
        "config-v1",
        "VALID FIRST",
        "VALID SECOND",
    )
    .await
    .expect_err("database validation failure must roll back the complete set");
    assert_eq!(
        insert_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("23505")
    );
    assert_eq!(
        active_extraction_set(migrator, ITEM_A, REVISION_A).await,
        SET_A_1
    );
    assert_eq!(
        extraction_set_count(migrator, ITEM_A, "duplicate_pipeline_identity").await,
        0
    );
    assert_eq!(authority_epoch(migrator).await, epoch_before_insert_failure);

    let epoch_before_reprocessing = authority_epoch(migrator).await;
    activate_test_extraction(
        migrator,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        &source_sha256,
        SET_A_2,
        "2.0.0",
        "config-v2",
        "A REPROCESSED FIRST",
        "A REPROCESSED SECOND",
    )
    .await
    .expect("activate changed parser and config");
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_reprocessing + 1
    );
    assert_eq!(
        active_extraction_set(migrator, ITEM_A, REVISION_A).await,
        SET_A_2
    );
    assert_eq!(extraction_set_count(migrator, ITEM_A, SET_A_1).await, 1);
    assert_eq!(extraction_set_count(migrator, ITEM_A, SET_A_2).await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM source_revisions
             WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3",
        )
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind(REVISION_A)
        .fetch_one(migrator)
        .await
        .expect("count source identity after parser reprocessing"),
        1
    );

    let eligible = eligible_test_passages(
        runtime,
        alice_reader,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        SET_A_2,
        "00000000-0000-0000-0000-000000000181",
    )
    .await
    .expect("read active extraction");
    assert_eq!(eligible.len(), 2);
    assert!(eligible.iter().all(|row| row.0 == SET_A_2));
    assert_eq!(eligible[0].1, "A REPROCESSED FIRST");
    assert!(eligible.iter().all(|row| !row.1.contains("PASSAGE")));
    let selected = eligible_test_passages_selected(
        runtime,
        alice_reader,
        ITEM_A,
        REVISION_A,
        SOURCE_REVISION,
        SET_A_2,
        "00000000-0000-0000-0000-000000000188",
        &["passage-2"],
    )
    .await
    .expect("read one explicitly selected passage");
    assert_eq!(
        selected,
        vec![(SET_A_2.to_owned(), "A REPROCESSED SECOND".to_owned())]
    );
    assert!(
        eligible_test_passages_selected(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            SOURCE_REVISION,
            SET_A_2,
            "00000000-0000-0000-0000-000000000189",
            &[],
        )
        .await
        .is_err()
    );
    let too_many_selector_ids = (0..17)
        .map(|index| format!("selector-{index}"))
        .collect::<Vec<_>>();
    let too_many_selector_refs = too_many_selector_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert!(
        eligible_test_passages_selected(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            SOURCE_REVISION,
            SET_A_2,
            "00000000-0000-0000-0000-000000000190",
            &too_many_selector_refs,
        )
        .await
        .is_err()
    );
    sqlx::query(
        "DELETE FROM source_passages
         WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3
           AND source_revision_id=$4 AND extraction_set_id=$5 AND id='passage-2'",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(REVISION_A)
    .bind(SOURCE_REVISION)
    .bind(SET_A_2)
    .execute(migrator)
    .await
    .expect("corrupt active extraction completeness");
    assert!(
        eligible_test_passages_selected(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            SOURCE_REVISION,
            SET_A_2,
            "00000000-0000-0000-0000-000000000193",
            &["passage-1"],
        )
        .await
        .expect("incomplete active extraction must fail closed")
        .is_empty()
    );
    sqlx::query(
        "INSERT INTO source_passages
           (tenant_id,item_id,revision_id,source_revision_id,extraction_set_id,id,
            structural_parent_id,passage_order,continuation_direction,locator,content,
            search_document)
         VALUES ($1,$2,$3,$4,$5,'passage-2','section-1',2,'from_previous',
                 '{\"section\":2}'::jsonb,'A REPROCESSED SECOND',
                 to_tsvector('simple','A REPROCESSED SECOND'))",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(REVISION_A)
    .bind(SOURCE_REVISION)
    .bind(SET_A_2)
    .execute(migrator)
    .await
    .expect("restore complete active extraction fixture");

    let wrong_tenant_error = eligible_test_passages_as(
        runtime,
        alice_reader,
        BETA_TENANT,
        ALICE_CREDENTIAL,
        ITEM_A,
        REVISION_A,
        "guessed_source",
        "guessed_set",
        "00000000-0000-0000-0000-000000000194",
        &["passage-1"],
    )
    .await
    .expect_err("wrong-tenant selector must fail before identity comparison");
    assert_eq!(
        wrong_tenant_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
    let wrong_credential_error = eligible_test_passages_as(
        runtime,
        alice_reader,
        ALPHA_TENANT,
        "c0000000000000000000000000000002",
        ITEM_A,
        REVISION_A,
        "guessed_source",
        "guessed_set",
        "00000000-0000-0000-0000-000000000195",
        &["passage-1"],
    )
    .await
    .expect_err("wrong credential selector must fail before identity comparison");
    assert_eq!(
        wrong_credential_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
    assert!(
        eligible_test_passages_as(
            runtime,
            alice_reader,
            ALPHA_TENANT,
            ALICE_CREDENTIAL,
            "missing_document_item",
            "missing_document_revision",
            "guessed_source",
            "guessed_set",
            "00000000-0000-0000-0000-000000000196",
            &["passage-1"],
        )
        .await
        .expect("missing target must not disclose extraction identity")
        .is_empty()
    );
    sqlx::query(
        "INSERT INTO revision_subjects (tenant_id,item_id,revision_id,subject_id)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(REVISION_A)
    .bind(BOB_SUBJECT)
    .execute(migrator)
    .await
    .expect("restrict document applicability away from Alice");
    assert!(
        eligible_test_passages_as(
            runtime,
            alice_reader,
            ALPHA_TENANT,
            ALICE_CREDENTIAL,
            ITEM_A,
            REVISION_A,
            "guessed_source",
            "guessed_set",
            "00000000-0000-0000-0000-000000000197",
            &["passage-1"],
        )
        .await
        .expect("inapplicable target must not disclose extraction identity")
        .is_empty()
    );
    sqlx::query(
        "DELETE FROM revision_subjects
         WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3 AND subject_id=$4",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(REVISION_A)
    .bind(BOB_SUBJECT)
    .execute(migrator)
    .await
    .expect("restore document applicability");
    sqlx::query(
        "UPDATE apps SET active=false
         WHERE tenant_id=$1 AND id='a0000000000000000000000000000001'",
    )
    .bind(ALPHA_TENANT)
    .execute(migrator)
    .await
    .expect("disable document reader app");
    let wrong_app_error = eligible_test_passages_as(
        runtime,
        alice_reader,
        ALPHA_TENANT,
        ALICE_CREDENTIAL,
        ITEM_A,
        REVISION_A,
        "guessed_source",
        "guessed_set",
        "00000000-0000-0000-0000-000000000198",
        &["passage-1"],
    )
    .await
    .expect_err("inactive app must fail before identity comparison");
    assert_eq!(
        wrong_app_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
    sqlx::query(
        "UPDATE apps SET active=true
         WHERE tenant_id=$1 AND id='a0000000000000000000000000000001'",
    )
    .bind(ALPHA_TENANT)
    .execute(migrator)
    .await
    .expect("restore document reader app");

    sqlx::query("UPDATE collection_grants SET can_read=false WHERE tenant_id=$1 AND collection_id=$2 AND principal_id='10000000000000000000000000000001'")
        .bind(ALPHA_TENANT).bind(COLLECTION_A).execute(migrator).await.expect("revoke document collection access");
    assert!(
        eligible_test_passages(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            "guessed_source",
            "guessed_set",
            "00000000-0000-0000-0000-000000000182"
        )
        .await
        .expect("revoked eligibility")
        .is_empty()
    );
    let independent_eligible = eligible_test_passages(
        runtime,
        alice_reader,
        ITEM_B,
        REVISION_B,
        SOURCE_REVISION,
        &concurrent_active,
        "00000000-0000-0000-0000-000000000183",
    )
    .await
    .expect("independent collection eligibility");
    assert_eq!(independent_eligible.len(), 2);
    assert!(
        independent_eligible
            .iter()
            .all(|passage| passage.0 == concurrent_active)
    );
    sqlx::query("UPDATE collection_grants SET can_read=true WHERE tenant_id=$1 AND collection_id=$2 AND principal_id='10000000000000000000000000000001'")
        .bind(ALPHA_TENANT).bind(COLLECTION_A).execute(migrator).await.expect("restore document collection access");

    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION_A)
    .execute(migrator)
    .await
    .expect("withdraw document collection");
    assert!(
        eligible_test_passages(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            "guessed_source",
            "guessed_set",
            "00000000-0000-0000-0000-000000000184"
        )
        .await
        .expect("withdrawn eligibility")
        .is_empty()
    );
    sqlx::query("UPDATE collections SET withdrawn_at=NULL WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(COLLECTION_A)
        .execute(migrator)
        .await
        .expect("restore document collection");

    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind(REVISION_A_2)
        .execute(migrator)
        .await
        .expect("switch current document revision");
    assert!(
        eligible_test_passages(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A_2,
            SOURCE_REVISION,
            SET_A_2,
            "00000000-0000-0000-0000-000000000185"
        )
        .await
        .expect("new revision has no active extraction")
        .is_empty()
    );
    assert!(
        eligible_test_passages(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            SOURCE_REVISION,
            SET_A_2,
            "00000000-0000-0000-0000-000000000186"
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind(REVISION_A)
        .execute(migrator)
        .await
        .expect("restore current document revision");

    sqlx::query("UPDATE items SET active_revision_id=NULL,deleted_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT).bind(ITEM_A).execute(migrator).await.expect("delete document item");
    assert!(
        eligible_test_passages(
            runtime,
            alice_reader,
            ITEM_A,
            REVISION_A,
            "guessed_source",
            "guessed_set",
            "00000000-0000-0000-0000-000000000187"
        )
        .await
        .expect("deleted eligibility")
        .is_empty()
    );

    for statement in [
        "UPDATE source_revisions SET tenant_id=tenant_id WHERE tenant_id=$1 AND item_id=$2",
        "UPDATE extraction_sets SET tenant_id=tenant_id WHERE tenant_id=$1 AND item_id=$2",
        "UPDATE source_passages SET tenant_id=tenant_id WHERE tenant_id=$1 AND item_id=$2",
    ] {
        let error = sqlx::query(statement)
            .bind(ALPHA_TENANT)
            .bind(ITEM_A)
            .execute(migrator)
            .await
            .expect_err("derived row update must reject");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("55000")
        );
    }
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT has_function_privilege('agentic_memory_runtime',
          'activate_document_extraction(text,text,text,text,bytea,text,text,text,text,text[],text[],text[],text[],text[])', 'EXECUTE')",
    )
    .fetch_one(migrator)
    .await
    .expect("check internal activation privilege"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mutation_audit")
            .fetch_one(migrator)
            .await
            .expect("count user mutation audits after internal extraction"),
        mutation_audits_before,
        "internal extraction advances authority but must not forge a user mutation identity"
    );

    sqlx::query(
        "UPDATE items SET active_revision_id=NULL,deleted_at=NULL
         WHERE tenant_id=$1 AND id IN ($2,$3)",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM_A)
    .bind(ITEM_B)
    .execute(migrator)
    .await
    .expect("deactivate document fixtures");
    sqlx::query("DELETE FROM revisions WHERE tenant_id=$1 AND item_id IN ($2,$3)")
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind(ITEM_B)
        .execute(migrator)
        .await
        .expect("remove document revisions");
    sqlx::query("DELETE FROM items WHERE tenant_id=$1 AND id IN ($2,$3)")
        .bind(ALPHA_TENANT)
        .bind(ITEM_A)
        .bind(ITEM_B)
        .execute(migrator)
        .await
        .expect("remove document items");
}

#[allow(clippy::too_many_arguments)]
async fn activate_test_extraction(
    pool: &PgPool,
    item_id: &str,
    revision_id: &str,
    source_revision_id: &str,
    source_sha256: &[u8],
    extraction_set_id: &str,
    parser_version: &str,
    config_version: &str,
    first_content: &str,
    second_content: &str,
) -> Result<String, sqlx::Error> {
    activate_test_extraction_rows(
        pool,
        item_id,
        revision_id,
        source_revision_id,
        source_sha256,
        extraction_set_id,
        parser_version,
        config_version,
        &["passage-1", "passage-2"],
        &[Some("section-1"), Some("section-1")],
        &["to_next", "from_previous"],
        &[
            serde_json::json!({"section": 1}),
            serde_json::json!({"section": 2}),
        ],
        &[first_content.to_owned(), second_content.to_owned()],
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn activate_test_extraction_rows(
    pool: &PgPool,
    item_id: &str,
    revision_id: &str,
    source_revision_id: &str,
    source_sha256: &[u8],
    extraction_set_id: &str,
    parser_version: &str,
    config_version: &str,
    passage_ids: &[&str],
    structural_parent_ids: &[Option<&str>],
    continuation_directions: &[&str],
    locators: &[Value],
    contents: &[String],
) -> Result<String, sqlx::Error> {
    let passage_ids = passage_ids
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let structural_parent_ids = structural_parent_ids
        .iter()
        .map(|value| value.map(str::to_owned))
        .collect::<Vec<_>>();
    let continuation_directions = continuation_directions
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let locators = locators.iter().map(Value::to_string).collect::<Vec<_>>();
    sqlx::query_scalar::<_, String>(
        "SELECT activate_document_extraction(
           $1,$2,$3,$4,$5,$6,'synthetic-parser',$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .bind(source_revision_id)
    .bind(source_sha256)
    .bind(extraction_set_id)
    .bind(parser_version)
    .bind(config_version)
    .bind(passage_ids)
    .bind(structural_parent_ids)
    .bind(continuation_directions)
    .bind(locators)
    .bind(contents)
    .fetch_one(pool)
    .await
}

async fn active_extraction_set(pool: &PgPool, item_id: &str, revision_id: &str) -> String {
    sqlx::query_scalar(
        "SELECT extraction_set_id FROM active_extraction_sets
         WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .fetch_one(pool)
    .await
    .expect("read active extraction set")
}

async fn extraction_set_count(pool: &PgPool, item_id: &str, extraction_set_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM extraction_sets
         WHERE tenant_id=$1 AND item_id=$2 AND id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(extraction_set_id)
    .fetch_one(pool)
    .await
    .expect("count extraction sets")
}

async fn eligible_test_passages(
    pool: &PgPool,
    bearer: &str,
    item_id: &str,
    revision_id: &str,
    expected_source_revision_id: &str,
    expected_extraction_set_id: &str,
    request_id: &str,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    eligible_test_passages_selected(
        pool,
        bearer,
        item_id,
        revision_id,
        expected_source_revision_id,
        expected_extraction_set_id,
        request_id,
        &["passage-1", "passage-2"],
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn eligible_test_passages_selected(
    pool: &PgPool,
    bearer: &str,
    item_id: &str,
    revision_id: &str,
    expected_source_revision_id: &str,
    expected_extraction_set_id: &str,
    request_id: &str,
    passage_ids: &[&str],
) -> Result<Vec<(String, String)>, sqlx::Error> {
    eligible_test_passages_as(
        pool,
        bearer,
        ALPHA_TENANT,
        ALICE_CREDENTIAL,
        item_id,
        revision_id,
        expected_source_revision_id,
        expected_extraction_set_id,
        request_id,
        passage_ids,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn eligible_test_passages_as(
    pool: &PgPool,
    bearer: &str,
    tenant_id: &str,
    credential_id: &str,
    item_id: &str,
    revision_id: &str,
    expected_source_revision_id: &str,
    expected_extraction_set_id: &str,
    request_id: &str,
    passage_ids: &[&str],
) -> Result<Vec<(String, String)>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query_scalar::<_, String>("SELECT set_config('app.operation', 'read', true)")
        .fetch_one(&mut *transaction)
        .await?;
    sqlx::query_scalar::<_, String>("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant_id)
        .fetch_one(&mut *transaction)
        .await?;
    let rows = sqlx::query(
        "SELECT extraction_set_id,content
         FROM eligible_source_passages($1,$2,$3,$4,$5,$6,$7,NULL,$8)",
    )
    .bind(tenant_id)
    .bind(credential_id)
    .bind(request_id)
    .bind(item_id)
    .bind(revision_id)
    .bind(expected_source_revision_id)
    .bind(expected_extraction_set_id)
    .bind(
        passage_ids
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *transaction)
    .await?;
    let passages = rows
        .iter()
        .map(|row| Ok((row.try_get("extraction_set_id")?, row.try_get("content")?)))
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    transaction.commit().await?;
    Ok(passages)
}

#[allow(clippy::too_many_lines)]
async fn assert_step4_independent_grant_revocation_and_withdrawal(
    migrator: &PgPool,
    runtime: &PgPool,
    alice_reader: &str,
    bob_reader: &str,
) {
    const COLLECTION: &str = "70000000000000000000000000000001";
    const ITEM: &str = "80000000000000000000000000000001";
    const REVISION: &str = "b0000000000000000000000000000001";
    const CONTENT: &str = "SHARED ALPHA BOB HANDBOOK SENTINEL";
    const ALICE_PRINCIPAL: &str = "10000000000000000000000000000001";
    const BOB_PRINCIPAL: &str = "10000000000000000000000000000002";

    restore_alice_credential(migrator).await;
    let mut seed = migrator.begin().await.expect("begin shared fixture seed");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *seed)
        .await
        .expect("lock tenant before shared fixture seed");
    sqlx::query(
        "INSERT INTO collections (tenant_id,id,app_id,audience_kind,owner_principal_id)
         VALUES ($1,$2,'a0000000000000000000000000000001','restricted',NULL)",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .execute(&mut *seed)
    .await
    .expect("seed restricted shared collection");
    for principal in [ALICE_PRINCIPAL, BOB_PRINCIPAL] {
        sqlx::query(
            "INSERT INTO collection_grants (tenant_id,collection_id,principal_id,can_read)
             VALUES ($1,$2,$3,true)",
        )
        .bind(ALPHA_TENANT)
        .bind(COLLECTION)
        .bind(principal)
        .execute(&mut *seed)
        .await
        .expect("seed independent direct collection grant");
    }
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,$2,$3)")
        .bind(ALPHA_TENANT)
        .bind(ITEM)
        .bind(COLLECTION)
        .execute(&mut *seed)
        .await
        .expect("seed shared item");
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,$2,$3,$4)")
        .bind(ALPHA_TENANT)
        .bind(ITEM)
        .bind(REVISION)
        .bind(CONTENT)
        .execute(&mut *seed)
        .await
        .expect("seed shared current revision");
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
         VALUES ($1,$2,$3,to_tsvector('simple',$4))",
    )
    .bind(ALPHA_TENANT)
    .bind(ITEM)
    .bind(REVISION)
    .bind(CONTENT)
    .execute(&mut *seed)
    .await
    .expect("seed shared lexical representation");
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(ALPHA_TENANT)
        .bind(ITEM)
        .bind(REVISION)
        .execute(&mut *seed)
        .await
        .expect("activate shared revision");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(ALPHA_TENANT)
        .execute(&mut *seed)
        .await
        .expect("advance authority for shared fixture seed");
    seed.commit().await.expect("commit shared fixture seed");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM revision_subjects
             WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3",
        )
        .bind(ALPHA_TENANT)
        .bind(ITEM)
        .bind(REVISION)
        .fetch_one(migrator)
        .await
        .expect("shared revision has unrestricted applicability"),
        0
    );

    let fresh_runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("fresh shared-source runtime sessions");
    let fresh_app = router(fresh_runtime.clone());
    assert_shared_source_visible(
        &fresh_app,
        alice_reader,
        COLLECTION,
        ITEM,
        REVISION,
        CONTENT,
    )
    .await;
    assert_shared_source_visible(&fresh_app, bob_reader, COLLECTION, ITEM, REVISION, CONTENT).await;

    let mut remove_alice = migrator.begin().await.expect("begin Alice grant removal");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *remove_alice)
        .await
        .expect("lock tenant before Alice grant removal");
    sqlx::query(
        "DELETE FROM collection_grants
         WHERE tenant_id=$1 AND collection_id=$2 AND principal_id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .bind(ALICE_PRINCIPAL)
    .execute(&mut *remove_alice)
    .await
    .expect("remove only Alice direct grant");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(ALPHA_TENANT)
        .execute(&mut *remove_alice)
        .await
        .expect("advance authority for Alice grant removal");
    let blocked_app = fresh_app.clone();
    let blocked_reader = alice_reader.to_owned();
    let blocked_read = tokio::spawn(async move {
        request_expected(&blocked_app, &blocked_reader, ITEM, REVISION).await
    });
    wait_for_authority_lock_wait(migrator).await;
    remove_alice
        .commit()
        .await
        .expect("commit Alice grant removal");
    let removed = blocked_read.await.expect("join grant-removal read");
    assert_sanitized_unavailable(&removed);
    assert_step4_read_rejection_audit(
        migrator,
        &removed,
        "unavailable",
        &[ITEM, REVISION, CONTENT],
    )
    .await;
    assert_shared_source_hidden(
        &fresh_app,
        alice_reader,
        COLLECTION,
        ITEM,
        REVISION,
        CONTENT,
    )
    .await;
    assert_shared_source_visible(&fresh_app, bob_reader, COLLECTION, ITEM, REVISION, CONTENT).await;

    let mut restore_alice = migrator
        .begin()
        .await
        .expect("begin Alice grant restoration");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *restore_alice)
        .await
        .expect("lock tenant before Alice grant restoration");
    sqlx::query(
        "INSERT INTO collection_grants (tenant_id,collection_id,principal_id,can_read)
         VALUES ($1,$2,$3,true)",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .bind(ALICE_PRINCIPAL)
    .execute(&mut *restore_alice)
    .await
    .expect("restore Alice direct grant");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(ALPHA_TENANT)
        .execute(&mut *restore_alice)
        .await
        .expect("advance authority for Alice grant restoration");
    restore_alice
        .commit()
        .await
        .expect("commit Alice grant restoration");
    assert_shared_source_visible(
        &fresh_app,
        alice_reader,
        COLLECTION,
        ITEM,
        REVISION,
        CONTENT,
    )
    .await;
    assert_shared_source_visible(&fresh_app, bob_reader, COLLECTION, ITEM, REVISION, CONTENT).await;

    let mut withdraw = migrator.begin().await.expect("begin shared withdrawal");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *withdraw)
        .await
        .expect("lock tenant before shared withdrawal");
    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(COLLECTION)
    .execute(&mut *withdraw)
    .await
    .expect("withdraw shared collection");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(ALPHA_TENANT)
        .execute(&mut *withdraw)
        .await
        .expect("advance authority for shared withdrawal");
    let blocked_app = fresh_app.clone();
    let blocked_reader = alice_reader.to_owned();
    let blocked_withdrawal = tokio::spawn(async move {
        request_expected(&blocked_app, &blocked_reader, ITEM, REVISION).await
    });
    wait_for_authority_lock_wait(migrator).await;
    withdraw.commit().await.expect("commit shared withdrawal");
    let withdrawn = blocked_withdrawal.await.expect("join withdrawal read");
    assert_sanitized_unavailable(&withdrawn);
    assert_step4_read_rejection_audit(
        migrator,
        &withdrawn,
        "unavailable",
        &[ITEM, REVISION, CONTENT],
    )
    .await;
    assert_shared_source_hidden(
        &fresh_app,
        alice_reader,
        COLLECTION,
        ITEM,
        REVISION,
        CONTENT,
    )
    .await;
    assert_shared_source_hidden(&fresh_app, bob_reader, COLLECTION, ITEM, REVISION, CONTENT).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM lexical_representations
             WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3",
        )
        .bind(ALPHA_TENANT)
        .bind(ITEM)
        .bind(REVISION)
        .fetch_one(migrator)
        .await
        .expect("withdrawal retains lexical representation"),
        1
    );
    assert_eq!(
        request(&fresh_app, alice_reader, ALLOWED_ITEM).await.0,
        StatusCode::OK
    );

    let mut expire = migrator.begin().await.expect("begin reader expiry race");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *expire)
        .await
        .expect("lock tenant before reader expiry");
    let blocked_app = fresh_app.clone();
    let blocked_reader = alice_reader.to_owned();
    let expiring_read = tokio::spawn(async move {
        request_expected(
            &blocked_app,
            &blocked_reader,
            ALLOWED_ITEM,
            "50000000000000000000000000000001",
        )
        .await
    });
    wait_for_authority_lock_wait(migrator).await;
    sqlx::query(
        "UPDATE credentials SET expires_at=clock_timestamp()-interval '1 second'
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(&mut *expire)
    .await
    .expect("expire Alice after read authentication");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(ALPHA_TENANT)
        .execute(&mut *expire)
        .await
        .expect("advance authority for reader expiry");
    expire.commit().await.expect("commit reader expiry");
    let expired = expiring_read.await.expect("join expiring read");
    assert_eq!(expired.0, StatusCode::UNAUTHORIZED);
    assert_eq!(expired.1["code"], "unauthenticated");
    assert_step4_read_rejection_audit(
        migrator,
        &expired,
        "unauthenticated",
        &[
            ALLOWED_ITEM,
            "50000000000000000000000000000001",
            "ALLOWED_ALPHA_HANDBOOK",
        ],
    )
    .await;
    restore_alice_credential(migrator).await;
    drop(fresh_app);
    fresh_runtime.close().await;
    wait_for_idle_pool(runtime).await;
}

async fn assert_shared_source_visible(
    app: &axum::Router,
    reader: &str,
    collection_id: &str,
    item_id: &str,
    revision_id: &str,
    content: &str,
) {
    let read = request_expected(app, reader, item_id, revision_id).await;
    assert_eq!(read.0, StatusCode::OK);
    assert_eq!(read.1["revision_id"], revision_id);
    assert_eq!(read.1["content"], content);
    let searched = search_json(app, Some(reader), content).await;
    assert_eq!(searched.0, StatusCode::OK);
    assert!(
        searched.1["items"]
            .as_array()
            .expect("shared search items")
            .iter()
            .any(|item| item["item_id"] == item_id)
    );
    let collections = get_json(app, Some(reader), "/v1/collections?limit=100").await;
    assert_eq!(collections.0, StatusCode::OK);
    assert!(
        collections.1["items"]
            .as_array()
            .expect("shared collection list")
            .iter()
            .any(|collection| collection["collection_id"] == collection_id)
    );
    let items = get_json(app, Some(reader), "/v1/items?limit=100").await;
    assert_eq!(items.0, StatusCode::OK);
    assert!(
        items.1["items"]
            .as_array()
            .expect("shared item list")
            .iter()
            .any(|item| item["item_id"] == item_id && item["revision_id"] == revision_id)
    );
}

async fn assert_shared_source_hidden(
    app: &axum::Router,
    reader: &str,
    collection_id: &str,
    item_id: &str,
    revision_id: &str,
    content: &str,
) {
    let read = request_expected(app, reader, item_id, revision_id).await;
    assert_sanitized_unavailable(&read);
    let searched = search_json(app, Some(reader), content).await;
    assert_eq!(searched.0, StatusCode::OK);
    assert!(
        searched.1["items"]
            .as_array()
            .expect("hidden shared search items")
            .is_empty()
    );
    let search_text = searched.1.to_string();
    assert!(!search_text.contains(item_id));
    assert!(!search_text.contains(revision_id));
    assert!(!search_text.contains(content));
    let collections = get_json(app, Some(reader), "/v1/collections?limit=100").await;
    assert_eq!(collections.0, StatusCode::OK);
    assert!(
        collections.1["items"]
            .as_array()
            .expect("hidden shared collection list")
            .iter()
            .all(|collection| collection["collection_id"] != collection_id)
    );
    assert!(!collections.1.to_string().contains(collection_id));
    let items = get_json(app, Some(reader), "/v1/items?limit=100").await;
    assert_eq!(items.0, StatusCode::OK);
    let item_text = items.1.to_string();
    assert!(
        items.1["items"]
            .as_array()
            .expect("hidden shared item list")
            .iter()
            .all(|item| {
                item["item_id"] != item_id
                    && item["revision_id"] != revision_id
                    && item["collection_id"] != collection_id
            })
    );
    assert!(!item_text.contains(content));
}

async fn assert_step4_read_rejection_audit(
    pool: &PgPool,
    response: &(StatusCode, Value),
    outcome: &str,
    forbidden: &[&str],
) {
    let response_request_id = request_id(&response.1);
    assert_read_rejection_audit(pool, response_request_id, outcome).await;
    let audit = sqlx::query_scalar::<_, String>(
        "SELECT row_to_json(a)::text FROM read_audit a
         WHERE request_id=$1 AND operation='read' AND outcome=$2",
    )
    .bind(response_request_id)
    .bind(outcome)
    .fetch_one(pool)
    .await
    .expect("read request-specific Step 4 audit");
    for value in forbidden {
        assert!(!response.1.to_string().contains(value));
        assert!(!audit.contains(value));
    }
}

#[allow(clippy::too_many_lines)]
async fn assert_step4_session_failure_and_tenant_reuse(
    migrator: &PgPool,
    alice_reader: &str,
    alice_writer: &str,
) {
    let failed_content = "SESSION_FAILURE_UNCOMMITTED_SENTINEL";
    let failed_body = serde_json::json!({
        "content": failed_content,
        "subjects": [ALICE_SUBJECT]
    })
    .to_string();
    let failed_key = "session-failure-uncommitted-01";
    let before_failure = persistence_counts(migrator).await;
    let before_failure_epoch = authority_epoch(migrator).await;
    let failed_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("dedicated failed-session runtime");
    let mut failed_tx = failed_pool
        .begin()
        .await
        .expect("begin uncommitted writer transaction");
    let failed_pid = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
        .fetch_one(&mut *failed_tx)
        .await
        .expect("capture exact failed backend PID");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(alice_writer.as_bytes()).as_slice())
    .fetch_one(&mut *failed_tx)
    .await
    .expect("set failed-session writer digest");
    for (key, value) in [
        ("app.tenant_id", ALPHA_TENANT),
        ("app.operation", "create"),
        ("lock_timeout", "250ms"),
        ("statement_timeout", "1000ms"),
    ] {
        set_local(&mut failed_tx, key, value).await;
    }
    let failed_row = sqlx::query("SELECT * FROM create_private_memory($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(ALPHA_TENANT)
        .bind(WRITER_CREDENTIAL)
        .bind("00000000-0000-0000-0000-000000000071")
        .bind(Sha256::digest(failed_key.as_bytes()).as_slice())
        .bind(canonical_create_digest(failed_body.as_bytes()))
        .bind(failed_content)
        .bind(vec![ALICE_SUBJECT.to_owned()])
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .fetch_one(&mut *failed_tx)
        .await
        .expect("execute uncommitted constrained create");
    let failed_item: String = failed_row.try_get("item_id").expect("uncommitted item ID");
    let failed_revision: String = failed_row
        .try_get("revision_id")
        .expect("uncommitted revision ID");
    let failed_operation: String = failed_row
        .try_get("operation_id")
        .expect("uncommitted operation ID");
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
            .bind(failed_pid)
            .fetch_one(migrator)
            .await
            .expect("terminate exact synthetic runtime backend")
    );
    assert!(
        failed_tx.commit().await.is_err(),
        "terminated writer transaction cannot acknowledge commit"
    );
    assert_ne!(
        runtime_backend_pid(&failed_pool).await,
        failed_pid,
        "dedicated pool must reconnect after its exact backend is terminated"
    );
    failed_pool.close().await;
    assert_eq!(persistence_counts(migrator).await, before_failure);
    assert_eq!(authority_epoch(migrator).await, before_failure_epoch);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT
               (SELECT count(*) FROM items WHERE tenant_id=$1 AND id=$2) +
               (SELECT count(*) FROM revisions WHERE tenant_id=$1 AND item_id=$2 AND id=$3) +
               (SELECT count(*) FROM lexical_representations WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3) +
               (SELECT count(*) FROM idempotency_records WHERE tenant_id=$1 AND operation='create' AND key_digest=$4) +
               (SELECT count(*) FROM mutation_audit WHERE tenant_id=$1 AND operation_id=$5) +
               (SELECT count(*) FROM revisions WHERE content=$6)",
        )
        .bind(ALPHA_TENANT)
        .bind(&failed_item)
        .bind(&failed_revision)
        .bind(Sha256::digest(failed_key.as_bytes()).as_slice())
        .bind(&failed_operation)
        .bind(failed_content)
        .fetch_one(migrator)
        .await
        .expect("uncommitted session failure leaves no persistence"),
        0
    );

    let durable_content = "APPLICATION_RECONSTRUCTION_SENTINEL";
    let durable_body = serde_json::json!({
        "content": durable_content,
        "subjects": [ALICE_SUBJECT]
    })
    .to_string();
    let durable_key = "application-reconstruction-01";
    let before_durable = persistence_counts(migrator).await;
    let before_durable_epoch = authority_epoch(migrator).await;
    let first_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("first reconstructed application pool");
    let first_app = router(first_pool.clone());
    let created = post_memory(
        &first_app,
        alice_writer,
        durable_key,
        durable_body.as_bytes(),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert_eq!(created.1["replayed"], false);
    let durable_item = created.1["item_id"]
        .as_str()
        .expect("durable item ID")
        .to_owned();
    let durable_revision = created.1["revision_id"]
        .as_str()
        .expect("durable revision ID")
        .to_owned();
    let durable_operation = created.1["operation_id"]
        .as_str()
        .expect("durable operation ID")
        .to_owned();
    let durable_completed = created.1["completed_at"].clone();
    drop(first_app);
    first_pool.close().await;

    let second_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("reconstructed application pool");
    let second_app = router(second_pool.clone());
    let read = request_expected(&second_app, alice_reader, &durable_item, &durable_revision).await;
    assert_eq!(read.0, StatusCode::OK);
    assert_eq!(read.1["content"], durable_content);
    let search = search_json(&second_app, Some(alice_reader), durable_content).await;
    assert_eq!(search.0, StatusCode::OK);
    assert!(
        search.1["items"]
            .as_array()
            .expect("reconstructed search items")
            .iter()
            .any(|item| item["item_id"] == durable_item)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM mutation_audit
             WHERE tenant_id=$1 AND operation_id=$2 AND target_id=$3 AND revision_id=$4
               AND outcome='created'",
        )
        .bind(ALPHA_TENANT)
        .bind(&durable_operation)
        .bind(&durable_item)
        .bind(&durable_revision)
        .fetch_one(migrator)
        .await
        .expect("durable mutation audit survives reconstruction"),
        1
    );
    let after_durable = persistence_counts(migrator).await;
    assert_eq!(
        after_durable,
        (
            before_durable.0 + 1,
            before_durable.1 + 1,
            before_durable.2 + 1,
            before_durable.3 + 1,
            before_durable.4 + 1,
            before_durable.5 + 1,
        )
    );
    assert_eq!(authority_epoch(migrator).await, before_durable_epoch + 1);
    let replay = post_memory(
        &second_app,
        alice_writer,
        durable_key,
        durable_body.as_bytes(),
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["replayed"], true);
    assert_eq!(replay.1["item_id"], durable_item);
    assert_eq!(replay.1["revision_id"], durable_revision);
    assert_eq!(replay.1["operation_id"], durable_operation);
    assert_eq!(replay.1["completed_at"], durable_completed);
    assert_eq!(persistence_counts(migrator).await, after_durable);
    assert_eq!(authority_epoch(migrator).await, before_durable_epoch + 1);
    drop(second_app);
    second_pool.close().await;

    assert_step4_single_backend_tenant_isolation(migrator, alice_reader).await;
}

#[allow(clippy::too_many_lines)]
async fn assert_step4_single_backend_tenant_isolation(migrator: &PgPool, alice_reader: &str) {
    const BETA_CREDENTIAL: &str = "c0000000000000000000000000000071";
    const BETA_PRINCIPAL: &str = "20000000000000000000000000000001";
    let beta_reader = random_bearer();
    sqlx::query(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
         SELECT tenant_id,item_id,id,to_tsvector('simple',content)
         FROM revisions WHERE tenant_id=$1 AND item_id=$2 AND id=$3
         ON CONFLICT DO NOTHING",
    )
    .bind(ALPHA_TENANT)
    .bind(ALLOWED_ITEM)
    .bind("50000000000000000000000000000001")
    .execute(migrator)
    .await
    .expect("prepare existing Alpha item for lexical session-reuse proof");
    let mut beta_setup = migrator.begin().await.expect("begin Beta reader setup");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(BETA_TENANT)
        .execute(&mut *beta_setup)
        .await
        .expect("lock Beta authority for reader setup");
    sqlx::query(
        "INSERT INTO credentials
           (tenant_id,id,principal_id,app_id,token_digest,credential_class,
            allowed_operations,issued_at,expires_at)
         VALUES ($1,$2,$3,'a0000000000000000000000000000002',$4,
                 'agent_reader',ARRAY['list','search','read'],clock_timestamp(),
                 clock_timestamp()+interval '24 hours')",
    )
    .bind(BETA_TENANT)
    .bind(BETA_CREDENTIAL)
    .bind(BETA_PRINCIPAL)
    .bind(Sha256::digest(beta_reader.as_bytes()).as_slice())
    .execute(&mut *beta_setup)
    .await
    .expect("insert digest-only Beta reader");
    sqlx::query(
        "INSERT INTO collection_grants (tenant_id,collection_id,principal_id,can_read)
         VALUES ($1,'30000000000000000000000000000003',$2,true)",
    )
    .bind(BETA_TENANT)
    .bind(BETA_PRINCIPAL)
    .execute(&mut *beta_setup)
    .await
    .expect("grant Beta reader its restricted collection");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(BETA_TENANT)
        .execute(&mut *beta_setup)
        .await
        .expect("advance Beta authority");
    beta_setup.commit().await.expect("commit Beta reader setup");

    let one_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("single-backend tenant-reuse pool");
    let one_app = router(one_pool.clone());
    let first_pid = runtime_backend_pid(&one_pool).await;
    assert_tenant_reader_surface(
        migrator,
        &one_app,
        alice_reader,
        ALPHA_TENANT,
        ALICE_CREDENTIAL,
        "10000000000000000000000000000001",
        "a0000000000000000000000000000001",
        ALLOWED_ITEM,
        "50000000000000000000000000000001",
        "ALLOWED_ALPHA_HANDBOOK",
        FOREIGN_ITEM,
        "FORBIDDEN_BETA_COMPANY",
    )
    .await;
    assert_eq!(runtime_backend_pid(&one_pool).await, first_pid);
    assert_tenant_reader_surface(
        migrator,
        &one_app,
        &beta_reader,
        BETA_TENANT,
        BETA_CREDENTIAL,
        BETA_PRINCIPAL,
        "a0000000000000000000000000000002",
        FOREIGN_ITEM,
        "50000000000000000000000000000003",
        "FORBIDDEN_BETA_COMPANY",
        ALLOWED_ITEM,
        "ALLOWED_ALPHA_HANDBOOK",
    )
    .await;
    assert_eq!(runtime_backend_pid(&one_pool).await, first_pid);

    let mut aborted = one_pool
        .begin()
        .await
        .expect("begin aborted Alpha transaction");
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
            .fetch_one(&mut *aborted)
            .await
            .expect("PID inside aborted transaction"),
        first_pid
    );
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(alice_reader.as_bytes()).as_slice())
    .fetch_one(&mut *aborted)
    .await
    .expect("set Alpha digest before safe failure");
    set_local(&mut aborted, "app.tenant_id", ALPHA_TENANT).await;
    set_local(&mut aborted, "app.operation", "read").await;
    let division_error = sqlx::query("SELECT 1 / 0")
        .execute(&mut *aborted)
        .await
        .expect_err("safe division error must abort the synthetic transaction");
    assert_eq!(
        division_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22012")
    );
    let failed_state = sqlx::query("SELECT 1")
        .execute(&mut *aborted)
        .await
        .expect_err("transaction must remain failed until explicit rollback");
    assert_eq!(
        failed_state
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("25P02")
    );
    aborted
        .rollback()
        .await
        .expect("explicitly roll back failed Alpha transaction");
    assert_eq!(runtime_backend_pid(&one_pool).await, first_pid);
    assert_tenant_reader_surface(
        migrator,
        &one_app,
        &beta_reader,
        BETA_TENANT,
        BETA_CREDENTIAL,
        BETA_PRINCIPAL,
        "a0000000000000000000000000000002",
        FOREIGN_ITEM,
        "50000000000000000000000000000003",
        "FORBIDDEN_BETA_COMPANY",
        ALLOWED_ITEM,
        "ALLOWED_ALPHA_HANDBOOK",
    )
    .await;
    assert_eq!(runtime_backend_pid(&one_pool).await, first_pid);
    assert_tenant_reader_surface(
        migrator,
        &one_app,
        alice_reader,
        ALPHA_TENANT,
        ALICE_CREDENTIAL,
        "10000000000000000000000000000001",
        "a0000000000000000000000000000001",
        ALLOWED_ITEM,
        "50000000000000000000000000000001",
        "ALLOWED_ALPHA_HANDBOOK",
        FOREIGN_ITEM,
        "FORBIDDEN_BETA_COMPANY",
    )
    .await;
    assert_eq!(runtime_backend_pid(&one_pool).await, first_pid);
    let clean_context = sqlx::query(
        "SELECT nullif(current_setting('app.tenant_id',true),'') IS NULL AS tenant_empty,
                nullif(current_setting('app.credential_digest',true),'') IS NULL AS digest_empty,
                nullif(current_setting('app.operation',true),'') IS NULL AS operation_empty",
    )
    .fetch_one(&one_pool)
    .await
    .expect("inspect transaction-local context outside requests");
    for field in ["tenant_empty", "digest_empty", "operation_empty"] {
        assert!(clean_context.try_get::<bool, _>(field).expect("clean GUC"));
    }
    drop(one_app);
    one_pool.close().await;
}

#[allow(clippy::too_many_arguments)]
async fn assert_tenant_reader_surface(
    migrator: &PgPool,
    app: &axum::Router,
    reader: &str,
    tenant_id: &str,
    credential_id: &str,
    principal_id: &str,
    app_id: &str,
    own_item: &str,
    own_revision: &str,
    own_content: &str,
    foreign_item: &str,
    foreign_content: &str,
) {
    let own_read = request_expected(app, reader, own_item, own_revision).await;
    assert_eq!(own_read.0, StatusCode::OK);
    assert_eq!(own_read.1["content"], own_content);
    let own_search = search_json(app, Some(reader), own_content).await;
    assert_eq!(own_search.0, StatusCode::OK);
    assert!(
        own_search.1["items"]
            .as_array()
            .expect("tenant search items")
            .iter()
            .any(|item| item["item_id"] == own_item)
    );
    let own_list = get_json(app, Some(reader), "/v1/items?limit=100").await;
    assert_eq!(own_list.0, StatusCode::OK);
    let own_list_items = own_list.1["items"].as_array().expect("tenant list items");
    assert!(
        own_list_items
            .iter()
            .any(|item| item["item_id"] == own_item)
    );
    let list_text = own_list.1.to_string();
    assert!(!list_text.contains(foreign_item));
    assert!(!list_text.contains(foreign_content));

    let foreign_read = request(app, reader, foreign_item).await;
    assert_sanitized_unavailable(&foreign_read);
    assert_reader_audit_attribution(
        migrator,
        &foreign_read.1,
        "read",
        "unavailable",
        tenant_id,
        credential_id,
        principal_id,
        app_id,
        &[foreign_item, foreign_content],
    )
    .await;
    let foreign_search = search_json(app, Some(reader), foreign_content).await;
    assert_eq!(foreign_search.0, StatusCode::OK);
    assert!(
        foreign_search.1["items"]
            .as_array()
            .expect("cross-tenant search items")
            .is_empty()
    );
    assert_reader_audit_attribution(
        migrator,
        &foreign_search.1,
        "search",
        "released",
        tenant_id,
        credential_id,
        principal_id,
        app_id,
        &[foreign_item, foreign_content],
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn assert_reader_audit_attribution(
    pool: &PgPool,
    response: &Value,
    operation: &str,
    outcome: &str,
    tenant_id: &str,
    credential_id: &str,
    principal_id: &str,
    app_id: &str,
    forbidden: &[&str],
) {
    let audit = sqlx::query_scalar::<_, String>(
        "SELECT row_to_json(a)::text FROM read_audit a
         WHERE request_id=$1 AND operation=$2 AND outcome=$3 AND target_id IS NULL
           AND tenant_id=$4 AND credential_id=$5 AND principal_id=$6 AND app_id=$7",
    )
    .bind(request_id(response))
    .bind(operation)
    .bind(outcome)
    .bind(tenant_id)
    .bind(credential_id)
    .bind(principal_id)
    .bind(app_id)
    .fetch_one(pool)
    .await
    .expect("request-specific tenant-attributed audit");
    for value in forbidden {
        assert!(!response.to_string().contains(value));
        assert!(!audit.contains(value));
    }
}

async fn runtime_backend_pid(pool: &PgPool) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(pool)
        .await
        .expect("read exact single-pool backend PID")
}

struct CooperatingHostFixture {
    item_id: Option<String>,
    revision_id: Option<String>,
    content: Option<String>,
    dependent_draft: Option<String>,
}

impl CooperatingHostFixture {
    fn from_authorized_read(read: &(StatusCode, Value), dependent_draft: &str) -> Self {
        assert_eq!(read.0, StatusCode::OK);
        Self {
            item_id: Some(
                read.1["item_id"]
                    .as_str()
                    .expect("host item evidence")
                    .to_owned(),
            ),
            revision_id: Some(
                read.1["revision_id"]
                    .as_str()
                    .expect("host revision evidence")
                    .to_owned(),
            ),
            content: Some(
                read.1["content"]
                    .as_str()
                    .expect("host content evidence")
                    .to_owned(),
            ),
            dependent_draft: Some(dependent_draft.to_owned()),
        }
    }

    async fn revalidate(&mut self, app: &axum::Router, reader: &str) -> StatusCode {
        let (Some(item_id), Some(revision_id)) = (self.item_id.clone(), self.revision_id.clone())
        else {
            return StatusCode::NOT_FOUND;
        };
        let response = request_expected(app, reader, &item_id, &revision_id).await;
        if response.0 == StatusCode::OK {
            assert_eq!(response.1["item_id"], item_id);
            assert_eq!(response.1["revision_id"], revision_id);
            assert_eq!(response.1["content"].as_str(), self.content.as_deref());
        } else {
            let expected_code = match response.0 {
                StatusCode::CONFLICT => "stale_context",
                StatusCode::NOT_FOUND => "unavailable",
                StatusCode::UNAUTHORIZED => "unauthenticated",
                StatusCode::SERVICE_UNAVAILABLE => "storage_unavailable",
                status => panic!("unexpected host revalidation status: {status}"),
            };
            assert_eq!(response.1["code"], expected_code);
            self.discard();
        }
        response.0
    }

    async fn next_prompt(
        &mut self,
        app: &axum::Router,
        reader: &str,
    ) -> (StatusCode, Option<String>) {
        let status = self.revalidate(app, reader).await;
        let prompt = (status == StatusCode::OK).then(|| {
            format!(
                "HOST_NEXT_PROMPT_SENTINEL {}",
                self.content.as_deref().expect("revalidated host evidence")
            )
        });
        (status, prompt)
    }

    async fn release_draft(
        &mut self,
        app: &axum::Router,
        reader: &str,
    ) -> (StatusCode, Option<String>) {
        let status = self.revalidate(app, reader).await;
        let released = (status == StatusCode::OK)
            .then(|| self.dependent_draft.take())
            .flatten();
        (status, released)
    }

    fn discard(&mut self) {
        self.item_id = None;
        self.revision_id = None;
        self.content = None;
        self.dependent_draft = None;
    }

    fn retained_text(&self) -> String {
        format!(
            "{:?}{:?}{:?}{:?}",
            self.item_id, self.revision_id, self.content, self.dependent_draft
        )
    }
}

#[allow(clippy::too_many_lines)]
async fn assert_step4_cooperating_host_discards_stale_evidence(
    app: &axum::Router,
    migrator: &PgPool,
    alice_reader: &str,
    alice_writer: &str,
) {
    let mut positive = create_host_fixture(
        app,
        alice_reader,
        alice_writer,
        "host-positive-01",
        "HOST_POSITIVE_CONTENT_SENTINEL",
        "HOST_POSITIVE_DRAFT_SENTINEL",
    )
    .await;
    let positive_prompt = positive.next_prompt(app, alice_reader).await;
    assert_eq!(positive_prompt.0, StatusCode::OK);
    assert!(
        positive_prompt
            .1
            .as_deref()
            .is_some_and(|prompt| prompt.contains("HOST_POSITIVE_CONTENT_SENTINEL"))
    );
    let positive_release = positive.release_draft(app, alice_reader).await;
    assert_eq!(positive_release.0, StatusCode::OK);
    assert_eq!(
        positive_release.1.as_deref(),
        Some("HOST_POSITIVE_DRAFT_SENTINEL")
    );

    let mut stale = create_host_fixture(
        app,
        alice_reader,
        alice_writer,
        "host-stale-create-01",
        "HOST_STALE_CONTENT_SENTINEL",
        "HOST_STALE_DRAFT_SENTINEL",
    )
    .await;
    let stale_item = stale
        .item_id
        .as_deref()
        .expect("stale host item")
        .to_owned();
    let stale_revision = stale
        .revision_id
        .as_deref()
        .expect("stale host revision")
        .to_owned();
    let corrected_body = serde_json::json!({
        "expected_revision_id": stale_revision,
        "content": "HOST_CORRECTED_CONTENT_SENTINEL",
        "subjects": [ALICE_SUBJECT]
    })
    .to_string();
    assert_eq!(
        put_memory(
            app,
            alice_writer,
            "host-stale-correct-01",
            &stale_item,
            corrected_body.as_bytes(),
        )
        .await
        .0,
        StatusCode::OK
    );
    let stale_prompt = stale.next_prompt(app, alice_reader).await;
    assert_eq!(stale_prompt.0, StatusCode::CONFLICT);
    assert!(stale_prompt.1.is_none());
    for marker in [
        "HOST_STALE_CONTENT_SENTINEL",
        "HOST_STALE_DRAFT_SENTINEL",
        "HOST_NEXT_PROMPT_SENTINEL",
    ] {
        assert!(!stale.retained_text().contains(marker));
        assert!(!format!("{:?}", stale_prompt.1).contains(marker));
    }

    let mut forgotten = create_host_fixture(
        app,
        alice_reader,
        alice_writer,
        "host-forget-create-01",
        "HOST_FORGOTTEN_CONTENT_SENTINEL",
        "HOST_FORGOTTEN_DRAFT_SENTINEL",
    )
    .await;
    let forgotten_item = forgotten
        .item_id
        .as_deref()
        .expect("forgotten host item")
        .to_owned();
    let forgotten_revision = forgotten
        .revision_id
        .as_deref()
        .expect("forgotten host revision")
        .to_owned();
    let current_prompt = forgotten.next_prompt(app, alice_reader).await;
    assert_eq!(current_prompt.0, StatusCode::OK);
    assert!(
        current_prompt
            .1
            .as_deref()
            .is_some_and(|prompt| prompt.contains("HOST_FORGOTTEN_CONTENT_SENTINEL"))
    );
    let forget_body = serde_json::json!({"expected_revision_id": forgotten_revision}).to_string();
    assert_eq!(
        delete_memory(
            app,
            alice_writer,
            "host-forget-key-01",
            &forgotten_item,
            forget_body.as_bytes(),
        )
        .await
        .0,
        StatusCode::OK
    );
    let forgotten_release = forgotten.release_draft(app, alice_reader).await;
    assert_eq!(forgotten_release.0, StatusCode::NOT_FOUND);
    assert!(forgotten_release.1.is_none());
    for marker in [
        "HOST_FORGOTTEN_CONTENT_SENTINEL",
        "HOST_FORGOTTEN_DRAFT_SENTINEL",
    ] {
        assert!(!forgotten.retained_text().contains(marker));
        assert!(!format!("{:?}", forgotten_release.1).contains(marker));
    }

    let mut revoked = create_host_fixture(
        app,
        alice_reader,
        alice_writer,
        "host-revoke-create-01",
        "HOST_REVOKED_CONTENT_SENTINEL",
        "HOST_REVOKED_DRAFT_SENTINEL",
    )
    .await;
    revoke_alice_credential(migrator).await;
    let revoked_prompt = revoked.next_prompt(app, alice_reader).await;
    assert_eq!(revoked_prompt.0, StatusCode::UNAUTHORIZED);
    assert!(revoked_prompt.1.is_none());
    for marker in [
        "HOST_REVOKED_CONTENT_SENTINEL",
        "HOST_REVOKED_DRAFT_SENTINEL",
        "HOST_NEXT_PROMPT_SENTINEL",
    ] {
        assert!(!revoked.retained_text().contains(marker));
        assert!(!format!("{:?}", revoked_prompt.1).contains(marker));
    }
    restore_alice_credential(migrator).await;

    let mut unavailable = create_host_fixture(
        app,
        alice_reader,
        alice_writer,
        "host-storage-create-01",
        "HOST_STORAGE_CONTENT_SENTINEL",
        "HOST_STORAGE_DRAFT_SENTINEL",
    )
    .await;
    let closed_pool = PgPoolOptions::new()
        .connect_lazy(&test_database_url(RUNTIME_URL))
        .expect("lazy cooperating-host closed pool");
    closed_pool.close().await;
    let unavailable_prompt = unavailable
        .next_prompt(&router(closed_pool), alice_reader)
        .await;
    assert_eq!(unavailable_prompt.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(unavailable_prompt.1.is_none());
    for marker in [
        "HOST_STORAGE_CONTENT_SENTINEL",
        "HOST_STORAGE_DRAFT_SENTINEL",
        "HOST_NEXT_PROMPT_SENTINEL",
    ] {
        assert!(!unavailable.retained_text().contains(marker));
        assert!(!format!("{:?}", unavailable_prompt.1).contains(marker));
    }
}

async fn create_host_fixture(
    app: &axum::Router,
    alice_reader: &str,
    alice_writer: &str,
    key: &str,
    content: &str,
    dependent_draft: &str,
) -> CooperatingHostFixture {
    let body = serde_json::json!({"content": content, "subjects": [ALICE_SUBJECT]}).to_string();
    let created = post_memory(app, alice_writer, key, body.as_bytes()).await;
    assert_eq!(created.0, StatusCode::CREATED);
    let item_id = created.1["item_id"].as_str().expect("host item ID");
    let revision_id = created.1["revision_id"].as_str().expect("host revision ID");
    let read = request_expected(app, alice_reader, item_id, revision_id).await;
    assert_eq!(read.0, StatusCode::OK);
    assert_eq!(read.1["item_id"], item_id);
    assert_eq!(read.1["revision_id"], revision_id);
    assert_eq!(read.1["content"], content);
    CooperatingHostFixture::from_authorized_read(&read, dependent_draft)
}

#[allow(clippy::too_many_lines)]
async fn assert_trusted_private_create(
    app: &axum::Router,
    migrator: &PgPool,
    runtime: &PgPool,
    purge_worker: &PgPool,
    alice_reader: &str,
    bob_reader: &str,
    alice_writer: &str,
) {
    let content = "PRIVATE_CREATE_ATOMIC_SENTINEL";
    let body = format!(r#"{{"content":"{content}","subjects":["{ALICE_SUBJECT}"]}}"#);
    let idempotency_key = "create-atomic-0001";
    let initial_epoch = authority_epoch(migrator).await;
    let created = post_memory(app, alice_writer, idempotency_key, body.as_bytes()).await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert_eq!(created.1["status"], "ready");
    assert_eq!(created.1["replayed"], false);
    assert_eq!(created.1["lexical_status"], "ready");
    assert_eq!(created.1["semantic_status"], "not_requested");
    let item_id = created.1["item_id"].as_str().expect("created item ID");
    let revision_id = created.1["revision_id"]
        .as_str()
        .expect("created revision ID");
    let operation_id = created.1["operation_id"]
        .as_str()
        .expect("created operation ID");
    for id in [item_id, revision_id, operation_id] {
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    let create_completion = stored_completion(migrator, "create", idempotency_key).await;
    assert_eq!(created.1["completed_at"], create_completion.0);
    assert!(create_completion.1);

    let persisted = sqlx::query(
        "SELECT i.active_revision_id, r.content,
                l.document = to_tsvector('simple', r.content) AS lexical_ready,
                c.owner_principal_id, c.app_id,
                (SELECT count(*) FROM revision_subjects rs
                 WHERE rs.tenant_id = r.tenant_id AND rs.item_id = r.item_id
                   AND rs.revision_id = r.id AND rs.subject_id = $3) AS subject_count,
                (SELECT count(*) FROM idempotency_records d
                 WHERE d.tenant_id = r.tenant_id AND d.item_id = r.item_id
                   AND d.revision_id = r.id AND d.operation_id = $4
                   AND d.key_digest = $5 AND d.request_digest = $7
                   AND d.expires_at > d.completed_at
                   AND d.expires_at <= d.completed_at + interval '24 hours') AS idempotency_count,
                (SELECT count(*) FROM mutation_audit a
                 WHERE a.request_id = $6 AND a.target_id = r.item_id
                   AND a.revision_id = r.id AND a.operation_id = $4
                   AND a.outcome = 'created'
                   AND a.authority_epoch = $8) AS audit_count
         FROM items i
         JOIN revisions r ON r.tenant_id = i.tenant_id AND r.item_id = i.id
                          AND r.id = i.active_revision_id
         JOIN lexical_representations l
           ON l.tenant_id = r.tenant_id AND l.item_id = r.item_id AND l.revision_id = r.id
         JOIN collections c ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
         WHERE i.tenant_id = $1 AND i.id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(ALICE_SUBJECT)
    .bind(operation_id)
    .bind(Sha256::digest(idempotency_key.as_bytes()).as_slice())
    .bind(request_id(&created.1))
    .bind(canonical_create_digest(body.as_bytes()).as_slice())
    .bind(initial_epoch + 1)
    .fetch_one(migrator)
    .await
    .expect("created memory committed atomically");
    assert_eq!(
        persisted
            .try_get::<String, _>("active_revision_id")
            .expect("active revision"),
        revision_id
    );
    assert_eq!(
        persisted.try_get::<String, _>("content").expect("content"),
        content
    );
    assert!(
        persisted
            .try_get::<bool, _>("lexical_ready")
            .expect("lexical ready")
    );
    assert_eq!(
        persisted
            .try_get::<String, _>("owner_principal_id")
            .expect("private owner"),
        "10000000000000000000000000000001"
    );
    assert_eq!(
        persisted.try_get::<String, _>("app_id").expect("bound app"),
        "a0000000000000000000000000000001"
    );
    for count in ["subject_count", "idempotency_count", "audit_count"] {
        assert_eq!(
            persisted
                .try_get::<i64, _>(count)
                .expect("atomic row count"),
            1
        );
    }
    assert_eq!(authority_epoch(migrator).await, initial_epoch + 1);
    let fresh_runtime = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("fresh exit-flow runtime session");
    let fresh_app = router(fresh_runtime);
    let fresh_search = search_json(&fresh_app, Some(alice_reader), "PRIVATE CREATE ATOMIC").await;
    assert_eq!(fresh_search.0, StatusCode::OK);
    assert!(
        fresh_search.1["items"]
            .as_array()
            .is_some_and(|items| { items.iter().any(|item| item["item_id"] == item_id) })
    );
    let fresh_read = request_expected(&fresh_app, alice_reader, item_id, revision_id).await;
    assert_eq!(fresh_read.0, StatusCode::OK);
    assert_eq!(fresh_read.1["revision_id"], revision_id);
    assert_eq!(fresh_read.1["content"], content);
    let after_original = persistence_counts(migrator).await;
    let replayed = post_memory(app, alice_writer, idempotency_key, body.as_bytes()).await;
    assert_eq!(replayed.0, StatusCode::OK);
    assert_eq!(replayed.1["replayed"], true);
    assert!(replayed.1.get("lexical_status").is_none());
    assert!(replayed.1.get("semantic_status").is_none());
    for field in ["item_id", "revision_id", "operation_id", "completed_at"] {
        assert_eq!(replayed.1[field], created.1[field]);
    }
    assert_eq!(
        write_request_audit_count(
            migrator,
            &replayed.1,
            idempotency_key,
            "replayed",
            initial_epoch + 1,
        )
        .await,
        1
    );
    assert_eq!(persistence_counts(migrator).await, after_original);
    assert_eq!(authority_epoch(migrator).await, initial_epoch + 1);
    let expired_while_waiting = replay_while_writer_expires_on_item(
        app,
        migrator,
        alice_writer,
        "create",
        idempotency_key,
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(expired_while_waiting.0, StatusCode::UNAUTHORIZED);
    assert_eq!(expired_while_waiting.1["code"], "unauthenticated");
    assert!(expired_while_waiting.1.get("item_id").is_none());
    assert_eq!(persistence_counts(migrator).await, after_original);
    assert_eq!(authority_epoch(migrator).await, initial_epoch + 1);
    let changed_body =
        format!(r#"{{"content":"CREATE_REPLAY_CHANGED_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let changed = post_memory(app, alice_writer, idempotency_key, changed_body.as_bytes()).await;
    assert_eq!(changed.0, StatusCode::CONFLICT);
    assert_eq!(changed.1["code"], "idempotency_conflict");
    assert!(!changed.1.to_string().contains("SENTINEL"));
    assert_eq!(
        write_request_audit_count(
            migrator,
            &changed.1,
            idempotency_key,
            "idempotency_conflict",
            initial_epoch + 1,
        )
        .await,
        1
    );
    assert_eq!(persistence_counts(migrator).await, after_original);
    assert_eq!(authority_epoch(migrator).await, initial_epoch + 1);

    let concurrent_body = format!(
        r#"{{"content":"CONCURRENT_CREATE_REPLAY_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let before_concurrent = persistence_counts(migrator).await;
    let concurrent_epoch = authority_epoch(migrator).await;
    let left_app = app.clone();
    let right_app = app.clone();
    let left_writer = alice_writer.to_owned();
    let right_writer = alice_writer.to_owned();
    let left_body = concurrent_body.clone();
    let right_body = concurrent_body.clone();
    let mut concurrent_gate = migrator.begin().await.expect("begin create replay gate");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *concurrent_gate)
        .await
        .expect("hold create replay tenant gate");
    let left = tokio::spawn(async move {
        post_memory(
            &left_app,
            &left_writer,
            "concurrent-create-replay-01",
            left_body.as_bytes(),
        )
        .await
    });
    let right = tokio::spawn(async move {
        post_memory(
            &right_app,
            &right_writer,
            "concurrent-create-replay-01",
            right_body.as_bytes(),
        )
        .await
    });
    wait_for_write_waiters(migrator, "create_private_memory", 2).await;
    concurrent_gate
        .commit()
        .await
        .expect("release create replay gate");
    let left = left.await.expect("join left create replay");
    let right = right.await.expect("join right create replay");
    assert_eq!(
        [left.0, right.0]
            .into_iter()
            .filter(|status| *status == StatusCode::CREATED)
            .count(),
        1
    );
    assert_eq!(
        [left.0, right.0]
            .into_iter()
            .filter(|status| *status == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(left.1["item_id"], right.1["item_id"]);
    assert_eq!(left.1["revision_id"], right.1["revision_id"]);
    assert_eq!(left.1["operation_id"], right.1["operation_id"]);
    assert_eq!(left.1["completed_at"], right.1["completed_at"]);
    assert_ne!(left.1["replayed"], right.1["replayed"]);
    for response in [&left.1, &right.1] {
        if response["replayed"] == true {
            assert!(response.get("lexical_status").is_none());
            assert!(response.get("semantic_status").is_none());
        }
    }
    let after_concurrent = persistence_counts(migrator).await;
    assert_eq!(
        after_concurrent,
        (
            before_concurrent.0 + 1,
            before_concurrent.1 + 1,
            before_concurrent.2 + 1,
            before_concurrent.3 + 1,
            before_concurrent.4 + 1,
            before_concurrent.5 + 1,
        )
    );
    assert_eq!(authority_epoch(migrator).await, concurrent_epoch + 1);
    let immutable = sqlx::query(
        "UPDATE revisions SET content = $4 \
         WHERE tenant_id = $1 AND item_id = $2 AND id = $3",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(revision_id)
    .bind("IMMUTABILITY_VIOLATION")
    .execute(migrator)
    .await;
    assert!(
        immutable.is_err(),
        "committed revisions cannot be changed in place"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT content FROM revisions WHERE tenant_id = $1 AND item_id = $2 AND id = $3",
        )
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(revision_id)
        .fetch_one(migrator)
        .await
        .expect("immutable revision remains readable"),
        content
    );
    let after_success = persistence_counts(migrator).await;

    assert_agent_write_denial_matrix(app, migrator, runtime, alice_reader, item_id, revision_id)
        .await;

    let overlapping_credential = sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class,
          allowed_operations, issued_at, expires_at)
         VALUES ($1, 'c0000000000000000000000000000099',
                 '10000000000000000000000000000001',
                 'a0000000000000000000000000000001', $2,
                 'trusted_writer', ARRAY['create', 'read'],
                 clock_timestamp(), clock_timestamp() + interval '1 hour')",
    )
    .bind(ALPHA_TENANT)
    .bind(vec![0xa5_u8; 32])
    .execute(migrator)
    .await;
    assert!(
        overlapping_credential.is_err(),
        "credential operation classes must not overlap"
    );

    assert_null_subjects_rejected(runtime, alice_writer).await;
    assert_eq!(persistence_counts(migrator).await, after_success);
    let missing_tenant_context = direct_create_without_tenant(runtime, alice_writer).await;
    let after_missing_tenant = persistence_counts(migrator).await;
    assert!(
        missing_tenant_context.is_err(),
        "writer mutation must require transaction-local tenant context"
    );
    assert_eq!(after_missing_tenant, after_success);
    assert_eq!(authority_epoch(migrator).await, initial_epoch + 2);

    let null_audit_operation = sqlx::query(
        "SELECT record_request_rejection(
           '00000000-0000-0000-0000-000000000098', NULL, 'malformed')",
    )
    .execute(runtime)
    .await;
    let null_audit_rows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM create_rejection_audit
         WHERE request_id = '00000000-0000-0000-0000-000000000098'",
    )
    .fetch_one(migrator)
    .await
    .expect("inspect NULL-operation rejection audit");
    assert!(
        null_audit_operation.is_err(),
        "NULL audit operation must be rejected"
    );
    assert_eq!(null_audit_rows, 0);

    let forged = format!(
        r#"{{"content":"FORGED_CREATE_SENTINEL","subjects":["{ALICE_SUBJECT}"],"tenant_id":"FORGED_COMPANY","principal_id":"FORGED_PRINCIPAL","app_id":"FORGED_APP","collection_id":"FORGED_COLLECTION","item_id":"FORGED_ITEM","revision_id":"FORGED_REVISION","user_confirmed":true}}"#,
    );
    let forged_denied =
        post_memory(app, alice_writer, "writer-forged-0001", forged.as_bytes()).await;
    assert_eq!(forged_denied.0, StatusCode::BAD_REQUEST);
    assert_eq!(forged_denied.1["code"], "malformed");
    assert!(!forged_denied.1.to_string().contains("FORGED_"));

    for bearer in [
        None,
        Some("invalid-credential-invalid-credential-invalid-credential-0000000000"),
    ] {
        let denied =
            post_memory_with_auth(app, bearer, Some("invalid-writer-0001"), forged.as_bytes())
                .await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.1["code"], "unauthenticated");
        assert!(!denied.1.to_string().contains("FORGED_"));
    }
    set_writer_validity(migrator, true, false).await;
    let expired = post_memory(app, alice_writer, idempotency_key, body.as_bytes()).await;
    assert_eq!(expired.0, StatusCode::UNAUTHORIZED);
    assert_eq!(expired.1["code"], "unauthenticated");
    assert!(expired.1.get("item_id").is_none());
    set_writer_validity(migrator, false, true).await;
    let revoked = post_memory(app, alice_writer, idempotency_key, body.as_bytes()).await;
    assert_eq!(revoked.0, StatusCode::UNAUTHORIZED);
    assert_eq!(revoked.1["code"], "unauthenticated");
    assert!(revoked.1.get("item_id").is_none());
    set_writer_validity(migrator, false, false).await;
    for (deactivate, restore, id) in [
        (
            "UPDATE principals SET active = false WHERE tenant_id = $1 AND id = $2",
            "UPDATE principals SET active = true WHERE tenant_id = $1 AND id = $2",
            "10000000000000000000000000000001",
        ),
        (
            "UPDATE apps SET active = false WHERE tenant_id = $1 AND id = $2",
            "UPDATE apps SET active = true WHERE tenant_id = $1 AND id = $2",
            "a0000000000000000000000000000001",
        ),
    ] {
        sqlx::query(deactivate)
            .bind(ALPHA_TENANT)
            .bind(id)
            .execute(migrator)
            .await
            .expect("deactivate replay authority");
        let denied = post_memory(app, alice_writer, idempotency_key, body.as_bytes()).await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.1["code"], "unauthenticated");
        assert!(denied.1.get("item_id").is_none());
        sqlx::query(restore)
            .bind(ALPHA_TENANT)
            .bind(id)
            .execute(migrator)
            .await
            .expect("restore replay authority");
    }
    set_writer_validity(migrator, false, true).await;
    assert_eq!(
        put_memory_request(
            app,
            Some(alice_writer),
            Some("correction-revoked-1"),
            "/v1/items/%FF?tenant_id=FORGED_COMPANY",
            Some("application/json"),
            forged.as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    set_writer_validity(migrator, false, false).await;

    let mut held = migrator.begin().await.expect("begin writer authority lock");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id = $1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *held)
        .await
        .expect("hold writer authority lock");
    let blocked_app = app.clone();
    let blocked_writer = alice_writer.to_owned();
    let blocked_body = body.clone();
    let blocked_key = idempotency_key.to_owned();
    let blocked_create = tokio::spawn(async move {
        post_memory(
            &blocked_app,
            &blocked_writer,
            &blocked_key,
            blocked_body.as_bytes(),
        )
        .await
    });
    wait_for_writer_authority_lock_wait(migrator).await;
    set_writer_validity(migrator, false, true).await;
    held.commit().await.expect("release writer authority lock");
    let post_wait_revoked = blocked_create.await.expect("join blocked writer request");
    assert_eq!(post_wait_revoked.0, StatusCode::UNAUTHORIZED);
    assert_eq!(post_wait_revoked.1["code"], "unauthenticated");
    set_writer_validity(migrator, false, false).await;

    for idempotency_key in [None, Some("short")] {
        let denied =
            post_memory_with_auth(app, Some(alice_writer), idempotency_key, body.as_bytes()).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
    }
    for (index, content_type) in [None, Some("text/plain")].into_iter().enumerate() {
        let denied = post_memory_with_headers(
            app,
            Some(alice_writer),
            Some("content-type-check-0001"),
            content_type,
            format!(
                r#"{{"content":"CONTENT_TYPE_SENTINEL_{index}","subjects":["{ALICE_SUBJECT}"]}}"#
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
    }

    let bob_scope_body =
        format!(r#"{{"content":"BOB_SCOPE_SENTINEL","subjects":["{BOB_SUBJECT}"]}}"#);
    let bob_scope_denied = post_memory(
        app,
        alice_writer,
        "bob-scope-denied-01",
        bob_scope_body.as_bytes(),
    )
    .await;
    assert_eq!(bob_scope_denied.0, StatusCode::BAD_REQUEST);
    assert_eq!(bob_scope_denied.1["code"], "malformed");

    sqlx::query(
        "UPDATE collections SET withdrawn_at = clock_timestamp() \
         WHERE tenant_id = $1 AND id = '30000000000000000000000000000004'",
    )
    .bind(ALPHA_TENANT)
    .execute(migrator)
    .await
    .expect("withdraw Alice private collection");
    let withdrawn_body =
        format!(r#"{{"content":"WITHDRAWN_COLLECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let withdrawn = post_memory(
        app,
        alice_writer,
        "withdrawn-create-001",
        withdrawn_body.as_bytes(),
    )
    .await;
    assert_eq!(withdrawn.0, StatusCode::NOT_FOUND);
    assert_eq!(withdrawn.1["code"], "unavailable");
    let withdrawn_replay = post_memory(app, alice_writer, idempotency_key, body.as_bytes()).await;
    assert_eq!(withdrawn_replay.0, StatusCode::NOT_FOUND);
    assert_eq!(withdrawn_replay.1["code"], "unavailable");
    assert!(withdrawn_replay.1.get("item_id").is_none());
    sqlx::query(
        "UPDATE collections SET withdrawn_at = NULL \
         WHERE tenant_id = $1 AND id = '30000000000000000000000000000004'",
    )
    .bind(ALPHA_TENANT)
    .execute(migrator)
    .await
    .expect("restore Alice private collection");

    let nul_body =
        format!(r#"{{"content":"NUL\u0000CONTENT_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let nul_denied = post_memory(
        app,
        alice_writer,
        "nul-content-denied-1",
        nul_body.as_bytes(),
    )
    .await;
    assert_eq!(nul_denied.0, StatusCode::BAD_REQUEST);
    assert_eq!(nul_denied.1["code"], "malformed");

    let duplicate = format!(
        r#"{{"content":"DUPLICATE_A","content":"DUPLICATE_B","subjects":["{ALICE_SUBJECT}"]}}"#,
    );
    let wrong_type = r#"{"content":7,"subjects":"WRONG_TYPE"}"#.to_owned();
    let trailing =
        format!(r#"{{"content":"TRAILING_DATA","subjects":["{ALICE_SUBJECT}"]}} trailing"#);
    let too_many_subjects = serde_json::json!({
        "content": "TOO_MANY_SUBJECTS",
        "subjects": vec![ALICE_SUBJECT; 9]
    })
    .to_string();
    let too_deep = format!(
        "{{\"content\":\"TOO_DEEP\",\"subjects\":{}\"{ALICE_SUBJECT}\"{}}}",
        "[".repeat(17),
        "]".repeat(17)
    );
    let many_members = format!(
        "{{\"content\":\"TOO_MANY_MEMBERS\",\"subjects\":[\"{ALICE_SUBJECT}\"],{}}}",
        (0..255)
            .map(|index| format!("\"field_{index}\":null"))
            .collect::<Vec<_>>()
            .join(",")
    );
    for (index, malformed) in [
        duplicate,
        wrong_type,
        trailing,
        too_many_subjects,
        too_deep,
        many_members,
    ]
    .iter()
    .enumerate()
    {
        let denied = post_memory(
            app,
            alice_writer,
            &format!("malformed-create-{index:04}"),
            malformed.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
    }
    let oversized = vec![b'X'; 64 * 1024 + 1];
    let oversized_denied = post_memory(app, alice_writer, "oversized-create-01", &oversized).await;
    assert_eq!(oversized_denied.0, StatusCode::BAD_REQUEST);
    assert_eq!(oversized_denied.1["code"], "malformed");

    let forged_rows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM revisions
         WHERE content IN ('FORGED_CREATE_SENTINEL', 'DUPLICATE_A', 'DUPLICATE_B',
                           'TRAILING_DATA', 'TOO_MANY_SUBJECTS', 'TOO_DEEP',
                           'TOO_MANY_MEMBERS', 'BOB_SCOPE_SENTINEL',
                           'WITHDRAWN_COLLECTION_SENTINEL',
                           'CONTENT_TYPE_SENTINEL_0', 'CONTENT_TYPE_SENTINEL_1')",
    )
    .fetch_one(migrator)
    .await
    .expect("rejected bodies persisted nothing");
    assert_eq!(forged_rows, 0);

    assert_eq!(
        persistence_counts(migrator).await,
        after_success,
        "every denied create must leave persistence unchanged"
    );
    let before_failure = after_success;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_test_lexical_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic lexical insertion failure'; END $$;
         CREATE TRIGGER fail_test_lexical_insert BEFORE INSERT ON lexical_representations
         FOR EACH ROW EXECUTE FUNCTION fail_test_lexical_insert()",
    )
    .execute(migrator)
    .await
    .expect("install synthetic lexical failure trigger");
    let failed_body =
        format!(r#"{{"content":"LEXICAL_ABORT_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let failed = post_memory(
        app,
        alice_writer,
        "lexical-failure-0001",
        failed_body.as_bytes(),
    )
    .await;
    sqlx::raw_sql(
        "DROP TRIGGER fail_test_lexical_insert ON lexical_representations;
         DROP FUNCTION fail_test_lexical_insert()",
    )
    .execute(migrator)
    .await
    .expect("remove synthetic lexical failure trigger");
    assert_eq!(failed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.1["code"], "storage_unavailable");
    assert!(!failed.1.to_string().contains("lexical"));
    assert_eq!(persistence_counts(migrator).await, before_failure);
    assert_eq!(authority_epoch(migrator).await, initial_epoch + 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM revisions WHERE content = 'LEXICAL_ABORT_SENTINEL'",
        )
        .fetch_one(migrator)
        .await
        .expect("failed content absent"),
        0
    );

    let before_commit_failure = persistence_counts(migrator).await;
    let epoch_before_commit_failure = authority_epoch(migrator).await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_test_create_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic deferred commit failure'; END $$;
         CREATE CONSTRAINT TRIGGER fail_test_create_commit AFTER INSERT ON mutation_audit
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
         EXECUTE FUNCTION fail_test_create_commit()",
    )
    .execute(migrator)
    .await
    .expect("install synthetic deferred commit failure");
    let commit_failure_body =
        format!(r#"{{"content":"COMMIT_ABORT_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let commit_failed = post_memory(
        app,
        alice_writer,
        "commit-failure-0001",
        commit_failure_body.as_bytes(),
    )
    .await;
    sqlx::raw_sql(
        "DROP TRIGGER fail_test_create_commit ON mutation_audit;
         DROP FUNCTION fail_test_create_commit()",
    )
    .execute(migrator)
    .await
    .expect("remove synthetic deferred commit failure");
    assert_eq!(commit_failed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(commit_failed.1["code"], "storage_unavailable");
    assert!(!commit_failed.1.to_string().contains("commit"));
    assert_eq!(persistence_counts(migrator).await, before_commit_failure);
    assert_eq!(authority_epoch(migrator).await, epoch_before_commit_failure);

    wait_for_idle_pool(runtime).await;
    let rejection_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM create_rejection_audit WHERE operation = 'create'",
    )
    .fetch_one(migrator)
    .await
    .expect("count sanitized create rejections");
    assert_eq!(rejection_count, 30);
    for (outcome, expected) in [
        ("malformed", 14_i64),
        ("unauthenticated", 9),
        ("unavailable", 2),
        ("storage_unavailable", 2),
        ("idempotency_conflict", 1),
        ("replayed", 2),
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM create_rejection_audit
                 WHERE operation = 'create' AND outcome = $1",
            )
            .bind(outcome)
            .fetch_one(migrator)
            .await
            .expect("count create rejection outcome"),
            expected,
            "unexpected sanitized rejection count for {outcome}"
        );
    }
    let leaked_rejections = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM create_rejection_audit a
         WHERE row_to_json(a)::text ~
           '(PRIVATE_CREATE|FORGED_|BOB_SCOPE|NUL|LEXICAL|COMMIT|s000000)'",
    )
    .fetch_one(migrator)
    .await
    .expect("inspect rejection audit for payload leakage");
    assert_eq!(leaked_rejections, 0);

    assert_fresh_keyword_search(migrator, item_id, revision_id, alice_reader, alice_writer).await;
    assert_authorized_list(
        app,
        migrator,
        runtime,
        alice_reader,
        bob_reader,
        alice_writer,
    )
    .await;
    assert_expired_create_record(
        app,
        migrator,
        alice_writer,
        "concurrent-create-replay-01",
        concurrent_body.as_bytes(),
    )
    .await;
    assert_trusted_correction(
        app,
        migrator,
        purge_worker,
        alice_reader,
        alice_writer,
        item_id,
        revision_id,
    )
    .await;
    assert_writer_validity(app, migrator, alice_reader, alice_writer).await;
}

#[allow(clippy::too_many_lines)]
async fn assert_writer_validity(
    app: &axum::Router,
    migrator: &PgPool,
    alice_reader: &str,
    alice_writer: &str,
) {
    let content = "KNOWN VALIDITY CREATE SENTINEL";
    let created = post_memory(
        app,
        alice_writer,
        "known-validity-create-01",
        serde_json::json!({
            "content": content,
            "subjects": [ALICE_SUBJECT],
            "valid_from": "2020-01-02T03:04:05Z",
            "valid_until": "2099-06-07T08:09:10.123456Z"
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    let item_id = created.1["item_id"].as_str().expect("known-validity item");
    let read = request(app, alice_reader, item_id).await;
    assert_eq!(read.0, StatusCode::OK);
    assert_eq!(read.1["content"], content);
    assert_eq!(read.1["validity_status"], "known");
    assert_eq!(read.1["valid_from"], "2020-01-02T03:04:05.000000Z");
    assert_eq!(read.1["valid_until"], "2099-06-07T08:09:10.123456Z");
    assert!(read.1["recorded_at"].as_str().is_some_and(|value| {
        value.len() == 27 && value.ends_with('Z') && value.as_bytes().get(10) == Some(&b'T')
    }));

    let same_body = serde_json::json!({
        "content": content,
        "subjects": [ALICE_SUBJECT],
        "valid_from": "2020-01-02T03:04:05Z",
        "valid_until": "2099-06-07T08:09:10.123456Z"
    })
    .to_string();
    let replay = post_memory(
        app,
        alice_writer,
        "known-validity-create-01",
        same_body.as_bytes(),
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["item_id"], item_id);
    assert_eq!(replay.1["replayed"], true);
    let conflict = post_memory(
        app,
        alice_writer,
        "known-validity-create-01",
        serde_json::json!({
            "content": content,
            "subjects": [ALICE_SUBJECT],
            "valid_from": "2020-01-02T03:04:05Z",
            "valid_until": "2099-06-07T08:09:11Z"
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.1["code"], "idempotency_conflict");

    let searched = search_json(app, Some(alice_reader), content).await;
    assert_eq!(searched.0, StatusCode::OK);
    let searched_item = searched.1["items"]
        .as_array()
        .expect("validity search items")
        .iter()
        .find(|candidate| candidate["item_id"] == item_id)
        .expect("known-validity search result");
    assert_eq!(searched_item["validity_status"], "known");
    assert_eq!(searched_item["valid_from"], "2020-01-02T03:04:05.000000Z");
    assert_eq!(searched_item["valid_until"], "2099-06-07T08:09:10.123456Z");
    assert!(searched_item["recorded_at"].as_str().is_some_and(|value| {
        value.len() == 27 && value.ends_with('Z') && value.as_bytes().get(10) == Some(&b'T')
    }));
    let listed = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
    assert_eq!(listed.0, StatusCode::OK);
    let listed_item = listed.1["items"]
        .as_array()
        .expect("validity list items")
        .iter()
        .find(|candidate| candidate["item_id"] == item_id)
        .expect("known-validity list result");
    assert_eq!(listed_item["validity_status"], "known");
    assert_eq!(listed_item["valid_from"], "2020-01-02T03:04:05.000000Z");
    assert_eq!(listed_item["valid_until"], "2099-06-07T08:09:10.123456Z");
    assert!(listed_item["recorded_at"].as_str().is_some());

    let original_revision = created.1["revision_id"]
        .as_str()
        .expect("known-validity original revision");
    let correction_content = "KNOWN VALIDITY CORRECTION SENTINEL";
    let correction_body = serde_json::json!({
        "expected_revision_id": original_revision,
        "content": correction_content,
        "subjects": [ALICE_SUBJECT],
        "valid_from": "2021-02-03T04:05:06.7Z",
        "valid_until": "2098-07-08T09:10:11Z"
    })
    .to_string();
    let corrected = put_memory(
        app,
        alice_writer,
        "known-validity-correct-01",
        item_id,
        correction_body.as_bytes(),
    )
    .await;
    assert_eq!(corrected.0, StatusCode::OK);
    let corrected_revision = corrected.1["revision_id"]
        .as_str()
        .expect("known-validity corrected revision");
    let corrected_read = request_expected(app, alice_reader, item_id, corrected_revision).await;
    assert_eq!(corrected_read.0, StatusCode::OK);
    assert_eq!(corrected_read.1["content"], correction_content);
    assert_eq!(
        corrected_read.1["valid_from"],
        "2021-02-03T04:05:06.700000Z"
    );
    assert_eq!(
        corrected_read.1["valid_until"],
        "2098-07-08T09:10:11.000000Z"
    );
    let corrected_search = search_json(app, Some(alice_reader), correction_content).await;
    assert_eq!(corrected_search.0, StatusCode::OK);
    assert_eq!(corrected_search.1["items"][0]["item_id"], item_id);
    assert_eq!(corrected_search.1["items"][0]["validity_status"], "known");
    assert_eq!(
        corrected_search.1["items"][0]["valid_from"],
        "2021-02-03T04:05:06.700000Z"
    );
    let corrected_list = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
    assert_eq!(corrected_list.0, StatusCode::OK);
    let corrected_list_item = corrected_list.1["items"]
        .as_array()
        .expect("corrected validity list items")
        .iter()
        .find(|candidate| candidate["item_id"] == item_id)
        .expect("corrected validity list result");
    assert_eq!(
        corrected_list_item["valid_until"],
        "2098-07-08T09:10:11.000000Z"
    );
    assert_eq!(corrected_list_item["validity_status"], "known");
    let correction_replay = put_memory(
        app,
        alice_writer,
        "known-validity-correct-01",
        item_id,
        correction_body.as_bytes(),
    )
    .await;
    assert_eq!(correction_replay.0, StatusCode::OK);
    assert_eq!(correction_replay.1["replayed"], true);
    let correction_conflict = put_memory(
        app,
        alice_writer,
        "known-validity-correct-01",
        item_id,
        serde_json::json!({
            "expected_revision_id": original_revision,
            "content": correction_content,
            "subjects": [ALICE_SUBJECT],
            "valid_from": "2021-02-03T04:05:06.7Z",
            "valid_until": "2098-07-08T09:10:12Z"
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(correction_conflict.0, StatusCode::CONFLICT);
    assert_eq!(correction_conflict.1["code"], "idempotency_conflict");

    for (key, marker, validity) in [
        (
            "future-validity-create-01",
            "FUTURE VALIDITY SENTINEL",
            serde_json::json!({"valid_from":"2098-01-01T00:00:00Z"}),
        ),
        (
            "expired-validity-create-01",
            "EXPIRED VALIDITY SENTINEL",
            serde_json::json!({"valid_until":"2001-01-01T00:00:00Z"}),
        ),
    ] {
        let mut body = serde_json::json!({"content":marker,"subjects":[ALICE_SUBJECT]});
        body.as_object_mut()
            .expect("validity body object")
            .extend(validity.as_object().expect("validity fields").clone());
        let created = post_memory(app, alice_writer, key, body.to_string().as_bytes()).await;
        assert_eq!(created.0, StatusCode::CREATED);
        let hidden_item = created.1["item_id"].as_str().expect("time-hidden item");
        assert_eq!(
            request(app, alice_reader, hidden_item).await.0,
            StatusCode::NOT_FOUND
        );
        let hidden_search = search_json(app, Some(alice_reader), marker).await;
        assert_eq!(hidden_search.0, StatusCode::OK);
        assert_eq!(hidden_search.1["items"], serde_json::json!([]));
        let hidden_list = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
        assert_eq!(hidden_list.0, StatusCode::OK);
        assert!(!hidden_list.1.to_string().contains(hidden_item));
    }

    for (key, field, value) in [
        (
            "from-only-validity-01",
            "valid_from",
            "2002-01-01T00:00:00Z",
        ),
        (
            "until-only-validity-01",
            "valid_until",
            "2097-01-01T00:00:00Z",
        ),
    ] {
        let mut body = serde_json::json!({
            "content": format!("ONE SIDED {field} SENTINEL"),
            "subjects": [ALICE_SUBJECT]
        });
        body[field] = Value::String(value.to_owned());
        let created = post_memory(app, alice_writer, key, body.to_string().as_bytes()).await;
        assert_eq!(created.0, StatusCode::CREATED);
        let read = request(
            app,
            alice_reader,
            created.1["item_id"].as_str().expect("one-sided item"),
        )
        .await;
        assert_eq!(read.0, StatusCode::OK);
        assert_eq!(read.1["validity_status"], "known");
        assert_eq!(read.1[field], format!("{}.000000Z", &value[..19]));
        let other_field = if field == "valid_from" {
            "valid_until"
        } else {
            "valid_from"
        };
        assert!(read.1[other_field].is_null());
    }

    let unknown = post_memory(
        app,
        alice_writer,
        "unknown-validity-create-01",
        serde_json::json!({"content":"UNKNOWN VALIDITY SENTINEL","subjects":[ALICE_SUBJECT]})
            .to_string()
            .as_bytes(),
    )
    .await;
    assert_eq!(unknown.0, StatusCode::CREATED);
    let unknown_read = request(
        app,
        alice_reader,
        unknown.1["item_id"]
            .as_str()
            .expect("unknown-validity item"),
    )
    .await;
    assert_eq!(unknown_read.0, StatusCode::OK);
    assert_eq!(unknown_read.1["validity_status"], "unknown");
    assert!(unknown_read.1["valid_from"].is_null());
    assert!(unknown_read.1["valid_until"].is_null());

    let before_invalid = persistence_counts(migrator).await;
    let epoch_before_invalid = authority_epoch(migrator).await;
    let invalid_values = [
        serde_json::json!({"valid_from":null}),
        serde_json::json!({"valid_from":"2020-01-01T00:00:00+00:00"}),
        serde_json::json!({"valid_from":"TIME_FORMAT_SENTINEL"}),
        serde_json::json!({"valid_from":"CREATE_TIME_NUL_SENTINEL\u{0}"}),
        serde_json::json!({"valid_from":"2023-02-29T00:00:00Z"}),
        serde_json::json!({"valid_from":"2020-01-01T24:00:00Z"}),
        serde_json::json!({"valid_from":"2099-12-31T23:59:59.9999999Z"}),
        serde_json::json!({
            "valid_from":"2022-01-01T00:00:00Z",
            "valid_until":"2022-01-01T00:00:00Z"
        }),
    ];
    let mut invalid_request_ids = Vec::new();
    for (index, validity) in invalid_values.iter().enumerate() {
        let key = format!("invalid-validity-{index:02}");
        let mut body = serde_json::json!({
            "content":"INVALID VALIDITY BODY SENTINEL",
            "subjects":[ALICE_SUBJECT]
        });
        body.as_object_mut().expect("invalid body object").extend(
            validity
                .as_object()
                .expect("invalid validity fields")
                .clone(),
        );
        let denied = post_memory(app, alice_writer, &key, body.to_string().as_bytes()).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        assert!(!denied.1.to_string().contains("SENTINEL"));
        invalid_request_ids.push(request_id(&denied.1).to_owned());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM idempotency_records WHERE key_digest=$1",
            )
            .bind(Sha256::digest(key.as_bytes()).as_slice())
            .fetch_one(migrator)
            .await
            .expect("invalid validity leaves no idempotency key"),
            0
        );
    }
    for (index, validity) in [
        serde_json::json!({"valid_until":null}),
        serde_json::json!({"valid_until":"CORRECTION_TIME_SENTINEL"}),
        serde_json::json!({"valid_until":"CORRECTION_TIME_NUL_SENTINEL\u{0}"}),
        serde_json::json!({
            "valid_from":"2090-01-01T00:00:00Z",
            "valid_until":"2080-01-01T00:00:00Z"
        }),
    ]
    .iter()
    .enumerate()
    {
        let key = format!("invalid-correct-validity-{index:02}");
        let mut body = serde_json::json!({
            "expected_revision_id": corrected_revision,
            "content":"INVALID CORRECTION VALIDITY SENTINEL",
            "subjects":[ALICE_SUBJECT]
        });
        body.as_object_mut()
            .expect("invalid correction body object")
            .extend(
                validity
                    .as_object()
                    .expect("invalid validity fields")
                    .clone(),
            );
        let denied = put_memory(
            app,
            alice_writer,
            &key,
            item_id,
            body.to_string().as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        assert!(!denied.1.to_string().contains("SENTINEL"));
        invalid_request_ids.push(request_id(&denied.1).to_owned());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM idempotency_records WHERE key_digest=$1",
            )
            .bind(Sha256::digest(key.as_bytes()).as_slice())
            .fetch_one(migrator)
            .await
            .expect("invalid correction validity leaves no idempotency key"),
            0
        );
    }
    assert_eq!(persistence_counts(migrator).await, before_invalid);
    assert_eq!(authority_epoch(migrator).await, epoch_before_invalid);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM create_rejection_audit
             WHERE request_id=ANY($1) AND operation IN ('create','correct')
               AND outcome='malformed'",
        )
        .bind(invalid_request_ids.as_slice())
        .fetch_one(migrator)
        .await
        .expect("invalid validity rejection audits"),
        i64::try_from(invalid_request_ids.len()).expect("bounded validity rejection count")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM create_rejection_audit a
             WHERE request_id=ANY($1) AND row_to_json(a)::text LIKE ANY(ARRAY[
               '%SENTINEL%', '%2020-01-01T00:00:00+00:00%',
               '%2023-02-29T00:00:00Z%', '%2020-01-01T24:00:00Z%',
               '%2099-12-31T23:59:59.9999999Z%', '%2022-01-01T00:00:00Z%',
               '%2090-01-01T00:00:00Z%', '%2080-01-01T00:00:00Z%'
             ])",
        )
        .bind(invalid_request_ids.as_slice())
        .fetch_one(migrator)
        .await
        .expect("validity rejection audit non-disclosure"),
        0
    );

    let before_internal_fault = persistence_counts(migrator).await;
    let epoch_before_internal_fault = authority_epoch(migrator).await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_test_validity_internal_fault() RETURNS trigger
         LANGUAGE plpgsql AS $$ BEGIN PERFORM 1 / 0; RETURN NEW; END $$;
         CREATE TRIGGER fail_test_validity_internal_fault
         BEFORE INSERT ON revisions FOR EACH ROW
         EXECUTE FUNCTION fail_test_validity_internal_fault()",
    )
    .execute(migrator)
    .await
    .expect("install internal class-22 fault");
    let internal_fault_key = "validity-internal-fault-01";
    let internal_fault = post_memory(
        app,
        alice_writer,
        internal_fault_key,
        serde_json::json!({
            "content":"VALIDITY INTERNAL FAULT SENTINEL",
            "subjects":[ALICE_SUBJECT],
            "valid_from":"2020-01-01T00:00:00Z",
            "valid_until":"2099-01-01T00:00:00Z"
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    sqlx::raw_sql(
        "DROP TRIGGER fail_test_validity_internal_fault ON revisions;
         DROP FUNCTION fail_test_validity_internal_fault()",
    )
    .execute(migrator)
    .await
    .expect("remove internal class-22 fault");
    assert_eq!(internal_fault.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(internal_fault.1["code"], "storage_unavailable");
    assert!(!internal_fault.1.to_string().contains("SENTINEL"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM create_rejection_audit
             WHERE request_id=$1 AND operation='create'
               AND outcome='storage_unavailable'",
        )
        .bind(request_id(&internal_fault.1))
        .fetch_one(migrator)
        .await
        .expect("internal fault sanitized rejection audit"),
        1
    );
    assert_eq!(persistence_counts(migrator).await, before_internal_fault);
    assert_eq!(authority_epoch(migrator).await, epoch_before_internal_fault);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM idempotency_records WHERE key_digest=$1",
        )
        .bind(Sha256::digest(internal_fault_key.as_bytes()).as_slice())
        .fetch_one(migrator)
        .await
        .expect("internal fault rolls back provisional idempotency key"),
        0
    );

    let auth_first = post_memory_with_auth(
        app,
        None,
        Some("auth-first-validity-01"),
        br#"{"content":"AUTH FIRST VALIDITY SENTINEL","subjects":["FORGED_SUBJECT"],"valid_from":null}"#,
    )
    .await;
    assert_eq!(auth_first.0, StatusCode::UNAUTHORIZED);
}

#[allow(clippy::too_many_lines)]
async fn assert_trusted_correction(
    app: &axum::Router,
    migrator: &PgPool,
    purge_worker: &PgPool,
    alice_reader: &str,
    alice_writer: &str,
    item_id: &str,
    expected_revision_id: &str,
) {
    let similar_body =
        format!(r#"{{"content":"SIMILAR_UNTARGETED_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let similar = post_memory(
        app,
        alice_writer,
        "similar-untargeted-01",
        similar_body.as_bytes(),
    )
    .await;
    assert_eq!(similar.0, StatusCode::CREATED);
    let similar_id = similar.1["item_id"].as_str().expect("similar item ID");
    let similar_snapshot = created_private_target_snapshot(migrator, similar_id).await;
    assert_correction_envelope_denials(app, migrator, alice_writer, item_id, expected_revision_id)
        .await;
    let epoch = authority_epoch(migrator).await;
    let bodies = [
        format!(
            r#"{{"expected_revision_id":"{expected_revision_id}","content":"CORRECTION_WINNER_ALPHA_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
        ),
        format!(
            r#"{{"expected_revision_id":"{expected_revision_id}","content":"CORRECTION_WINNER_BRAVO_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
        ),
    ];
    let left_app = app.clone();
    let right_app = app.clone();
    let (left, right) = tokio::join!(
        put_memory(
            &left_app,
            alice_writer,
            "competing-correct-01",
            item_id,
            bodies[0].as_bytes()
        ),
        put_memory(
            &right_app,
            alice_writer,
            "competing-correct-02",
            item_id,
            bodies[1].as_bytes()
        ),
    );
    let (winner, loser, winner_content) = if left.0 == StatusCode::OK {
        (&left, &right, "CORRECTION_WINNER_ALPHA_SENTINEL")
    } else {
        (&right, &left, "CORRECTION_WINNER_BRAVO_SENTINEL")
    };
    assert_eq!(winner.0, StatusCode::OK);
    assert_eq!(winner.1["replayed"], false);
    assert_eq!(loser.0, StatusCode::CONFLICT);
    assert_eq!(loser.1["code"], "stale_context");
    let new_revision = winner.1["revision_id"]
        .as_str()
        .expect("corrected revision");
    assert_ne!(new_revision, expected_revision_id);
    assert_eq!(authority_epoch(migrator).await, epoch + 1);
    let winner_key = if left.0 == StatusCode::OK {
        "competing-correct-01"
    } else {
        "competing-correct-02"
    };
    let correction_completion = stored_completion(migrator, "correct", winner_key).await;
    assert_eq!(winner.1["completed_at"], correction_completion.0);
    assert!(correction_completion.1);
    let atomic = sqlx::query(
        "SELECT i.active_revision_id, r.content,
                EXISTS (SELECT 1 FROM lexical_representations l
                        WHERE l.tenant_id=i.tenant_id AND l.item_id=i.id AND l.revision_id=$3) AS lexical,
                (SELECT count(*) FROM mutation_audit a WHERE a.target_id=i.id
                 AND a.revision_id=$3 AND a.operation='correct' AND a.outcome='corrected') AS audit,
                (SELECT array_agg(rs.subject_id ORDER BY rs.subject_id)
                 FROM revision_subjects rs WHERE rs.tenant_id=i.tenant_id
                   AND rs.item_id=i.id AND rs.revision_id=$3) AS subjects,
                (SELECT content FROM revisions old WHERE old.tenant_id=i.tenant_id
                 AND old.item_id=i.id AND old.id=$4) AS old_content
         FROM items i JOIN revisions r ON r.tenant_id=i.tenant_id AND r.item_id=i.id AND r.id=$3
         WHERE i.tenant_id=$1 AND i.id=$2",
    ).bind(ALPHA_TENANT).bind(item_id).bind(new_revision).bind(expected_revision_id)
      .fetch_one(migrator).await.expect("correction committed atomically");
    assert_eq!(
        atomic
            .try_get::<String, _>("active_revision_id")
            .expect("active"),
        new_revision
    );
    assert_eq!(
        atomic.try_get::<String, _>("content").expect("content"),
        winner_content
    );
    assert!(atomic.try_get::<bool, _>("lexical").expect("lexical"));
    assert_eq!(atomic.try_get::<i64, _>("audit").expect("audit"), 1);
    assert_eq!(
        atomic
            .try_get::<Vec<String>, _>("subjects")
            .expect("subjects"),
        vec![ALICE_SUBJECT.to_owned()]
    );
    assert_eq!(
        atomic
            .try_get::<String, _>("old_content")
            .expect("old content"),
        "PRIVATE_CREATE_ATOMIC_SENTINEL"
    );
    assert_eq!(
        created_private_target_snapshot(migrator, similar_id).await,
        similar_snapshot
    );

    let replay_body = format!(
        r#"{{"expected_revision_id":"{new_revision}","content":"CONCURRENT_CORRECTION_REPLAY_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let before_replay_correction = persistence_counts(migrator).await;
    let replay_epoch = authority_epoch(migrator).await;
    let mut concurrent_gate = migrator
        .begin()
        .await
        .expect("begin correction replay gate");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *concurrent_gate)
        .await
        .expect("hold correction replay tenant gate");
    let left_app = app.clone();
    let right_app = app.clone();
    let left_writer = alice_writer.to_owned();
    let right_writer = alice_writer.to_owned();
    let left_item = item_id.to_owned();
    let right_item = item_id.to_owned();
    let left_body = replay_body.clone();
    let right_body = replay_body.clone();
    let left = tokio::spawn(async move {
        put_memory(
            &left_app,
            &left_writer,
            "concurrent-correct-replay-01",
            &left_item,
            left_body.as_bytes(),
        )
        .await
    });
    let right = tokio::spawn(async move {
        put_memory(
            &right_app,
            &right_writer,
            "concurrent-correct-replay-01",
            &right_item,
            right_body.as_bytes(),
        )
        .await
    });
    wait_for_write_waiters(migrator, "correct_private_memory", 2).await;
    concurrent_gate
        .commit()
        .await
        .expect("release correction replay gate");
    let left = left.await.expect("join left correction replay");
    let right = right.await.expect("join right correction replay");
    assert_eq!(left.0, StatusCode::OK);
    assert_eq!(right.0, StatusCode::OK);
    assert_ne!(left.1["replayed"], right.1["replayed"]);
    for response in [&left.1, &right.1] {
        if response["replayed"] == true {
            assert!(response.get("lexical_status").is_none());
            assert!(response.get("semantic_status").is_none());
        }
    }
    for field in ["item_id", "revision_id", "operation_id", "completed_at"] {
        assert_eq!(left.1[field], right.1[field]);
    }
    let replay_revision = left.1["revision_id"]
        .as_str()
        .expect("idempotent correction revision");
    let after_replay_correction = persistence_counts(migrator).await;
    assert_eq!(
        after_replay_correction,
        (
            before_replay_correction.0,
            before_replay_correction.1 + 1,
            before_replay_correction.2 + 1,
            before_replay_correction.3 + 1,
            before_replay_correction.4 + 1,
            before_replay_correction.5 + 1,
        )
    );
    assert_eq!(authority_epoch(migrator).await, replay_epoch + 1);
    let expired_while_waiting = replay_while_writer_expires_on_item(
        app,
        migrator,
        alice_writer,
        "correct",
        "concurrent-correct-replay-01",
        item_id,
        replay_body.as_bytes(),
    )
    .await;
    assert_eq!(expired_while_waiting.0, StatusCode::UNAUTHORIZED);
    assert_eq!(expired_while_waiting.1["code"], "unauthenticated");
    assert!(expired_while_waiting.1.get("item_id").is_none());
    assert_eq!(persistence_counts(migrator).await, after_replay_correction);
    assert_eq!(authority_epoch(migrator).await, replay_epoch + 1);

    let later_body = format!(
        r#"{{"expected_revision_id":"{replay_revision}","content":"LATER_CORRECTION_CURRENT_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let mut read_after_correction_gate = migrator
        .begin()
        .await
        .expect("begin correction-before-read tenant gate");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *read_after_correction_gate)
        .await
        .expect("hold correction-before-read tenant gate");
    let correcting_app = app.clone();
    let correcting_writer = alice_writer.to_owned();
    let correcting_item = item_id.to_owned();
    let correcting_body = later_body.clone();
    let correcting = tokio::spawn(async move {
        put_memory(
            &correcting_app,
            &correcting_writer,
            "create-atomic-0001",
            &correcting_item,
            correcting_body.as_bytes(),
        )
        .await
    });
    wait_for_correction_authority_lock_wait(migrator).await;
    let stale_app = app.clone();
    let stale_reader = alice_reader.to_owned();
    let stale_item = item_id.to_owned();
    let stale_expected = replay_revision.to_owned();
    let ordered_stale_read = tokio::spawn(async move {
        request_expected(&stale_app, &stale_reader, &stale_item, &stale_expected).await
    });
    wait_for_authority_lock_wait(migrator).await;
    read_after_correction_gate
        .commit()
        .await
        .expect("release correction-before-read tenant gate");
    let later = correcting.await.expect("join ordered correction");
    let ordered_stale_read = ordered_stale_read.await.expect("join ordered stale read");
    assert_eq!(later.0, StatusCode::OK);
    assert_eq!(later.1["replayed"], false);
    let current_revision = later.1["revision_id"]
        .as_str()
        .expect("later current revision");
    assert_eq!(ordered_stale_read.0, StatusCode::CONFLICT);
    assert_eq!(ordered_stale_read.1["code"], "stale_context");
    for forbidden in [
        replay_revision,
        current_revision,
        "CONCURRENT_CORRECTION_REPLAY_SENTINEL",
        "LATER_CORRECTION_CURRENT_SENTINEL",
    ] {
        assert!(!ordered_stale_read.1.to_string().contains(forbidden));
    }
    assert_read_rejection_audit(migrator, request_id(&ordered_stale_read.1), "stale_context").await;
    let before_historical_replay = persistence_counts(migrator).await;
    let epoch_before_historical_replay = authority_epoch(migrator).await;
    let historical_replay = put_memory(
        app,
        alice_writer,
        "concurrent-correct-replay-01",
        item_id,
        replay_body.as_bytes(),
    )
    .await;
    assert_eq!(historical_replay.0, StatusCode::OK);
    assert_eq!(historical_replay.1["replayed"], true);
    assert!(historical_replay.1.get("lexical_status").is_none());
    assert!(historical_replay.1.get("semantic_status").is_none());
    assert_eq!(historical_replay.1["revision_id"], replay_revision);
    assert_eq!(historical_replay.1["operation_id"], left.1["operation_id"]);
    assert_eq!(historical_replay.1["completed_at"], left.1["completed_at"]);
    assert_eq!(
        write_request_audit_count(
            migrator,
            &historical_replay.1,
            "concurrent-correct-replay-01",
            "replayed",
            epoch_before_historical_replay,
        )
        .await,
        1
    );
    assert_eq!(persistence_counts(migrator).await, before_historical_replay);
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_historical_replay
    );
    let conflict_body = format!(
        r#"{{"expected_revision_id":"{new_revision}","content":"CORRECTION_REPLAY_CHANGED_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let conflict = put_memory(
        app,
        alice_writer,
        "concurrent-correct-replay-01",
        item_id,
        conflict_body.as_bytes(),
    )
    .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(conflict.1["code"], "idempotency_conflict");
    assert_eq!(
        write_request_audit_count(
            migrator,
            &conflict.1,
            "concurrent-correct-replay-01",
            "idempotency_conflict",
            epoch_before_historical_replay,
        )
        .await,
        1
    );
    assert_eq!(persistence_counts(migrator).await, before_historical_replay);
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_historical_replay
    );
    let before_receipt_denials = persistence_counts(migrator).await;
    set_writer_validity(migrator, false, true).await;
    for candidate in [&replay_body, &conflict_body] {
        let denied = put_memory(
            app,
            alice_writer,
            "concurrent-correct-replay-01",
            item_id,
            candidate.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.1["code"], "unauthenticated");
        assert!(denied.1.get("item_id").is_none());
    }
    set_writer_validity(migrator, false, false).await;
    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp()
         WHERE tenant_id=$1 AND id='30000000000000000000000000000004'",
    )
    .bind(ALPHA_TENANT)
    .execute(migrator)
    .await
    .expect("withdraw correction receipt collection");
    let withdrawn_read = request_expected(app, alice_reader, item_id, current_revision).await;
    assert_sanitized_unavailable(&withdrawn_read);
    for candidate in [&replay_body, &conflict_body] {
        let denied = put_memory(
            app,
            alice_writer,
            "concurrent-correct-replay-01",
            item_id,
            candidate.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::NOT_FOUND);
        assert_eq!(denied.1["code"], "unavailable");
        assert!(denied.1.get("item_id").is_none());
    }
    sqlx::query(
        "UPDATE collections SET withdrawn_at=NULL
         WHERE tenant_id=$1 AND id='30000000000000000000000000000004'",
    )
    .bind(ALPHA_TENANT)
    .execute(migrator)
    .await
    .expect("restore correction receipt collection");
    assert_eq!(persistence_counts(migrator).await, before_receipt_denials);
    assert_eq!(
        authority_epoch(migrator).await,
        epoch_before_historical_replay
    );
    sqlx::query(
        "UPDATE idempotency_records SET expires_at=clock_timestamp()-interval '1 second'
         WHERE tenant_id=$1 AND operation='correct' AND key_digest=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(Sha256::digest(b"concurrent-correct-replay-01").as_slice())
    .execute(migrator)
    .await
    .expect("expire completed correction key");
    let before_expired_correction = persistence_counts(migrator).await;
    let expired_correction_epoch = authority_epoch(migrator).await;
    let expired_correction = put_memory(
        app,
        alice_writer,
        "concurrent-correct-replay-01",
        item_id,
        replay_body.as_bytes(),
    )
    .await;
    assert_eq!(expired_correction.0, StatusCode::CONFLICT);
    assert_eq!(expired_correction.1["code"], "stale_context");
    assert!(expired_correction.1.get("replayed").is_none());
    assert_eq!(
        active_correction_key_count(migrator, "concurrent-correct-replay-01").await,
        0
    );
    assert_eq!(
        persistence_counts(migrator).await,
        before_expired_correction
    );
    assert_eq!(authority_epoch(migrator).await, expired_correction_epoch);

    let current = request_expected(app, alice_reader, item_id, current_revision).await;
    assert_eq!(current.0, StatusCode::OK);
    assert_eq!(current.1["revision_id"], current_revision);
    assert_eq!(current.1["content"], "LATER_CORRECTION_CURRENT_SENTINEL");
    for (scoped_item, expectation) in [
        (
            "40000000000000000000000000000099",
            "50000000000000000000000000000099",
        ),
        (
            "40000000000000000000000000000097",
            "00000000000000000000000000000000",
        ),
    ] {
        let inaccessible = request_expected(app, alice_reader, scoped_item, expectation).await;
        assert_sanitized_unavailable(&inaccessible);
    }
    let old_search = search_json(app, Some(alice_reader), "PRIVATE_CREATE_ATOMIC_SENTINEL").await;
    assert_eq!(old_search.0, StatusCode::OK);
    assert!(old_search.1["items"].as_array().is_some_and(|items| {
        items
            .iter()
            .all(|item| item["item_id"].as_str() != Some(item_id))
    }));
    let superseded_search = search_json(app, Some(alice_reader), winner_content).await;
    assert_eq!(superseded_search.0, StatusCode::OK);
    assert!(
        superseded_search.1["items"]
            .as_array()
            .is_some_and(|items| {
                items
                    .iter()
                    .all(|item| item["item_id"].as_str() != Some(item_id))
            })
    );
    let current_search = search_json(app, Some(alice_reader), "LATER CORRECTION CURRENT").await;
    assert!(current_search.1.to_string().contains(current_revision));
    let listed = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
    let listed_text = listed.1.to_string();
    assert!(listed_text.contains(current_revision));
    assert!(!listed_text.contains(expected_revision_id));
    assert!(!listed_text.contains(replay_revision));

    let stale_body = format!(
        r#"{{"expected_revision_id":"{expected_revision_id}","content":"STALE_CORRECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let before_denials = created_private_target_snapshot(migrator, item_id).await;
    assert_eq!(
        put_memory(
            app,
            alice_writer,
            "stale-correction-01",
            item_id,
            stale_body.as_bytes()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        correction_key_count(migrator, "stale-correction-01").await,
        0
    );
    for (index, target) in [
        MISSING_ITEM,
        BOB_PRIVATE_ITEM,
        ALLOWED_ITEM,
        "40000000000000000000000000000097",
    ]
    .into_iter()
    .enumerate()
    {
        let body = format!(
            r#"{{"expected_revision_id":"{current_revision}","content":"DENIED_CORRECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
        );
        let key = format!("denied-correct-{index:02}");
        let denied = put_memory(app, alice_writer, &key, target, body.as_bytes()).await;
        assert_eq!(denied.0, StatusCode::NOT_FOUND);
        assert_eq!(denied.1["code"], "unavailable");
        assert_eq!(correction_key_count(migrator, &key).await, 0);
    }
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        before_denials
    );
    assert_correction_failures_and_recheck(app, migrator, alice_writer, item_id, current_revision)
        .await;
    assert_trusted_forget(
        app,
        migrator,
        purge_worker,
        alice_reader,
        alice_writer,
        item_id,
        current_revision,
        similar_id,
        &similar_snapshot,
    )
    .await;
    let forgotten_create_body =
        format!(r#"{{"content":"PRIVATE_CREATE_ATOMIC_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    set_writer_validity(migrator, false, true).await;
    for denied in [
        post_memory(
            app,
            alice_writer,
            "create-atomic-0001",
            forgotten_create_body.as_bytes(),
        )
        .await,
        put_memory(
            app,
            alice_writer,
            "create-atomic-0001",
            item_id,
            later_body.as_bytes(),
        )
        .await,
    ] {
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert!(denied.1.get("operation_id").is_none());
    }
    set_writer_validity(migrator, false, false).await;
    let forgotten_create = post_memory(
        app,
        alice_writer,
        "create-atomic-0001",
        forgotten_create_body.as_bytes(),
    )
    .await;
    assert_minimal_forgotten_receipt(&forgotten_create.1, "create");
    let forgotten_correction = put_memory(
        app,
        alice_writer,
        "create-atomic-0001",
        item_id,
        later_body.as_bytes(),
    )
    .await;
    assert_minimal_forgotten_receipt(&forgotten_correction.1, "correct");
    let after_replay_read = request_expected(app, alice_reader, item_id, current_revision).await;
    assert_sanitized_unavailable(&after_replay_read);
    let after_replay_search =
        search_json(app, Some(alice_reader), "LATER CORRECTION CURRENT").await;
    assert_eq!(after_replay_search.0, StatusCode::OK);
    assert!(
        after_replay_search.1["items"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["item_id"] != item_id))
    );
    let after_replay_list = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
    assert_eq!(after_replay_list.0, StatusCode::OK);
    assert!(
        after_replay_list.1["items"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["item_id"] != item_id))
    );
    assert_forget_concurrency(app, migrator, alice_writer).await;
    for outcome in [
        "malformed",
        "unauthenticated",
        "unavailable",
        "stale_context",
        "idempotency_conflict",
        "replayed",
        "storage_unavailable",
    ] {
        assert!(sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM create_rejection_audit WHERE operation='correct' AND outcome=$1"
        ).bind(outcome).fetch_one(migrator).await.expect("count correction rejection outcome") > 0);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn assert_trusted_forget(
    app: &axum::Router,
    migrator: &PgPool,
    purge_worker: &PgPool,
    alice_reader: &str,
    alice_writer: &str,
    item_id: &str,
    revision_id: &str,
    similar_id: &str,
    similar_snapshot: &str,
) {
    let body = format!(r#"{{"expected_revision_id":"{revision_id}"}}"#);
    let before = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    let target_before = created_private_target_snapshot(migrator, item_id).await;
    for bearer in [
        None,
        Some("invalid-forget-credential-value-0000000000000000"),
    ] {
        let denied = delete_memory_request(
            app,
            bearer,
            Some("forget-auth-denied-01"),
            &format!("/v1/items/{item_id}"),
            Some("application/json"),
            br#"{"expected_revision_id":"FORGED_FORGET_REVISION","principal_id":"FORGED_FORGET_PRINCIPAL"}"#,
        )
        .await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.1["code"], "unauthenticated");
        assert!(!denied.1.to_string().contains("FORGED_FORGET"));
    }
    let mut oversized = format!(r#"{{"expected_revision_id":"{revision_id}"}}"#).into_bytes();
    oversized.resize(64 * 1024 + 1, b' ');
    let malformed_bodies = [
        br"{}".to_vec(),
        br#"{"expected_revision_id":3}"#.to_vec(),
        format!(r#"{{"expected_revision_id":"{revision_id}","tenant_id":"FORGED_FORGET_TENANT"}}"#)
            .into_bytes(),
        format!(
            r#"{{"expected_revision_id":"{revision_id}","expected_revision_id":"{revision_id}"}}"#
        )
        .into_bytes(),
        format!(r#"{{"expected_revision_id":"{revision_id}"}} trailing"#).into_bytes(),
        oversized,
    ];
    for malformed in &malformed_bodies {
        let denied =
            delete_memory(app, alice_writer, "forget-malformed-01", item_id, malformed).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        assert!(!denied.1.to_string().contains("FORGED_FORGET"));
        assert_forget_request_audit(migrator, &denied.1, None, "malformed", epoch).await;
    }
    for denied in [
        delete_memory_request(
            app,
            Some(alice_writer),
            None,
            &format!("/v1/items/{item_id}"),
            Some("application/json"),
            body.as_bytes(),
        )
        .await,
        delete_memory_request(
            app,
            Some(alice_writer),
            Some("forget-malformed-02"),
            &format!("/v1/items/{item_id}"),
            None,
            body.as_bytes(),
        )
        .await,
    ] {
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
    }
    set_writer_validity(migrator, true, false).await;
    assert_eq!(
        delete_memory(
            app,
            alice_writer,
            "forget-expired-001",
            item_id,
            body.as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    set_writer_validity(migrator, false, true).await;
    assert_eq!(
        delete_memory(
            app,
            alice_writer,
            "forget-revoked-001",
            item_id,
            body.as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    set_writer_validity(migrator, false, false).await;
    sqlx::query(
        "UPDATE credentials SET allowed_operations=ARRAY['create','correct']::text[]
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .execute(migrator)
    .await
    .expect("remove forget operation from writer");
    assert_eq!(
        delete_memory(
            app,
            alice_writer,
            "forget-operation-denied",
            item_id,
            body.as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query(
        "UPDATE credentials SET allowed_operations=ARRAY['create','correct','forget']::text[]
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .execute(migrator)
    .await
    .expect("restore writer operations");
    for target in [
        BOB_PRIVATE_ITEM,
        ALLOWED_ITEM,
        "40000000000000000000000000000097",
    ] {
        let denied = delete_memory(
            app,
            alice_writer,
            "forget-target-deny-01",
            target,
            body.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::NOT_FOUND);
        assert_eq!(denied.1["code"], "unavailable");
        assert_forget_request_audit(
            migrator,
            &denied.1,
            Some("forget-target-deny-01"),
            "unavailable",
            epoch,
        )
        .await;
    }
    assert_eq!(persistence_counts(migrator).await, before);
    assert_eq!(authority_epoch(migrator).await, epoch);
    let stale = delete_memory(
        app,
        alice_writer,
        "forget-stale-0001",
        item_id,
        br#"{"expected_revision_id":"00000000000000000000000000000000"}"#,
    )
    .await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["code"], "stale_context");
    assert_forget_request_audit(
        migrator,
        &stale.1,
        Some("forget-stale-0001"),
        "stale_context",
        epoch,
    )
    .await;
    let missing = delete_memory(
        app,
        alice_writer,
        "forget-missing-01",
        MISSING_ITEM,
        body.as_bytes(),
    )
    .await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_forget_request_audit(
        migrator,
        &missing.1,
        Some("forget-missing-01"),
        "unavailable",
        epoch,
    )
    .await;
    for (table, key) in [
        ("deletion_markers", "forget-marker-fail-01"),
        ("purge_jobs", "forget-job-failure-01"),
        ("mutation_audit", "forget-audit-fail-01"),
    ] {
        assert_forget_insert_failure(
            app,
            migrator,
            alice_writer,
            item_id,
            body.as_bytes(),
            table,
            key,
        )
        .await;
    }
    assert_forget_commit_failure(app, migrator, alice_writer, item_id, body.as_bytes()).await;
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        target_before
    );
    assert_eq!(persistence_counts(migrator).await, before);
    assert_eq!(authority_epoch(migrator).await, epoch);
    let forgotten = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(forgotten.0, StatusCode::OK);
    assert_eq!(forgotten.1["status"], "pending");
    assert_eq!(forgotten.1["operation"], "forget");
    assert_eq!(forgotten.1["replayed"], false);
    for forbidden in [
        "item_id",
        "revision_id",
        "content",
        "lexical_status",
        "semantic_status",
    ] {
        assert!(forgotten.1.get(forbidden).is_none());
    }
    let operation_id = forgotten.1["operation_id"]
        .as_str()
        .expect("forget operation ID");
    let committed = sqlx::query(
        "SELECT i.active_revision_id IS NULL AS inactive, i.deleted_at IS NOT NULL AS deleted,
                i.deletion_generation, m.purge_state, j.status,
                (SELECT count(*) FROM mutation_audit a
                 WHERE a.operation='forget' AND a.operation_id=$3
                   AND a.target_id=$2 AND a.revision_id=$4
                   AND a.outcome='forgotten') AS audits,
                (SELECT count(*) FROM idempotency_records d
                 WHERE d.operation='forget' AND d.operation_id=$3
                   AND d.request_digest=$5) AS keys
         FROM items i JOIN deletion_markers m
           ON m.tenant_id=i.tenant_id AND m.item_id=i.id
         JOIN purge_jobs j ON j.tenant_id=i.tenant_id AND j.item_id=i.id
           AND j.deletion_generation=i.deletion_generation
         WHERE i.tenant_id=$1 AND i.id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .bind(operation_id)
    .bind(revision_id)
    .bind(canonical_forget_digest(item_id, body.as_bytes()).as_slice())
    .fetch_one(migrator)
    .await
    .expect("forget commits marker, job, audit and key atomically");
    assert!(committed.try_get::<bool, _>("inactive").expect("inactive"));
    assert!(committed.try_get::<bool, _>("deleted").expect("deleted"));
    assert_eq!(
        committed
            .try_get::<i64, _>("deletion_generation")
            .expect("generation"),
        1
    );
    assert_eq!(
        committed
            .try_get::<String, _>("purge_state")
            .expect("marker state"),
        "pending"
    );
    assert_eq!(
        committed.try_get::<String, _>("status").expect("job state"),
        "pending"
    );
    assert_eq!(
        committed.try_get::<i64, _>("audits").expect("forget audit"),
        1
    );
    assert_eq!(committed.try_get::<i64, _>("keys").expect("forget key"), 1);
    assert_eq!(authority_epoch(migrator).await, epoch + 1);
    let forgotten_read = request_expected(app, alice_reader, item_id, revision_id).await;
    assert_sanitized_unavailable(&forgotten_read);
    let search = search_json(app, Some(alice_reader), "LATER CORRECTION CURRENT").await;
    assert_eq!(search.0, StatusCode::OK);
    assert!(search.1["items"].as_array().is_some_and(Vec::is_empty));
    let listed = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
    assert_eq!(listed.0, StatusCode::OK);
    let listed_items = listed.1["items"].as_array().expect("list items array");
    assert!(listed_items.iter().all(|item| item["item_id"] != item_id));
    let after_forget = persistence_counts(migrator).await;
    let replay = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["replayed"], true);
    assert!(replay.1.get("status").is_none());
    assert_eq!(replay.1["operation_id"], operation_id);
    assert_eq!(replay.1["completed_at"], forgotten.1["completed_at"]);
    assert_minimal_forgotten_receipt(&replay.1, "forget");
    assert_eq!(persistence_counts(migrator).await, after_forget);
    assert_eq!(authority_epoch(migrator).await, epoch + 1);
    set_writer_validity(migrator, false, true).await;
    let revoked_receipt = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(revoked_receipt.0, StatusCode::UNAUTHORIZED);
    assert!(revoked_receipt.1.get("operation_id").is_none());
    set_writer_validity(migrator, false, false).await;
    set_writer_validity(migrator, true, false).await;
    let expired_receipt = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(expired_receipt.0, StatusCode::UNAUTHORIZED);
    assert!(expired_receipt.1.get("operation_id").is_none());
    set_writer_validity(migrator, false, false).await;
    for (table, id) in [
        ("principals", "10000000000000000000000000000001"),
        ("apps", "a0000000000000000000000000000001"),
    ] {
        let deactivate = match table {
            "principals" => "UPDATE principals SET active=false WHERE tenant_id=$1 AND id=$2",
            "apps" => "UPDATE apps SET active=false WHERE tenant_id=$1 AND id=$2",
            _ => unreachable!("fixed authority table"),
        };
        let restore = match table {
            "principals" => "UPDATE principals SET active=true WHERE tenant_id=$1 AND id=$2",
            "apps" => "UPDATE apps SET active=true WHERE tenant_id=$1 AND id=$2",
            _ => unreachable!("fixed authority table"),
        };
        sqlx::query(deactivate)
            .bind(ALPHA_TENANT)
            .bind(id)
            .execute(migrator)
            .await
            .expect("deactivate forget receipt authority");
        let denied = delete_memory(
            app,
            alice_writer,
            "forget-private-01",
            item_id,
            body.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert!(denied.1.get("operation_id").is_none());
        sqlx::query(restore)
            .bind(ALPHA_TENANT)
            .bind(id)
            .execute(migrator)
            .await
            .expect("restore forget receipt authority");
    }
    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp()
         WHERE tenant_id=$1 AND id=(SELECT collection_id FROM items WHERE tenant_id=$1 AND id=$2)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .execute(migrator)
    .await
    .expect("withdraw forgotten receipt collection");
    let withdrawn_receipt = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(withdrawn_receipt.0, StatusCode::NOT_FOUND);
    assert!(withdrawn_receipt.1.get("operation_id").is_none());
    sqlx::query(
        "UPDATE collections SET withdrawn_at=NULL
         WHERE tenant_id=$1 AND id=(SELECT collection_id FROM items WHERE tenant_id=$1 AND id=$2)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .execute(migrator)
    .await
    .expect("restore forgotten receipt collection");
    let changed = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        br#"{"expected_revision_id":"00000000000000000000000000000000"}"#,
    )
    .await;
    assert_eq!(changed.0, StatusCode::CONFLICT);
    assert_eq!(changed.1["code"], "idempotency_conflict");
    assert_forget_request_audit(
        migrator,
        &changed.1,
        Some("forget-private-01"),
        "idempotency_conflict",
        epoch + 1,
    )
    .await;
    let retargeted = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        MISSING_ITEM,
        body.as_bytes(),
    )
    .await;
    assert_eq!(retargeted.0, StatusCode::CONFLICT);
    assert_eq!(retargeted.1["code"], "idempotency_conflict");
    assert_forget_request_audit(
        migrator,
        &retargeted.1,
        Some("forget-private-01"),
        "idempotency_conflict",
        epoch + 1,
    )
    .await;

    set_writer_validity(migrator, true, true).await;
    for statement in [
        "UPDATE principals SET active=false WHERE tenant_id=$1 AND id='10000000000000000000000000000001'",
        "UPDATE apps SET active=false WHERE tenant_id=$1 AND id='a0000000000000000000000000000001'",
    ] {
        sqlx::query(statement)
            .bind(ALPHA_TENANT)
            .execute(migrator)
            .await
            .expect("remove user receipt authority before independent purge");
    }
    sqlx::query(
        "UPDATE collections SET withdrawn_at=clock_timestamp()
         WHERE tenant_id=$1 AND id=(SELECT collection_id FROM items WHERE tenant_id=$1 AND id=$2)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .execute(migrator)
    .await
    .expect("withdraw collection before independent purge");

    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_forget_purge() RETURNS trigger
         LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''forced purge failure''; END'",
    )
    .execute(migrator)
    .await
    .expect("install purge failure function");
    sqlx::query(
        "CREATE TRIGGER fail_forget_purge BEFORE DELETE ON lexical_representations
         FOR EACH ROW EXECUTE FUNCTION fail_forget_purge()",
    )
    .execute(migrator)
    .await
    .expect("install purge failure trigger");
    let purge_before_failure = forget_state_snapshot(migrator, item_id).await;
    let mut failed_purge = purge_worker
        .begin()
        .await
        .expect("begin failed constrained purge");
    let purge_failure = sqlx::query("SELECT process_forget_purge($1,$2,$3)")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(1_i64)
        .execute(&mut *failed_purge)
        .await;
    assert!(purge_failure.is_err());
    failed_purge
        .rollback()
        .await
        .expect("rollback failed purge");
    sqlx::query("DROP TRIGGER fail_forget_purge ON lexical_representations")
        .execute(migrator)
        .await
        .expect("drop purge failure trigger");
    sqlx::query("DROP FUNCTION fail_forget_purge()")
        .execute(migrator)
        .await
        .expect("drop purge failure function");
    assert_eq!(
        forget_state_snapshot(migrator, item_id).await,
        purge_before_failure
    );

    let stale_purge_before = forget_state_snapshot(migrator, item_id).await;
    let mut stale_purge = purge_worker.begin().await.expect("begin stale purge");
    let stale_error = sqlx::query("SELECT process_forget_purge($1,$2,$3)")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(2_i64)
        .execute(&mut *stale_purge)
        .await
        .expect_err("well-formed mismatched generation must not purge");
    assert_eq!(
        stale_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("P0003")
    );
    stale_purge.rollback().await.expect("rollback stale purge");
    assert_eq!(
        forget_state_snapshot(migrator, item_id).await,
        stale_purge_before
    );

    let mut purge = purge_worker.begin().await.expect("begin constrained purge");
    let purge_state = sqlx::query_scalar::<_, String>("SELECT process_forget_purge($1,$2,$3)")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .bind(1_i64)
        .fetch_one(&mut *purge)
        .await
        .expect("process generation-bound purge");
    assert_eq!(purge_state, "complete");
    purge.commit().await.expect("commit constrained purge");
    set_writer_validity(migrator, false, false).await;
    for statement in [
        "UPDATE principals SET active=true WHERE tenant_id=$1 AND id='10000000000000000000000000000001'",
        "UPDATE apps SET active=true WHERE tenant_id=$1 AND id='a0000000000000000000000000000001'",
    ] {
        sqlx::query(statement)
            .bind(ALPHA_TENANT)
            .execute(migrator)
            .await
            .expect("restore user authority after worker-only purge proof");
    }
    sqlx::query(
        "UPDATE collections SET withdrawn_at=NULL
         WHERE tenant_id=$1 AND id=(SELECT collection_id FROM items WHERE tenant_id=$1 AND id=$2)",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .execute(migrator)
    .await
    .expect("restore collection after worker-only purge proof");
    let purged = sqlx::query(
        "SELECT (SELECT count(*) FROM revisions WHERE tenant_id=$1 AND item_id=$2) AS revisions,
                (SELECT count(*) FROM revision_subjects WHERE tenant_id=$1 AND item_id=$2) AS subjects,
                (SELECT count(*) FROM lexical_representations WHERE tenant_id=$1 AND item_id=$2) AS lexical,
                (SELECT purge_state FROM deletion_markers WHERE tenant_id=$1 AND item_id=$2) AS marker,
                (SELECT status FROM purge_jobs WHERE tenant_id=$1 AND item_id=$2) AS job",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .fetch_one(migrator)
    .await
    .expect("inspect completed physical purge");
    for column in ["revisions", "subjects", "lexical"] {
        assert_eq!(purged.try_get::<i64, _>(column).expect("purged rows"), 0);
    }
    assert_eq!(
        purged.try_get::<String, _>("marker").expect("marker state"),
        "complete"
    );
    assert_eq!(
        purged.try_get::<String, _>("job").expect("job state"),
        "complete"
    );
    let marker_json = sqlx::query_scalar::<_, String>(
        "SELECT row_to_json(m)::text FROM deletion_markers m
         WHERE tenant_id=$1 AND item_id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .fetch_one(migrator)
    .await
    .expect("read minimal deletion marker");
    for forbidden in ["LATER CORRECTION", "s000000", "content", "embedding"] {
        assert!(!marker_json.contains(forbidden));
    }
    let mut repeated_purge = purge_worker.begin().await.expect("begin repeated purge");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT process_forget_purge($1,$2,$3)")
            .bind(ALPHA_TENANT)
            .bind(item_id)
            .bind(1_i64)
            .fetch_one(&mut *repeated_purge)
            .await
            .expect("repeat completed purge"),
        "complete"
    );
    repeated_purge
        .commit()
        .await
        .expect("commit repeated purge");
    let completed_replay = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(completed_replay.0, StatusCode::OK);
    assert!(completed_replay.1.get("status").is_none());
    assert_eq!(completed_replay.1["replayed"], true);
    assert_eq!(completed_replay.1["operation_id"], operation_id);
    sqlx::query(
        "UPDATE idempotency_records SET expires_at=clock_timestamp()-interval '1 second'
         WHERE tenant_id=$1 AND operation='forget' AND key_digest=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(Sha256::digest(b"forget-private-01").as_slice())
    .execute(migrator)
    .await
    .expect("expire completed forget key");
    let before_expired_retry = persistence_counts(migrator).await;
    let expired_epoch = authority_epoch(migrator).await;
    let expired_retry = delete_memory(
        app,
        alice_writer,
        "forget-private-01",
        item_id,
        body.as_bytes(),
    )
    .await;
    assert_eq!(expired_retry.0, StatusCode::NOT_FOUND);
    assert_eq!(expired_retry.1["code"], "unavailable");
    assert!(expired_retry.1.get("replayed").is_none());
    assert_eq!(persistence_counts(migrator).await, before_expired_retry);
    assert_eq!(authority_epoch(migrator).await, expired_epoch);
    assert_eq!(
        created_private_target_snapshot(migrator, similar_id).await,
        similar_snapshot
    );
    for outcome in [
        "malformed",
        "unauthenticated",
        "unavailable",
        "stale_context",
        "storage_unavailable",
        "idempotency_conflict",
        "replayed",
    ] {
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM create_rejection_audit
                 WHERE operation='forget' AND outcome=$1",
            )
            .bind(outcome)
            .fetch_one(migrator)
            .await
            .expect("count required forget request audit outcome")
                > 0,
            "missing sanitized {outcome} forget request audit",
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM create_rejection_audit a
             WHERE a.operation='forget' AND row_to_json(a)::text ~ '(SENTINEL|FORGED_)'",
        )
        .fetch_one(migrator)
        .await
        .expect("forget rejection audits contain no request payload"),
        0
    );
    assert!(before.0 <= persistence_counts(migrator).await.0);
}

#[allow(clippy::too_many_arguments)]
async fn forget_state_snapshot(pool: &PgPool, item_id: &str) -> String {
    sqlx::query_scalar(
        "SELECT json_build_object(
           'active_revision_id', i.active_revision_id,
           'deleted_at', i.deleted_at,
           'deletion_generation', i.deletion_generation,
           'marker', (SELECT row_to_json(m) FROM deletion_markers m
                      WHERE m.tenant_id=i.tenant_id AND m.item_id=i.id),
           'job', (SELECT row_to_json(j) FROM purge_jobs j
                   WHERE j.tenant_id=i.tenant_id AND j.item_id=i.id),
           'revisions', (SELECT count(*) FROM revisions r
                         WHERE r.tenant_id=i.tenant_id AND r.item_id=i.id),
           'subjects', (SELECT count(*) FROM revision_subjects rs
                        WHERE rs.tenant_id=i.tenant_id AND rs.item_id=i.id),
           'lexical', (SELECT count(*) FROM lexical_representations l
                       WHERE l.tenant_id=i.tenant_id AND l.item_id=i.id)
         )::text FROM items i WHERE i.tenant_id=$1 AND i.id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("snapshot exact forget state")
}

async fn assert_forget_insert_failure(
    app: &axum::Router,
    migrator: &PgPool,
    writer: &str,
    item_id: &str,
    body: &[u8],
    table: &str,
    key: &str,
) {
    let snapshot = forget_state_snapshot(migrator, item_id).await;
    let counts = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_forget_insert() RETURNS trigger
         LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''forced forget insert failure''; END'",
    )
    .execute(migrator)
    .await
    .expect("install forget insert failure function");
    let (create_trigger, drop_trigger) = match table {
        "deletion_markers" => (
            "CREATE TRIGGER fail_forget_insert BEFORE INSERT ON deletion_markers
             FOR EACH ROW EXECUTE FUNCTION fail_forget_insert()",
            "DROP TRIGGER fail_forget_insert ON deletion_markers",
        ),
        "purge_jobs" => (
            "CREATE TRIGGER fail_forget_insert BEFORE INSERT ON purge_jobs
             FOR EACH ROW EXECUTE FUNCTION fail_forget_insert()",
            "DROP TRIGGER fail_forget_insert ON purge_jobs",
        ),
        "mutation_audit" => (
            "CREATE TRIGGER fail_forget_insert BEFORE INSERT ON mutation_audit
             FOR EACH ROW EXECUTE FUNCTION fail_forget_insert()",
            "DROP TRIGGER fail_forget_insert ON mutation_audit",
        ),
        _ => panic!("fixed forget failure table"),
    };
    sqlx::query(create_trigger)
        .execute(migrator)
        .await
        .expect("install forget insert failure trigger");
    let failed = delete_memory(app, writer, key, item_id, body).await;
    assert_eq!(failed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.1["code"], "storage_unavailable");
    sqlx::query(drop_trigger)
        .execute(migrator)
        .await
        .expect("drop forget insert failure trigger");
    sqlx::query("DROP FUNCTION fail_forget_insert()")
        .execute(migrator)
        .await
        .expect("drop forget insert failure function");
    assert_eq!(forget_state_snapshot(migrator, item_id).await, snapshot);
    assert_eq!(persistence_counts(migrator).await, counts);
    assert_eq!(authority_epoch(migrator).await, epoch);
}

async fn assert_forget_commit_failure(
    app: &axum::Router,
    migrator: &PgPool,
    writer: &str,
    item_id: &str,
    body: &[u8],
) {
    let snapshot = forget_state_snapshot(migrator, item_id).await;
    let counts = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_forget_commit() RETURNS trigger
         LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''forced forget commit failure''; END'",
    )
    .execute(migrator)
    .await
    .expect("install forget commit failure function");
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER fail_forget_commit AFTER UPDATE ON items
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_forget_commit()",
    )
    .execute(migrator)
    .await
    .expect("install deferred forget commit failure");
    let failed = delete_memory(app, writer, "forget-commit-fail-01", item_id, body).await;
    assert_eq!(failed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.1["code"], "storage_unavailable");
    sqlx::query("DROP TRIGGER fail_forget_commit ON items")
        .execute(migrator)
        .await
        .expect("drop deferred forget failure trigger");
    sqlx::query("DROP FUNCTION fail_forget_commit()")
        .execute(migrator)
        .await
        .expect("drop forget commit failure function");
    assert_eq!(forget_state_snapshot(migrator, item_id).await, snapshot);
    assert_eq!(persistence_counts(migrator).await, counts);
    assert_eq!(authority_epoch(migrator).await, epoch);
}

fn assert_minimal_forgotten_receipt(receipt: &Value, operation: &str) {
    assert_eq!(receipt["replayed"], true);
    assert_eq!(receipt["operation"], operation);
    let mut keys = receipt
        .as_object()
        .expect("minimal forgotten receipt object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["completed_at", "operation", "operation_id", "replayed"]
    );
}

#[allow(clippy::too_many_lines)]
async fn assert_forget_concurrency(app: &axum::Router, migrator: &PgPool, writer: &str) {
    let create_body =
        format!(r#"{{"content":"CONCURRENT_FORGET_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#);
    let created = post_memory(
        app,
        writer,
        "forget-concurrent-create",
        create_body.as_bytes(),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    let item_id = created.1["item_id"]
        .as_str()
        .expect("concurrent forget item");
    let revision_id = created.1["revision_id"]
        .as_str()
        .expect("concurrent forget revision");
    let body = format!(r#"{{"expected_revision_id":"{revision_id}"}}"#);
    let before = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    let mut gate = migrator
        .begin()
        .await
        .expect("begin concurrent forget gate");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *gate)
        .await
        .expect("hold concurrent forget tenant gate");
    let left_app = app.clone();
    let right_app = app.clone();
    let left_writer = writer.to_owned();
    let right_writer = writer.to_owned();
    let left_item = item_id.to_owned();
    let right_item = item_id.to_owned();
    let left_body = body.clone();
    let right_body = body.clone();
    let left = tokio::spawn(async move {
        delete_memory(
            &left_app,
            &left_writer,
            "forget-concurrent-01",
            &left_item,
            left_body.as_bytes(),
        )
        .await
    });
    let right = tokio::spawn(async move {
        delete_memory(
            &right_app,
            &right_writer,
            "forget-concurrent-01",
            &right_item,
            right_body.as_bytes(),
        )
        .await
    });
    wait_for_write_waiters(migrator, "forget_private_memory", 2).await;
    gate.commit().await.expect("release concurrent forget gate");
    let left = left.await.expect("join left forget");
    let right = right.await.expect("join right forget");
    assert_eq!(left.0, StatusCode::OK);
    assert_eq!(right.0, StatusCode::OK);
    assert_ne!(left.1["replayed"], right.1["replayed"]);
    assert_eq!(left.1["operation_id"], right.1["operation_id"]);
    assert_eq!(left.1["completed_at"], right.1["completed_at"]);
    assert_eq!(
        persistence_counts(migrator).await,
        (
            before.0,
            before.1,
            before.2,
            before.3,
            before.4 + 1,
            before.5 + 1
        )
    );
    assert_eq!(authority_epoch(migrator).await, epoch + 1);

    let race_create = post_memory(
        app,
        writer,
        "forget-correct-race-create",
        format!(r#"{{"content":"FORGET_CORRECT_RACE_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#)
            .as_bytes(),
    )
    .await;
    let race_item = race_create.1["item_id"].as_str().expect("race item");
    let race_revision = race_create.1["revision_id"]
        .as_str()
        .expect("race revision");
    let forget_body = format!(r#"{{"expected_revision_id":"{race_revision}"}}"#);
    let correct_body = format!(
        r#"{{"expected_revision_id":"{race_revision}","content":"OBSOLETE_CORRECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let mut race_gate = migrator.begin().await.expect("begin forget-correct gate");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *race_gate)
        .await
        .expect("hold forget-correct tenant gate");
    let forget_app = app.clone();
    let forget_writer = writer.to_owned();
    let forget_item = race_item.to_owned();
    let forget = tokio::spawn(async move {
        delete_memory(
            &forget_app,
            &forget_writer,
            "forget-correct-race-01",
            &forget_item,
            forget_body.as_bytes(),
        )
        .await
    });
    wait_for_write_waiters(migrator, "forget_private_memory", 1).await;
    let correct_app = app.clone();
    let correct_writer = writer.to_owned();
    let correct_item = race_item.to_owned();
    let correction = tokio::spawn(async move {
        put_memory(
            &correct_app,
            &correct_writer,
            "forget-correct-race-02",
            &correct_item,
            correct_body.as_bytes(),
        )
        .await
    });
    wait_for_write_waiters(migrator, "correct_private_memory", 1).await;
    race_gate
        .commit()
        .await
        .expect("release forget-correct gate");
    assert_eq!(forget.await.expect("join racing forget").0, StatusCode::OK);
    let correction = correction.await.expect("join obsolete correction");
    assert_eq!(correction.0, StatusCode::NOT_FOUND);
    assert_eq!(correction.1["code"], "unavailable");
    assert_eq!(
        correction_key_count(migrator, "forget-correct-race-02").await,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM revisions WHERE content=$1")
            .bind("OBSOLETE_CORRECTION_SENTINEL")
            .fetch_one(migrator)
            .await
            .expect("obsolete correction never committed"),
        0
    );

    for (suffix, expired, revoked) in [("expiry", true, false), ("revocation", false, true)] {
        let guarded = post_memory(
            app,
            writer,
            &format!("forget-reauth-create-{suffix}"),
            format!(
                r#"{{"content":"FORGET_REAUTH_{suffix}_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
            )
            .as_bytes(),
        )
        .await;
        let guarded_item = guarded.1["item_id"].as_str().expect("reauth item");
        let guarded_revision = guarded.1["revision_id"].as_str().expect("reauth revision");
        assert_forget_post_item_reauth(
            app,
            migrator,
            writer,
            suffix,
            guarded_item,
            guarded_revision,
            expired,
            revoked,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn assert_forget_post_item_reauth(
    app: &axum::Router,
    migrator: &PgPool,
    writer: &str,
    suffix: &str,
    item_id: &str,
    revision_id: &str,
    expired: bool,
    revoked: bool,
) {
    let before = created_private_target_snapshot(migrator, item_id).await;
    let counts = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    let mut gate = migrator.begin().await.expect("begin forget item gate");
    sqlx::query("SELECT id FROM items WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .execute(&mut *gate)
        .await
        .expect("hold exact forget item gate");
    set_writer_validity(migrator, false, false).await;
    let blocked_app = app.clone();
    let blocked_writer = writer.to_owned();
    let blocked_item = item_id.to_owned();
    let blocked_body = format!(r#"{{"expected_revision_id":"{revision_id}"}}"#);
    let key = format!("forget-post-item-{suffix}");
    let blocked = tokio::spawn(async move {
        delete_memory(
            &blocked_app,
            &blocked_writer,
            &key,
            &blocked_item,
            blocked_body.as_bytes(),
        )
        .await
    });
    wait_for_write_waiters(migrator, "forget_private_memory", 1).await;
    set_writer_validity(migrator, expired, revoked).await;
    gate.commit().await.expect("release forget item gate");
    let denied = blocked.await.expect("join post-item forget denial");
    assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
    assert_eq!(denied.1["code"], "unauthenticated");
    set_writer_validity(migrator, false, false).await;
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        before
    );
    assert_eq!(persistence_counts(migrator).await, counts);
    assert_eq!(authority_epoch(migrator).await, epoch);
}

async fn correction_key_count(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM idempotency_records
         WHERE operation='correct' AND key_digest=$1",
    )
    .bind(Sha256::digest(key.as_bytes()).as_slice())
    .fetch_one(pool)
    .await
    .expect("count correction idempotency key")
}

async fn active_correction_key_count(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM idempotency_records
         WHERE operation='correct' AND key_digest=$1 AND expires_at > clock_timestamp()",
    )
    .bind(Sha256::digest(key.as_bytes()).as_slice())
    .fetch_one(pool)
    .await
    .expect("count active correction idempotency key")
}

async fn assert_expired_create_record(
    app: &axum::Router,
    migrator: &PgPool,
    writer: &str,
    key: &str,
    body: &[u8],
) {
    let key_digest = Sha256::digest(key.as_bytes());
    let prior_item = sqlx::query_scalar::<_, String>(
        "SELECT item_id FROM idempotency_records
         WHERE tenant_id=$1 AND operation='create' AND key_digest=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(key_digest.as_slice())
    .fetch_one(migrator)
    .await
    .expect("read completed create key before expiry");
    sqlx::query(
        "UPDATE idempotency_records SET expires_at=clock_timestamp()-interval '1 second'
         WHERE tenant_id=$1 AND operation='create' AND key_digest=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(key_digest.as_slice())
    .execute(migrator)
    .await
    .expect("expire completed create key");
    let before = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    let fresh = post_memory(app, writer, key, body).await;
    assert_eq!(fresh.0, StatusCode::CREATED);
    assert_eq!(fresh.1["replayed"], false);
    assert_ne!(fresh.1["item_id"], prior_item);
    assert_eq!(
        persistence_counts(migrator).await,
        (
            before.0 + 1,
            before.1 + 1,
            before.2 + 1,
            before.3 + 1,
            before.4,
            before.5 + 1,
        )
    );
    assert_eq!(authority_epoch(migrator).await, epoch + 1);
    let after_fresh = persistence_counts(migrator).await;
    let replay = post_memory(app, writer, key, body).await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["replayed"], true);
    for field in ["item_id", "revision_id", "operation_id", "completed_at"] {
        assert_eq!(replay.1[field], fresh.1[field]);
    }
    assert_eq!(persistence_counts(migrator).await, after_fresh);
    assert_eq!(authority_epoch(migrator).await, epoch + 1);
}

async fn write_request_audit_count(
    pool: &PgPool,
    response: &Value,
    key: &str,
    outcome: &str,
    epoch: i64,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM create_rejection_audit
         WHERE request_id=$1 AND outcome=$2 AND authority_epoch=$3
           AND idempotency_key_digest=$4",
    )
    .bind(request_id(response))
    .bind(outcome)
    .bind(epoch)
    .bind(Sha256::digest(key.as_bytes()).as_slice())
    .fetch_one(pool)
    .await
    .expect("count sanitized write request audit")
}

async fn assert_forget_request_audit(
    pool: &PgPool,
    response: &Value,
    key: Option<&str>,
    outcome: &str,
    epoch: i64,
) {
    let row = sqlx::query(
        "SELECT count(*) AS matching,
                count(*) FILTER (WHERE tenant_id=$5
                  AND principal_id='10000000000000000000000000000001'
                  AND app_id='a0000000000000000000000000000001'
                  AND credential_id=$6
                  AND row_to_json(create_rejection_audit)::text !~ '(SENTINEL|FORGED_)') AS sanitized
         FROM create_rejection_audit
         WHERE request_id=$1 AND operation='forget' AND outcome=$2
           AND authority_epoch=$3 AND idempotency_key_digest IS NOT DISTINCT FROM $4",
    )
    .bind(request_id(response))
    .bind(outcome)
    .bind(epoch)
    .bind(key.map(|key| Sha256::digest(key.as_bytes()).to_vec()))
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .fetch_one(pool)
    .await
    .expect("read request-specific sanitized forget audit");
    assert_eq!(
        row.try_get::<i64, _>("matching").expect("matching audit"),
        1
    );
    assert_eq!(
        row.try_get::<i64, _>("sanitized").expect("sanitized audit"),
        1
    );
}

async fn stored_completion(pool: &PgPool, operation: &str, key: &str) -> (String, bool) {
    sqlx::query_as(
        "SELECT to_char(completed_at AT TIME ZONE 'UTC',
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),
                expires_at > completed_at
                  AND expires_at <= completed_at + interval '24 hours'
         FROM idempotency_records
         WHERE tenant_id=$1 AND operation=$2 AND key_digest=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(operation)
    .bind(Sha256::digest(key.as_bytes()).as_slice())
    .fetch_one(pool)
    .await
    .expect("read stable idempotency completion")
}

#[allow(clippy::too_many_lines)]
async fn assert_correction_envelope_denials(
    app: &axum::Router,
    migrator: &PgPool,
    alice_writer: &str,
    item_id: &str,
    revision_id: &str,
) {
    let snapshot = created_private_target_snapshot(migrator, item_id).await;
    let counts = persistence_counts(migrator).await;
    let epoch = authority_epoch(migrator).await;
    let forged = format!(
        r#"{{"expected_revision_id":"{revision_id}","content":"FORGED_CORRECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"],"tenant_id":"FORGED_COMPANY"}}"#
    );
    for bearer in [
        None,
        Some("invalid-correction-credential-invalid-correction-credential"),
    ] {
        let denied = put_memory_request(
            app,
            bearer,
            Some("correction-auth-deny"),
            "/v1/items/%FF?tenant_id=FORGED_COMPANY",
            Some("application/json"),
            forged.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert!(!denied.1.to_string().contains("FORGED_"));
    }
    set_writer_validity(migrator, true, false).await;
    assert_eq!(
        put_memory_request(
            app,
            Some(alice_writer),
            Some("correction-expired-1"),
            "/v1/items/%FF?tenant_id=FORGED_COMPANY",
            Some("application/json"),
            forged.as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    set_writer_validity(migrator, false, false).await;

    let restricted = random_bearer();
    sqlx::query("INSERT INTO credentials (tenant_id,id,principal_id,app_id,token_digest,credential_class,allowed_operations,issued_at,expires_at)
                 VALUES ($1,'c0000000000000000000000000000088','10000000000000000000000000000001','a0000000000000000000000000000001',$2,'trusted_writer',ARRAY['create'],clock_timestamp(),clock_timestamp()+interval '1 hour')")
        .bind(ALPHA_TENANT).bind(Sha256::digest(restricted.as_bytes()).as_slice())
        .execute(migrator).await.expect("insert create-only writer");
    assert_eq!(
        put_memory(
            app,
            &restricted,
            "restricted-correct-1",
            item_id,
            forged.as_bytes()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    let oversized_content = "X".repeat(32 * 1024 + 1);
    let too_many = serde_json::json!({"expected_revision_id": revision_id, "content":"TOO_MANY_CORRECTION_SUBJECTS_SENTINEL", "subjects": vec![ALICE_SUBJECT; 9]}).to_string();
    let cases = [
        "{".to_owned(),
        format!(
            r#"{{"expected_revision_id":"{revision_id}","expected_revision_id":"{revision_id}","content":"DUPLICATE_CORRECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
        ),
        forged,
        format!(r#"{{"content":"MISSING_EXPECTED_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#),
        format!(
            r#"{{"expected_revision_id":7,"content":"WRONG_TYPE_CORRECTION_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
        ),
        format!(
            r#"{{"expected_revision_id":"{revision_id}","content":"{oversized_content}","subjects":["{ALICE_SUBJECT}"]}}"#
        ),
        too_many,
        format!(
            r#"{{"expected_revision_id":"{revision_id}","content":"BOB_CORRECTION_SUBJECT_SENTINEL","subjects":["{BOB_SUBJECT}"]}}"#
        ),
        format!(
            r#"{{"expected_revision_id":"{revision_id}","content":"CROSS_APP_CORRECTION_SUBJECT_SENTINEL","subjects":["s0000000000000000000000000000098"]}}"#
        ),
    ];
    for (index, body) in cases.iter().enumerate() {
        let denied = put_memory(
            app,
            alice_writer,
            &format!("malformed-correct-{index:02}"),
            item_id,
            body.as_bytes(),
        )
        .await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert!(!denied.1.to_string().contains("SENTINEL"));
    }
    let valid_body = format!(
        r#"{{"expected_revision_id":"{revision_id}","content":"ENVELOPE_REQUIRED_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    for (key, content_type) in [
        (None, Some("application/json")),
        (Some("missing-content-type"), None),
    ] {
        assert_eq!(
            put_memory_request(
                app,
                Some(alice_writer),
                key,
                &format!("/v1/items/{item_id}"),
                content_type,
                valid_body.as_bytes()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let too_long_path = format!("/v1/items/{}", "x".repeat(65));
    assert_eq!(
        put_memory_request(
            app,
            Some(alice_writer),
            Some("long-path-correct-1"),
            &too_long_path,
            Some("application/json"),
            valid_body.as_bytes()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut oversized_body = valid_body.as_bytes().to_vec();
    oversized_body.resize(64 * 1024 + 1, b' ');
    assert_eq!(
        put_memory_request(
            app,
            Some(alice_writer),
            Some("large-body-correct"),
            &format!("/v1/items/{item_id}"),
            Some("application/json"),
            &oversized_body
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        snapshot
    );
    assert_eq!(persistence_counts(migrator).await, counts);
    assert_eq!(authority_epoch(migrator).await, epoch);
    let leaked = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM create_rejection_audit a WHERE operation='correct' AND row_to_json(a)::text ~ '(SENTINEL|FORGED_)'")
        .fetch_one(migrator).await.expect("correction rejection audits are sanitized");
    assert_eq!(leaked, 0);
}

#[allow(clippy::too_many_lines)]
async fn assert_correction_failures_and_recheck(
    app: &axum::Router,
    migrator: &PgPool,
    alice_writer: &str,
    item_id: &str,
    current_revision: &str,
) {
    let body = format!(
        r#"{{"expected_revision_id":"{current_revision}","content":"CORRECTION_FAILURE_SENTINEL","subjects":["{ALICE_SUBJECT}"]}}"#
    );
    let snapshot = created_private_target_snapshot(migrator, item_id).await;
    let epoch = authority_epoch(migrator).await;
    let persistence = persistence_counts(migrator).await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_test_correct_lexical() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic correction lexical failure'; END $$;
         CREATE TRIGGER fail_test_correct_lexical BEFORE INSERT ON lexical_representations
         FOR EACH ROW EXECUTE FUNCTION fail_test_correct_lexical()",
    )
    .execute(migrator)
    .await
    .expect("install correction lexical failure");
    let denied = put_memory(
        app,
        alice_writer,
        "correct-lexical-fail",
        item_id,
        body.as_bytes(),
    )
    .await;
    sqlx::raw_sql("DROP TRIGGER fail_test_correct_lexical ON lexical_representations; DROP FUNCTION fail_test_correct_lexical()")
        .execute(migrator).await.expect("remove correction lexical failure");
    assert_eq!(denied.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        snapshot
    );
    assert_eq!(authority_epoch(migrator).await, epoch);
    assert_eq!(persistence_counts(migrator).await, persistence);

    sqlx::raw_sql(
        "CREATE FUNCTION fail_test_correct_commit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'synthetic correction commit failure'; END $$;
         CREATE CONSTRAINT TRIGGER fail_test_correct_commit AFTER INSERT ON mutation_audit
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_test_correct_commit()",
    )
    .execute(migrator)
    .await
    .expect("install correction commit failure");
    let denied = put_memory(
        app,
        alice_writer,
        "correct-commit-fail",
        item_id,
        body.as_bytes(),
    )
    .await;
    sqlx::raw_sql("DROP TRIGGER fail_test_correct_commit ON mutation_audit; DROP FUNCTION fail_test_correct_commit()")
        .execute(migrator).await.expect("remove correction commit failure");
    assert_eq!(denied.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        snapshot
    );
    assert_eq!(authority_epoch(migrator).await, epoch);
    assert_eq!(persistence_counts(migrator).await, persistence);

    let mut held = migrator
        .begin()
        .await
        .expect("begin correction authority lock");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *held)
        .await
        .expect("hold correction authority lock");
    let blocked_app = app.clone();
    let blocked_writer = alice_writer.to_owned();
    let blocked_item = item_id.to_owned();
    let blocked_body = body.clone();
    let blocked = tokio::spawn(async move {
        put_memory(
            &blocked_app,
            &blocked_writer,
            "correct-postwait-revoke",
            &blocked_item,
            blocked_body.as_bytes(),
        )
        .await
    });
    wait_for_correction_authority_lock_wait(migrator).await;
    set_writer_validity(migrator, false, true).await;
    held.commit()
        .await
        .expect("release correction authority lock");
    let denied = blocked.await.expect("join blocked correction");
    assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
    set_writer_validity(migrator, false, false).await;
    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        snapshot
    );
    assert_eq!(authority_epoch(migrator).await, epoch);
    assert_eq!(persistence_counts(migrator).await, persistence);
}

async fn assert_agent_write_denial_matrix(
    app: &axum::Router,
    migrator: &PgPool,
    runtime: &PgPool,
    alice_reader: &str,
    item_id: &str,
    revision_id: &str,
) {
    let expected_persistence = persistence_counts(migrator).await;
    let expected_epoch = authority_epoch(migrator).await;
    let expected_target = created_private_target_snapshot(migrator, item_id).await;
    assert_agent_write_authority_constraints(migrator, runtime, alice_reader).await;

    let rejection_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM create_rejection_audit WHERE credential_id = $1",
    )
    .bind(ALICE_CREDENTIAL)
    .fetch_one(migrator)
    .await
    .expect("count prior agent create rejections");
    let create_body = format!(
        r#"{{"content":"AGENT_CREATE_SENTINEL","subjects":["{ALICE_SUBJECT}"],"collection_id":"30000000000000000000000000000004","item_id":"{item_id}"}}"#,
    );
    let create_denied = post_memory(
        app,
        alice_reader,
        "agent-create-denied-01",
        create_body.as_bytes(),
    )
    .await;
    assert_eq!(create_denied.0, StatusCode::UNAUTHORIZED);
    assert_eq!(create_denied.1["code"], "unauthenticated");
    assert!(!create_denied.1.to_string().contains("AGENT_CREATE"));
    let create_request_id = request_id(&create_denied.1);

    let correction_body = format!(
        r#"{{"content":"AGENT_CORRECT_SENTINEL","expected_revision_id":"{revision_id}","subjects":["{ALICE_SUBJECT}"],"principal_id":"FORGED_AGENT_PRINCIPAL"}}"#,
    );
    let correction_denied = put_memory(
        app,
        alice_reader,
        "agent-correct-denied-1",
        item_id,
        correction_body.as_bytes(),
    )
    .await;
    assert_eq!(correction_denied.0, StatusCode::UNAUTHORIZED);
    assert_eq!(correction_denied.1["code"], "unauthenticated");
    assert!(!correction_denied.1.to_string().contains("AGENT_CORRECT"));

    let forget_body = format!(
        r#"{{"expected_revision_id":"{revision_id}","principal_id":"FORGED_AGENT_PRINCIPAL","marker":"AGENT_FORGET_SENTINEL"}}"#,
    );
    let (status, response) = delete_memory(
        app,
        alice_reader,
        "agent-write-denied-01",
        item_id,
        forget_body.as_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(response["code"], "unauthenticated");

    assert_eq!(
        created_private_target_snapshot(migrator, item_id).await,
        expected_target,
    );
    assert_eq!(persistence_counts(migrator).await, expected_persistence);
    assert_eq!(authority_epoch(migrator).await, expected_epoch);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM create_rejection_audit
             WHERE credential_id = $1 AND outcome = 'unauthenticated'",
        )
        .bind(ALICE_CREDENTIAL)
        .fetch_one(migrator)
        .await
        .expect("count sanitized agent create rejection"),
        rejection_count + 3,
    );
    let sanitized_audit = sqlx::query_scalar::<_, String>(
        "SELECT row_to_json(a)::text FROM create_rejection_audit a
         WHERE request_id = $1",
    )
    .bind(create_request_id)
    .fetch_one(migrator)
    .await
    .expect("read sanitized agent create rejection");
    for forbidden in ["AGENT_CREATE", "30000000000000000000000000000004", item_id] {
        assert!(!sanitized_audit.contains(forbidden));
    }
}

async fn assert_agent_write_authority_constraints(
    migrator: &PgPool,
    runtime: &PgPool,
    alice_reader: &str,
) {
    for operation in ["create", "correct", "forget"] {
        let configured = sqlx::query(
            "UPDATE credentials SET allowed_operations = ARRAY[$3]
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(ALPHA_TENANT)
        .bind(ALICE_CREDENTIAL)
        .bind(operation)
        .execute(migrator)
        .await;
        assert_eq!(
            configured
                .expect_err("agent credential must reject write authority")
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("23514"),
        );
    }

    let mut transaction = runtime
        .begin()
        .await
        .expect("begin agent authority resolution check");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(alice_reader.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set agent credential digest");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resolve_current_writer('create')")
            .fetch_one(&mut *transaction)
            .await
            .expect("agent cannot resolve as trusted writer"),
        0,
    );
    for operation in ["create", "correct", "forget"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM resolve_current_reader($1)")
                .bind(operation)
                .fetch_one(&mut *transaction)
                .await
                .expect("agent reader resolver rejects write operation"),
            0,
        );
    }
    transaction
        .rollback()
        .await
        .expect("rollback agent authority resolution check");
}

#[allow(clippy::too_many_lines)]
async fn assert_authorized_list(
    app: &axum::Router,
    migrator: &PgPool,
    runtime: &PgPool,
    alice_reader: &str,
    bob_reader: &str,
    alice_writer: &str,
) {
    assert_operation_confusion(migrator, runtime, alice_reader).await;
    sqlx::raw_sql(
        "INSERT INTO collections
           (tenant_id, id, app_id, audience_kind, owner_principal_id, withdrawn_at)
         VALUES ('00000000000000000000000000000001',
                 '30000000000000000000000000000095',
                 'a0000000000000000000000000000001', 'restricted', NULL,
                 clock_timestamp());
         INSERT INTO collection_grants (tenant_id, collection_id, principal_id, can_read)
         VALUES ('00000000000000000000000000000001',
                 '30000000000000000000000000000095',
                 '10000000000000000000000000000001', true);
         INSERT INTO items (tenant_id, id, collection_id, deleted_at) VALUES
           ('00000000000000000000000000000001', '40000000000000000000000000000095',
            '30000000000000000000000000000095', NULL),
           ('00000000000000000000000000000001', '40000000000000000000000000000094',
            '30000000000000000000000000000001', clock_timestamp()),
           ('00000000000000000000000000000001', '40000000000000000000000000000093',
            '30000000000000000000000000000001', NULL),
           ('00000000000000000000000000000001', '40000000000000000000000000000092',
            '30000000000000000000000000000001', NULL);
         INSERT INTO revisions
           (tenant_id, item_id, id, content, valid_from, valid_until) VALUES
           ('00000000000000000000000000000001', '40000000000000000000000000000095',
            '50000000000000000000000000000095', 'LIST_WITHDRAWN_CONTENT', NULL, NULL),
           ('00000000000000000000000000000001', '40000000000000000000000000000094',
            '50000000000000000000000000000094', 'LIST_DELETED_CONTENT', NULL, NULL),
           ('00000000000000000000000000000001', '40000000000000000000000000000093',
            '50000000000000000000000000000093', 'LIST_EXPIRED_CONTENT', NULL,
            clock_timestamp() - interval '1 hour'),
           ('00000000000000000000000000000001', '40000000000000000000000000000092',
            '50000000000000000000000000000092', 'LIST_FUTURE_CONTENT',
            clock_timestamp() + interval '1 hour', NULL);
         UPDATE items SET active_revision_id = '5' || substring(id FROM 2)
         WHERE tenant_id = '00000000000000000000000000000001'
           AND id IN ('40000000000000000000000000000095',
                      '40000000000000000000000000000094',
                      '40000000000000000000000000000093',
                      '40000000000000000000000000000092');
         INSERT INTO items (tenant_id, id, collection_id)
         SELECT '00000000000000000000000000000001',
                '6' || lpad(n::text, 31, '0'),
                '30000000000000000000000000000001'
         FROM generate_series(1, 25) AS n;
         INSERT INTO revisions (tenant_id, item_id, id, content)
         SELECT '00000000000000000000000000000001',
                '6' || lpad(n::text, 31, '0'),
                '9' || lpad(n::text, 31, '0'), 'LIST_PAGE_CONTENT'
         FROM generate_series(1, 25) AS n;
         UPDATE items SET active_revision_id = '9' || substring(id FROM 2)
         WHERE tenant_id = '00000000000000000000000000000001'
           AND collection_id = '30000000000000000000000000000001'
           AND id LIKE '6%'",
    )
    .execute(migrator)
    .await
    .expect("seed list lifecycle and pagination oracles");

    let collections = get_json(app, Some(alice_reader), "/v1/collections?limit=1").await;
    assert_eq!(collections.0, StatusCode::OK);
    assert_eq!(collections.1["status"], "ready");
    assert_eq!(collections.1["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        collections.1["items"][0]["collection_id"],
        "30000000000000000000000000000001"
    );
    assert_eq!(collections.1["items"][0]["audience_kind"], "restricted");
    assert_eq!(collections.1["truncated"], true);
    assert!(collections.1.get("total").is_none());
    let first_cursor = collections.1["next_cursor"]
        .as_str()
        .expect("opaque collection cursor")
        .to_owned();
    assert_eq!(first_cursor.len(), 32);
    let replacement = get_json(app, Some(alice_reader), "/v1/collections?limit=1").await;
    assert_eq!(replacement.0, StatusCode::OK);
    let replacement_cursor = replacement.1["next_cursor"]
        .as_str()
        .expect("replacement collection cursor")
        .to_owned();
    assert_ne!(first_cursor, replacement_cursor);
    assert_eq!(list_cursor_count(migrator, "collections").await, 1);
    assert_eq!(
        get_json(
            app,
            Some(alice_reader),
            &format!("/v1/collections?cursor={first_cursor}"),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let left_app = app.clone();
    let right_app = app.clone();
    let (left, right) = tokio::join!(
        get_json(&left_app, Some(alice_reader), "/v1/collections?limit=1"),
        get_json(&right_app, Some(alice_reader), "/v1/collections?limit=1"),
    );
    assert_eq!(left.0, StatusCode::OK);
    assert_eq!(right.0, StatusCode::OK);
    let issued = [
        left.1["next_cursor"]
            .as_str()
            .expect("left concurrent cursor")
            .to_owned(),
        right.1["next_cursor"]
            .as_str()
            .expect("right concurrent cursor")
            .to_owned(),
    ];
    assert_ne!(issued[0], issued[1]);
    assert_eq!(list_cursor_count(migrator, "collections").await, 1);
    let collection_cursor = sqlx::query_scalar::<_, String>(
        "SELECT token FROM list_cursors
         WHERE tenant_id = $1 AND principal_id = '10000000000000000000000000000001'
           AND app_id = 'a0000000000000000000000000000001'
           AND resource_kind = 'collections'",
    )
    .bind(ALPHA_TENANT)
    .fetch_one(migrator)
    .await
    .expect("read current synthetic cursor slot");
    assert!(issued.contains(&collection_cursor));
    let mut continuation_statuses = Vec::new();
    for cursor in &issued {
        continuation_statuses.push(
            get_json(
                app,
                Some(alice_reader),
                &format!("/v1/collections?limit=1&cursor={cursor}"),
            )
            .await
            .0,
        );
    }
    continuation_statuses.sort();
    assert_eq!(
        continuation_statuses,
        [StatusCode::OK, StatusCode::BAD_REQUEST]
    );
    let continuation = get_json(
        app,
        Some(alice_reader),
        &format!("/v1/collections?limit=1&cursor={collection_cursor}"),
    )
    .await;
    assert_eq!(continuation.0, StatusCode::OK);
    assert_eq!(
        continuation.1["items"][0]["collection_id"],
        "30000000000000000000000000000004"
    );
    assert_eq!(continuation.1["truncated"], false);
    assert!(continuation.1["next_cursor"].is_null());
    let collections_text = format!("{}{}", collections.1, continuation.1);
    for forbidden in [
        "30000000000000000000000000000002",
        "30000000000000000000000000000003",
        "30000000000000000000000000000095",
    ] {
        assert!(!collections_text.contains(forbidden));
    }

    let default_items = get_json(app, Some(alice_reader), "/v1/items").await;
    assert_eq!(default_items.0, StatusCode::OK);
    assert_eq!(default_items.1["items"].as_array().map(Vec::len), Some(20));
    assert_eq!(default_items.1["truncated"], true);
    assert!(default_items.1.get("total").is_none());
    let first_item_ids = default_items.1["items"]
        .as_array()
        .expect("first item page")
        .iter()
        .map(|item| item["item_id"].as_str().expect("opaque item ID"))
        .collect::<Vec<_>>();
    let item_cursor = default_items.1["next_cursor"]
        .as_str()
        .expect("opaque item cursor");
    let next_items = get_json(
        app,
        Some(alice_reader),
        &format!("/v1/items?limit=100&cursor={item_cursor}"),
    )
    .await;
    assert_eq!(next_items.0, StatusCode::OK);
    let next_item_ids = next_items.1["items"]
        .as_array()
        .expect("second item page")
        .iter()
        .map(|item| item["item_id"].as_str().expect("opaque item ID"))
        .collect::<Vec<_>>();
    assert!(!next_item_ids.is_empty());
    assert!(
        next_item_ids
            .iter()
            .all(|item_id| !first_item_ids.contains(item_id))
    );
    assert_eq!(next_items.1["truncated"], false);
    assert!(next_items.1["next_cursor"].is_null());
    assert!(next_items.1.get("total").is_none());
    let all_items = get_json(app, Some(alice_reader), "/v1/items?limit=100").await;
    assert_eq!(all_items.0, StatusCode::OK);
    assert_eq!(all_items.1["truncated"], false);
    let all_items_text = all_items.1.to_string();
    assert!(all_items_text.contains(ALLOWED_ITEM));
    for forbidden in [
        BOB_PRIVATE_ITEM,
        FOREIGN_ITEM,
        "40000000000000000000000000000095",
        "40000000000000000000000000000094",
        "40000000000000000000000000000093",
        "40000000000000000000000000000092",
        "40000000000000000000000000000099",
        "40000000000000000000000000000097",
        "LIST_",
        "content",
        "excerpt",
    ] {
        assert!(!all_items_text.contains(forbidden));
    }

    let mut tampered = collection_cursor.clone();
    tampered.replace_range(31..32, if tampered.ends_with('0') { "1" } else { "0" });
    for (bearer, path) in [
        (
            Some(alice_reader),
            format!("/v1/collections?cursor={tampered}"),
        ),
        (
            Some(bob_reader),
            format!("/v1/collections?cursor={collection_cursor}"),
        ),
        (
            Some(alice_reader),
            format!("/v1/items?cursor={collection_cursor}"),
        ),
    ] {
        let denied = get_json(app, bearer, &path).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        assert!(!denied.1.to_string().contains(&collection_cursor));
    }
    sqlx::query("UPDATE list_cursors SET expires_at = clock_timestamp() - interval '1 second' WHERE token = $1")
        .bind(&collection_cursor)
        .execute(migrator)
        .await
        .expect("expire synthetic list cursor");
    assert_eq!(
        get_json(
            app,
            Some(alice_reader),
            &format!("/v1/collections?cursor={collection_cursor}"),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let forged_path = "/v1/items?principal_id=FORGED_LIST_PRINCIPAL&limit=100";
    for bearer in [
        None,
        Some(alice_writer),
        Some("invalid-list-credential-invalid-list-credential-0000"),
    ] {
        let denied = get_json(app, bearer, forged_path).await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert!(!denied.1.to_string().contains("FORGED_LIST"));
    }
    expire_alice_credential(migrator).await;
    assert_eq!(
        get_json(app, Some(alice_reader), forged_path).await.0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(migrator).await;
    revoke_alice_credential(migrator).await;
    assert_eq!(
        get_json(app, Some(alice_reader), forged_path).await.0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(migrator).await;
    for path in [
        "/v1/items?",
        "/v1/items?limit=0",
        "/v1/items?limit=101",
        "/v1/items?limit=1.5",
        "/v1/items?limit=1&limit=2",
        "/v1/items?scope=FORGED_LIST_SCOPE",
    ] {
        let denied = get_json(app, Some(alice_reader), path).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
    }
    let long_cursor = "a".repeat(513);
    assert_eq!(
        get_json(
            app,
            Some(alice_reader),
            &format!("/v1/items?cursor={long_cursor}"),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let body_denied =
        get_json_body(app, Some(alice_reader), "/v1/items", b"FORGED_LIST_BODY").await;
    assert_eq!(body_denied.0, StatusCode::BAD_REQUEST);
    assert!(!body_denied.1.to_string().contains("FORGED_LIST"));

    let mut held = migrator.begin().await.expect("begin list authority lock");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id = $1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *held)
        .await
        .expect("hold list authority lock");
    let blocked_app = app.clone();
    let blocked_reader = alice_reader.to_owned();
    let blocked = tokio::spawn(async move {
        get_json(&blocked_app, Some(&blocked_reader), "/v1/items?limit=1").await
    });
    wait_for_list_authority_lock_wait(migrator).await;
    revoke_alice_credential(migrator).await;
    held.commit().await.expect("release list authority lock");
    let post_wait = blocked.await.expect("join blocked list request");
    assert_eq!(post_wait.0, StatusCode::UNAUTHORIZED);
    restore_alice_credential(migrator).await;

    wait_for_idle_pool(runtime).await;
    for outcome in ["released", "malformed", "unauthenticated"] {
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM read_audit WHERE operation = 'list' AND outcome = $1",
            )
            .bind(outcome)
            .fetch_one(migrator)
            .await
            .expect("count list audit outcome")
                > 0,
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit a
             WHERE operation = 'list' AND
               (target_id IS NOT NULL OR row_to_json(a)::text ~ 'FORGED_LIST|LIST_'
                OR row_to_json(a)::text LIKE '%' || $1 || '%')",
        )
        .bind(collection_cursor)
        .fetch_one(migrator)
        .await
        .expect("list audit excludes cursor, body, and hidden metadata"),
        0
    );

    let closed_pool = PgPoolOptions::new()
        .connect_lazy(&test_database_url(RUNTIME_URL))
        .expect("lazy closed list pool");
    closed_pool.close().await;
    let closed = get_json(
        &router(closed_pool),
        Some(alice_reader),
        "/v1/items?scope=FORGED_LIST_STORAGE",
    )
    .await;
    assert_eq!(closed.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(closed.1["code"], "storage_unavailable");
    assert!(!closed.1.to_string().contains("FORGED_LIST"));
}

async fn list_cursor_count(migrator: &PgPool, resource_kind: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM list_cursors
         WHERE tenant_id = $1 AND principal_id = '10000000000000000000000000000001'
           AND app_id = 'a0000000000000000000000000000001'
           AND resource_kind = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(resource_kind)
    .fetch_one(migrator)
    .await
    .expect("count active synthetic cursor slots")
}

async fn assert_operation_confusion(migrator: &PgPool, runtime: &PgPool, alice_reader: &str) {
    for (index, operation) in ["list", "search"].into_iter().enumerate() {
        let bearer = random_bearer();
        let credential_id = format!("c000000000000000000000000000009{index}");
        sqlx::query(
            "INSERT INTO credentials
             (tenant_id, id, principal_id, app_id, token_digest, credential_class,
              allowed_operations, issued_at, expires_at)
             VALUES ($1, $2, '10000000000000000000000000000001',
                     'a0000000000000000000000000000001', $3, 'agent_reader',
                     ARRAY[$4], clock_timestamp(), clock_timestamp() + interval '1 hour')",
        )
        .bind(ALPHA_TENANT)
        .bind(&credential_id)
        .bind(Sha256::digest(bearer.as_bytes()).as_slice())
        .bind(operation)
        .execute(migrator)
        .await
        .expect("insert single-operation reader credential");
        let incompatible_operations: &[&str] = if operation == "list" {
            &["read", "search"]
        } else {
            &["read", "list"]
        };
        for incompatible in incompatible_operations {
            let error =
                invoke_reader_function(runtime, &bearer, &credential_id, operation, incompatible)
                    .await;
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::code)
                    .as_deref(),
                Some("42501"),
            );
        }
        for attempted_operation in [operation, "read"] {
            let mut transaction = runtime
                .begin()
                .await
                .expect("begin operation-confusion regression");
            sqlx::query_scalar::<_, String>(
                "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
            )
            .bind(Sha256::digest(bearer.as_bytes()).as_slice())
            .fetch_one(&mut *transaction)
            .await
            .expect("set single-operation credential digest");
            for (key, value) in [
                ("app.tenant_id", ALPHA_TENANT),
                ("app.operation", attempted_operation),
            ] {
                sqlx::query_scalar::<_, String>("SELECT set_config($1, $2, true)")
                    .bind(key)
                    .bind(value)
                    .fetch_one(&mut *transaction)
                    .await
                    .expect("set operation-confusion context");
            }
            assert!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM revisions")
                    .fetch_one(&mut *transaction)
                    .await
                    .is_err(),
                "single-operation credentials must have no direct table access",
            );
            transaction
                .rollback()
                .await
                .expect("rollback operation-confusion regression");
        }
    }
    for (context_operation, invoked_operation) in
        [("list", "read"), ("list", "search"), ("search", "list")]
    {
        let wrong_operation = invoke_reader_function(
            runtime,
            alice_reader,
            ALICE_CREDENTIAL,
            context_operation,
            invoked_operation,
        )
        .await;
        assert_eq!(
            wrong_operation
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("42501"),
        );
    }
}

async fn invoke_reader_function(
    runtime: &PgPool,
    bearer: &str,
    credential_id: &str,
    context_operation: &str,
    invoked_operation: &str,
) -> sqlx::Error {
    let mut transaction = runtime
        .begin()
        .await
        .expect("begin constrained-operation regression");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set constrained-operation digest");
    for (key, value) in [
        ("app.tenant_id", ALPHA_TENANT),
        ("app.operation", context_operation),
    ] {
        sqlx::query_scalar::<_, String>("SELECT set_config($1, $2, true)")
            .bind(key)
            .bind(value)
            .fetch_one(&mut *transaction)
            .await
            .expect("set constrained-operation context");
    }
    let result = match invoked_operation {
        "read" => {
            sqlx::query(
                "SELECT * FROM read_current_item(
                   $1, $2, '00000000-0000-0000-0000-000000000096', $3, NULL, NULL)",
            )
            .bind(ALPHA_TENANT)
            .bind(credential_id)
            .bind(ALLOWED_ITEM)
            .execute(&mut *transaction)
            .await
        }
        "search" => {
            sqlx::query(
                "SELECT * FROM search_current_memories(
                   $1, $2, '00000000-0000-0000-0000-000000000096', 'x', 4096, NULL)",
            )
            .bind(ALPHA_TENANT)
            .bind(credential_id)
            .execute(&mut *transaction)
            .await
        }
        "list" => {
            sqlx::query(
                "SELECT * FROM list_current_resources(
                   $1, $2, '00000000-0000-0000-0000-000000000096',
                   'items', 1, NULL)",
            )
            .bind(ALPHA_TENANT)
            .bind(credential_id)
            .execute(&mut *transaction)
            .await
        }
        _ => unreachable!("test invokes only known reader functions"),
    };
    let error = result.expect_err("incompatible constrained function must reject");
    transaction
        .rollback()
        .await
        .expect("rollback constrained-operation regression");
    error
}

#[allow(clippy::too_many_lines)]
async fn assert_fresh_keyword_search(
    migrator: &PgPool,
    item_id: &str,
    revision_id: &str,
    alice_reader: &str,
    alice_writer: &str,
) {
    sqlx::raw_sql(
        "INSERT INTO apps (tenant_id, id, active)
         VALUES ('00000000000000000000000000000001',
                 'a0000000000000000000000000000099', true);
         INSERT INTO subjects (tenant_id, id, app_id, kind, principal_id)
         VALUES ('00000000000000000000000000000001',
                 's0000000000000000000000000000099',
                 'a0000000000000000000000000000001', 'customer', NULL),
                ('00000000000000000000000000000001',
                 's0000000000000000000000000000096',
                 'a0000000000000000000000000000001', 'project', NULL),
                ('00000000000000000000000000000001',
                 's0000000000000000000000000000095',
                 'a0000000000000000000000000000001', 'customer', NULL),
                ('00000000000000000000000000000001',
                 's0000000000000000000000000000094',
                 'a0000000000000000000000000000001', 'customer', NULL),
                ('00000000000000000000000000000001',
                 's0000000000000000000000000000091',
                 'a0000000000000000000000000000001', 'project', NULL),
                ('00000000000000000000000000000001',
                 's0000000000000000000000000000093',
                 'a0000000000000000000000000000099', 'customer', NULL),
                ('00000000000000000000000000000001',
                 's0000000000000000000000000000098',
                 'a0000000000000000000000000000099', 'principal',
                 '10000000000000000000000000000001');
         INSERT INTO items (tenant_id, id, collection_id)
         VALUES ('00000000000000000000000000000001',
                 '40000000000000000000000000000099',
                 '30000000000000000000000000000004')",
    )
    .execute(migrator)
    .await
    .expect("seed customer-scoped search oracle");
    sqlx::raw_sql(
        "INSERT INTO revisions (tenant_id, item_id, id, content) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000099',
            '50000000000000000000000000000099',
            'PRIVATE CREATE ATOMIC CUSTOMER ONLY');
         UPDATE items SET active_revision_id = '50000000000000000000000000000099'
         WHERE tenant_id = '00000000000000000000000000000001'
           AND id = '40000000000000000000000000000099';
         INSERT INTO revision_subjects (tenant_id, item_id, revision_id, subject_id) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000099',
            '50000000000000000000000000000099',
            's0000000000000000000000000000099');
         INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT tenant_id, item_id, id, to_tsvector('simple', content)
         FROM revisions WHERE id = '50000000000000000000000000000099';
         INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT tenant_id, item_id, id, to_tsvector('simple', content)
         FROM revisions WHERE id IN
           ('50000000000000000000000000000002', '50000000000000000000000000000003');
         INSERT INTO items (tenant_id, id, collection_id) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000098',
            '30000000000000000000000000000001');
         INSERT INTO revisions (tenant_id, item_id, id, content) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000098',
            '50000000000000000000000000000098',
            'ALTERNATIVE PRINCIPAL MATCH');
         UPDATE items SET active_revision_id = '50000000000000000000000000000098'
         WHERE tenant_id = '00000000000000000000000000000001'
           AND id = '40000000000000000000000000000098';
         INSERT INTO revision_subjects (tenant_id, item_id, revision_id, subject_id) VALUES
           ('00000000000000000000000000000001', '40000000000000000000000000000098',
            '50000000000000000000000000000098', 's0000000000000000000000000000001'),
           ('00000000000000000000000000000001', '40000000000000000000000000000098',
            '50000000000000000000000000000098', 's0000000000000000000000000000002');
         INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT tenant_id, item_id, id, to_tsvector('simple', content)
         FROM revisions WHERE id = '50000000000000000000000000000098';
         INSERT INTO items (tenant_id, id, collection_id)
         SELECT '00000000000000000000000000000001',
                '7' || lpad(n::text, 31, '0'),
                '30000000000000000000000000000001'
         FROM generate_series(1, 13) AS n;
         INSERT INTO revisions (tenant_id, item_id, id, content)
         SELECT '00000000000000000000000000000001',
                '7' || lpad(n::text, 31, '0'),
                '8' || lpad(n::text, 31, '0'), 'CAP_RESULT_TOKEN'
         FROM generate_series(1, 13) AS n;
         UPDATE items SET active_revision_id = '8' || substring(id FROM 2)
         WHERE tenant_id = '00000000000000000000000000000001'
           AND collection_id = '30000000000000000000000000000001'
           AND id LIKE '7%';
         INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT tenant_id, item_id, id, to_tsvector('simple', content)
         FROM revisions WHERE content = 'CAP_RESULT_TOKEN';
         INSERT INTO revisions (tenant_id, item_id, id, content) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000098',
            '50000000000000000000000000000097',
            'STALE HISTORY UNIQUE TERM');
         INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT tenant_id, item_id, id, to_tsvector('simple', content)
         FROM revisions WHERE id = '50000000000000000000000000000097';
         INSERT INTO items (tenant_id, id, collection_id) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000097',
            '30000000000000000000000000000001');
         INSERT INTO revisions (tenant_id, item_id, id, content) VALUES
           ('00000000000000000000000000000001',
            '40000000000000000000000000000097',
            '50000000000000000000000000000096',
            'CROSS APP SUBJECT UNIQUE TERM');
         UPDATE items SET active_revision_id = '50000000000000000000000000000096'
         WHERE tenant_id = '00000000000000000000000000000001'
           AND id = '40000000000000000000000000000097';
         INSERT INTO revision_subjects (tenant_id, item_id, revision_id, subject_id) VALUES
           ('00000000000000000000000000000001', '40000000000000000000000000000097',
            '50000000000000000000000000000096', 's0000000000000000000000000000098');
         INSERT INTO lexical_representations (tenant_id, item_id, revision_id, document)
         SELECT tenant_id, item_id, id, to_tsvector('simple', content)
         FROM revisions WHERE id = '50000000000000000000000000000096'",
    )
    .execute(migrator)
    .await
    .expect("seed lexical isolation oracles");

    let fresh_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("fresh search runtime session");
    let fresh_app = router(fresh_pool.clone());
    let found = search(
        &fresh_app,
        Some(alice_reader),
        br#"{"query":"PRIVATE CREATE ATOMIC"}"#,
    )
    .await;
    assert_eq!(found.0, StatusCode::OK);
    assert_eq!(found.1["status"], "ready");
    assert_eq!(found.1["items"].as_array().expect("search items").len(), 1);
    assert_eq!(found.1["items"][0]["item_id"], item_id);
    assert_eq!(found.1["items"][0]["revision_id"], revision_id);
    assert_eq!(found.1["items"][0]["reason"], "lexical");
    assert_eq!(found.1["items"][0]["semantic_status"], "not_requested");
    assert_eq!(found.1["items"][0]["validity_status"], "unknown");
    assert!(
        found.1["context_bytes"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_eq!(found.1["truncated"], false);
    assert_eq!(found.1["warnings"], serde_json::json!([]));

    let scoped_content = "SCOPED CEDAR RENEWAL SENTINEL";
    let scoped_body = serde_json::json!({
        "content": scoped_content,
        "subjects": [
            "s0000000000000000000000000000095",
            "s0000000000000000000000000000096"
        ]
    })
    .to_string();
    let scoped_created = post_memory(
        &fresh_app,
        alice_writer,
        "scoped-search-create-01",
        scoped_body.as_bytes(),
    )
    .await;
    assert_eq!(scoped_created.0, StatusCode::CREATED);
    let scoped_item = scoped_created.1["item_id"]
        .as_str()
        .expect("scoped search item ID");
    let scoped = search(
        &fresh_app,
        Some(alice_reader),
        serde_json::json!({
            "query": scoped_content,
            "scope": {"subjects": [
                "s0000000000000000000000000000095",
                "s0000000000000000000000000000096"
            ]},
            "max_context_bytes": 4096,
            "time_mode": "current"
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(scoped.0, StatusCode::OK);
    assert_eq!(scoped.1["items"][0]["item_id"], scoped_item);
    let scoped_revision = scoped_created.1["revision_id"]
        .as_str()
        .expect("scoped search revision ID");
    let scoped_read = get_json(
        &fresh_app,
        Some(alice_reader),
        &format!(
            "/v1/items/{scoped_item}?expected_revision_id={scoped_revision}&subject_id=s0000000000000000000000000000095&subject_id=s0000000000000000000000000000096"
        ),
    )
    .await;
    assert_eq!(scoped_read.0, StatusCode::OK);
    assert_eq!(scoped_read.1["content"], scoped_content);
    let mut normalized_read_scope_denials = Vec::new();
    for query in [
        format!("expected_revision_id={scoped_revision}"),
        format!(
            "expected_revision_id={scoped_revision}&subject_id=s0000000000000000000000000000095"
        ),
        format!(
            "expected_revision_id={scoped_revision}&subject_id=s0000000000000000000000000000096"
        ),
        format!(
            "expected_revision_id={scoped_revision}&subject_id=s0000000000000000000000000000094&subject_id=s0000000000000000000000000000096"
        ),
        format!(
            "expected_revision_id={scoped_revision}&subject_id=s0000000000000000000000000000095&subject_id=s0000000000000000000000000000091"
        ),
    ] {
        let mut denied = get_json(
            &fresh_app,
            Some(alice_reader),
            &format!("/v1/items/{scoped_item}?{query}"),
        )
        .await;
        assert_eq!(denied.0, StatusCode::NOT_FOUND);
        assert_eq!(denied.1["code"], "unavailable");
        for forbidden in [scoped_item, scoped_revision, scoped_content] {
            assert!(!denied.1.to_string().contains(forbidden));
        }
        denied.1["request_id"] = Value::Null;
        normalized_read_scope_denials.push(denied.1);
    }
    assert!(
        normalized_read_scope_denials
            .windows(2)
            .all(|pair| pair[0] == pair[1])
    );

    let mut read_scope_rejection_ids = Vec::new();
    for query in [
        "subject_id=s0000000000000000000000000000092".to_owned(),
        format!("subject_id={ALICE_SUBJECT}"),
        "subject_id=s0000000000000000000000000000093".to_owned(),
        "subject_id=s0000000000000000000000000000095&subject_id=s0000000000000000000000000000095"
            .to_owned(),
        "subject_id=%730000000000000000000000000000095".to_owned(),
    ] {
        let denied = get_json(
            &fresh_app,
            Some(alice_reader),
            &format!("/v1/items/{scoped_item}?{query}"),
        )
        .await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        assert!(!denied.1.to_string().contains(scoped_item));
        assert!(!denied.1.to_string().contains("s000000"));
        read_scope_rejection_ids.push(request_id(&denied.1).to_owned());
    }
    let too_many_read_subjects = (0..9)
        .map(|index| format!("subject_id=s{index:031}"))
        .collect::<Vec<_>>()
        .join("&");
    let denied = get_json(
        &fresh_app,
        Some(alice_reader),
        &format!("/v1/items/{scoped_item}?{too_many_read_subjects}"),
    )
    .await;
    assert_eq!(denied.0, StatusCode::BAD_REQUEST);
    assert_eq!(denied.1["code"], "malformed");
    read_scope_rejection_ids.push(request_id(&denied.1).to_owned());
    for bearer in [None, Some(alice_writer)] {
        let denied = get_json(
            &fresh_app,
            bearer,
            &format!("/v1/items/{scoped_item}?subject_id=s0000000000000000000000000000092"),
        )
        .await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.1["code"], "unauthenticated");
    }

    let alternatives_content = "OR CUSTOMER PROJECT SENTINEL";
    let alternatives_created = post_memory(
        &fresh_app,
        alice_writer,
        "scope-or-create-01",
        serde_json::json!({
            "content": alternatives_content,
            "subjects": [
                "s0000000000000000000000000000095",
                "s0000000000000000000000000000094",
                "s0000000000000000000000000000096"
            ]
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(alternatives_created.0, StatusCode::CREATED);
    let alternatives_item = alternatives_created.1["item_id"]
        .as_str()
        .expect("alternative-scope item");
    for customer in [
        "s0000000000000000000000000000095",
        "s0000000000000000000000000000094",
    ] {
        let found = search(
            &fresh_app,
            Some(alice_reader),
            serde_json::json!({"query": alternatives_content, "scope": {"subjects": [
                customer, "s0000000000000000000000000000096"
            ]}})
            .to_string()
            .as_bytes(),
        )
        .await;
        assert_eq!(found.0, StatusCode::OK);
        assert_eq!(found.1["items"][0]["item_id"], alternatives_item);
    }
    for subjects in [
        serde_json::json!(["s0000000000000000000000000000095"]),
        serde_json::json!([
            "s0000000000000000000000000000095",
            "s0000000000000000000000000000091"
        ]),
    ] {
        let excluded = search(
            &fresh_app,
            Some(alice_reader),
            serde_json::json!({"query": alternatives_content, "scope": {"subjects": subjects}})
                .to_string()
                .as_bytes(),
        )
        .await;
        assert_eq!(excluded.0, StatusCode::OK);
        assert_eq!(excluded.1["items"], serde_json::json!([]));
    }
    for subjects in [
        serde_json::json!([]),
        serde_json::json!(["s0000000000000000000000000000095"]),
        serde_json::json!(["s0000000000000000000000000000096"]),
        serde_json::json!([
            "s0000000000000000000000000000094",
            "s0000000000000000000000000000096"
        ]),
    ] {
        let body = if subjects.as_array().is_some_and(Vec::is_empty) {
            serde_json::json!({"query": scoped_content})
        } else {
            serde_json::json!({"query": scoped_content, "scope": {"subjects": subjects}})
        };
        let excluded = search(&fresh_app, Some(alice_reader), body.to_string().as_bytes()).await;
        assert_eq!(excluded.0, StatusCode::OK);
        assert_eq!(excluded.1["items"], serde_json::json!([]));
    }

    let tiny = search(
        &fresh_app,
        Some(alice_reader),
        serde_json::json!({"query": scoped_content, "scope": {"subjects": [
            "s0000000000000000000000000000095",
            "s0000000000000000000000000000096"
        ]}, "max_context_bytes": 1})
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(tiny.0, StatusCode::OK);
    assert_eq!(tiny.1["context_bytes"], 1);
    assert_eq!(
        tiny.1["items"][0]["excerpt"].as_str().map(str::len),
        Some(1)
    );
    assert_eq!(tiny.1["truncated"], true);

    let utf8_content = "ééé CONSERVATIVE UTF8 BYTE BUDGET SENTINEL";
    let utf8_created = post_memory(
        &fresh_app,
        alice_writer,
        "utf8-byte-budget-create-01",
        serde_json::json!({
            "content": utf8_content,
            "subjects": ["s0000000000000000000000000000095"]
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(utf8_created.0, StatusCode::CREATED);
    let utf8 = search(
        &fresh_app,
        Some(alice_reader),
        serde_json::json!({
            "query": "CONSERVATIVE UTF8 BYTE BUDGET SENTINEL",
            "scope": {"subjects": ["s0000000000000000000000000000095"]},
            "max_context_bytes": 5
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(utf8.0, StatusCode::OK);
    assert_eq!(utf8.1["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(utf8.1["items"][0]["excerpt"], "éé");
    assert_eq!(utf8.1["context_bytes"], 4);
    assert!(utf8.1.get("context_tokens").is_none());
    assert_eq!(utf8.1["truncated"], true);

    let corrected_content = "SCOPED CEDAR CURRENT SENTINEL";
    let corrected = put_memory(
        &fresh_app,
        alice_writer,
        "scoped-search-correct-01",
        scoped_item,
        serde_json::json!({
            "expected_revision_id": scoped_revision,
            "content": corrected_content,
            "subjects": [
                "s0000000000000000000000000000095",
                "s0000000000000000000000000000096"
            ]
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    assert_eq!(corrected.0, StatusCode::OK);
    let corrected_revision = corrected.1["revision_id"]
        .as_str()
        .expect("corrected scoped revision");
    let stale = get_json(
        &fresh_app,
        Some(alice_reader),
        &format!(
            "/v1/items/{scoped_item}?expected_revision_id={scoped_revision}&subject_id=s0000000000000000000000000000095&subject_id=s0000000000000000000000000000096"
        ),
    )
    .await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["code"], "stale_context");
    for forbidden in [
        scoped_item,
        scoped_revision,
        corrected_revision,
        scoped_content,
        corrected_content,
    ] {
        assert!(!stale.1.to_string().contains(forbidden));
    }
    let current = get_json(
        &fresh_app,
        Some(alice_reader),
        &format!(
            "/v1/items/{scoped_item}?expected_revision_id={corrected_revision}&subject_id=s0000000000000000000000000000095&subject_id=s0000000000000000000000000000096"
        ),
    )
    .await;
    assert_eq!(current.0, StatusCode::OK);
    assert_eq!(current.1["content"], corrected_content);

    let nine_subjects = (0..9)
        .map(|index| format!("s{index:031}"))
        .collect::<Vec<_>>();
    let malformed_scope_bodies = [
        serde_json::json!({"query":"SCOPE_EMPTY_SENTINEL","scope":{"subjects":[]}})
            .to_string(),
        r#"{"query":"SCOPE_NULL_SENTINEL","scope":null}"#.to_owned(),
        serde_json::json!({"query":"SCOPE_DUPLICATE_SENTINEL","scope":{"subjects":[
            "s0000000000000000000000000000095",
            "s0000000000000000000000000000095"
        ]}})
        .to_string(),
        serde_json::json!({"query":"SCOPE_TOO_MANY_SENTINEL","scope":{"subjects":nine_subjects}})
            .to_string(),
        serde_json::json!({"query":"SCOPE_LONG_ID_SENTINEL","scope":{"subjects":["X".repeat(65)]}})
            .to_string(),
        serde_json::json!({"query":"SCOPE_UNKNOWN_SENTINEL","scope":{"subjects":[
            "s0000000000000000000000000000092"
        ]}})
        .to_string(),
        serde_json::json!({"query":"SCOPE_WRONG_KIND_SENTINEL","scope":{"subjects":[
            ALICE_SUBJECT
        ]}})
        .to_string(),
        serde_json::json!({"query":"SCOPE_CROSS_APP_SENTINEL","scope":{"subjects":[
            "s0000000000000000000000000000093"
        ]}})
        .to_string(),
        r#"{"query":"SCOPE_DUPLICATE_FIELD_SENTINEL","scope":{"subjects":["s0000000000000000000000000000095"],"subjects":["s0000000000000000000000000000096"]}}"#.to_owned(),
        r#"{"query":"SCOPE_UNKNOWN_FIELD_SENTINEL","scope":{"subjects":["s0000000000000000000000000000095"],"customer_id":"FORGED_SCOPE_SENTINEL"}}"#.to_owned(),
    ];
    let mut scope_rejection_ids = Vec::new();
    let mut normalized_scope_denials = Vec::new();
    for body in &malformed_scope_bodies {
        let mut denied = search(&fresh_app, Some(alice_reader), body.as_bytes()).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        for forbidden in [
            "SENTINEL",
            ALICE_SUBJECT,
            "s0000000000000000000000000000093",
        ] {
            assert!(!denied.1.to_string().contains(forbidden));
        }
        scope_rejection_ids.push(request_id(&denied.1).to_owned());
        denied.1["request_id"] = Value::Null;
        normalized_scope_denials.push(denied.1);
    }
    assert!(
        normalized_scope_denials
            .windows(2)
            .all(|pair| pair[0] == pair[1])
    );
    for body in [
        r#"{"query":"BUDGET_ZERO_SENTINEL","max_context_bytes":0}"#,
        r#"{"query":"BUDGET_HIGH_SENTINEL","max_context_bytes":16385}"#,
        r#"{"query":"BUDGET_FLOAT_SENTINEL","max_context_bytes":1.5}"#,
        r#"{"query":"BUDGET_LEGACY_SENTINEL","max_context_tokens":1}"#,
        r#"{"query":"TIME_UNKNOWN_SENTINEL","time_mode":"tomorrow"}"#,
        r#"{"query":"TIME_EXTRA_SENTINEL","time_mode":"valid_on","valid_on":"2026-09-06T00:00:00Z"}"#,
    ] {
        let denied = search(&fresh_app, Some(alice_reader), body.as_bytes()).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
        assert!(!denied.1.to_string().contains("SENTINEL"));
        scope_rejection_ids.push(request_id(&denied.1).to_owned());
    }

    for bearer in [None, Some(alice_writer)] {
        let auth_first = search(
            &fresh_app,
            bearer,
            br#"{"query":"AUTH_FIRST_TIME_SENTINEL","time_mode":"known_as_of","scope":{"subjects":["FORGED_SCOPE_SENTINEL"]}}"#,
        )
        .await;
        assert_eq!(auth_first.0, StatusCode::UNAUTHORIZED);
        assert_eq!(auth_first.1["code"], "unauthenticated");
        assert!(!auth_first.1.to_string().contains("SENTINEL"));
    }

    let mut unsupported_ids = Vec::new();
    for mode in ["valid_on", "known_as_of"] {
        let unsupported = search(
            &fresh_app,
            Some(alice_reader),
            serde_json::json!({"query": "SENTINEL", "time_mode": mode})
                .to_string()
                .as_bytes(),
        )
        .await;
        assert_eq!(unsupported.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unsupported.1["code"], "unsupported_time_semantics");
        assert!(!unsupported.1.to_string().contains("SENTINEL"));
        unsupported_ids.push(request_id(&unsupported.1).to_owned());
    }
    let current_read = get_json(
        &fresh_app,
        Some(alice_reader),
        &format!("/v1/items/{ALLOWED_ITEM}?time_mode=current"),
    )
    .await;
    assert_eq!(current_read.0, StatusCode::OK);
    assert_eq!(current_read.1["item_id"], ALLOWED_ITEM);
    let current_expected_read = get_json(
        &fresh_app,
        Some(alice_reader),
        &format!(
            "/v1/items/{ALLOWED_ITEM}?expected_revision_id=50000000000000000000000000000001&time_mode=current"
        ),
    )
    .await;
    assert_eq!(current_expected_read.0, StatusCode::OK);
    for mode in ["valid_on", "known_as_of"] {
        let unsupported = get_json(
            &fresh_app,
            Some(alice_reader),
            &format!("/v1/items/{ALLOWED_ITEM}?time_mode={mode}"),
        )
        .await;
        assert_eq!(unsupported.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(unsupported.1["code"], "unsupported_time_semantics");
        assert!(!unsupported.1.to_string().contains(ALLOWED_ITEM));
        unsupported_ids.push(request_id(&unsupported.1).to_owned());
    }
    for query in [
        "time_mode=tomorrow",
        "time_mode=current&time_mode=current",
        "time_mode=%63urrent",
        "time_mode=valid_on&valid_on=2026-09-06",
    ] {
        let malformed = get_json(
            &fresh_app,
            Some(alice_reader),
            &format!("/v1/items/{ALLOWED_ITEM}?{query}"),
        )
        .await;
        assert_eq!(malformed.0, StatusCode::BAD_REQUEST);
        assert_eq!(malformed.1["code"], "malformed");
        assert!(!malformed.1.to_string().contains(ALLOWED_ITEM));
        scope_rejection_ids.push(request_id(&malformed.1).to_owned());
    }
    wait_for_idle_pool(&fresh_pool).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit
             WHERE request_id = ANY($1) AND outcome='malformed' AND target_id IS NULL",
        )
        .bind(scope_rejection_ids.as_slice())
        .fetch_one(migrator)
        .await
        .expect("request-bound malformed scope/time audits"),
        i64::try_from(scope_rejection_ids.len()).expect("bounded rejection count")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit
             WHERE request_id = ANY($1) AND operation='read'
               AND outcome='malformed' AND target_id IS NULL
               AND tenant_id=$2 AND principal_id=$3 AND app_id=$4",
        )
        .bind(read_scope_rejection_ids.as_slice())
        .bind(ALPHA_TENANT)
        .bind("10000000000000000000000000000001")
        .bind("a0000000000000000000000000000001")
        .fetch_one(migrator)
        .await
        .expect("request-bound malformed read-scope audits"),
        i64::try_from(read_scope_rejection_ids.len()).expect("bounded read rejection count")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit
             WHERE request_id = ANY($1) AND outcome='unsupported_time_semantics'
               AND target_id IS NULL",
        )
        .bind(unsupported_ids.as_slice())
        .fetch_one(migrator)
        .await
        .expect("request-bound unsupported-time audits"),
        i64::try_from(unsupported_ids.len()).expect("bounded unsupported count")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit a WHERE request_id = ANY($1)
             AND row_to_json(a)::text ~ 'SENTINEL|s0000000000000000000000000000(001|09[2356])'",
        )
        .bind(scope_rejection_ids.as_slice())
        .fetch_one(migrator)
        .await
        .expect("scope/time audits exclude selectors and query text"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit a WHERE request_id = ANY($1)
             AND row_to_json(a)::text ~ 'SENTINEL|s0000000000000000000000000000(001|09[2356])'",
        )
        .bind(read_scope_rejection_ids.as_slice())
        .fetch_one(migrator)
        .await
        .expect("read-scope audits exclude selectors and content"),
        0
    );
    let alternative = search_json(
        &fresh_app,
        Some(alice_reader),
        "ALTERNATIVE PRINCIPAL MATCH",
    )
    .await;
    assert_eq!(alternative.0, StatusCode::OK);
    assert_eq!(
        alternative.1["items"]
            .as_array()
            .expect("alternative items")
            .len(),
        1
    );
    let capped = search_json(&fresh_app, Some(alice_reader), "CAP_RESULT_TOKEN").await;
    assert_eq!(capped.0, StatusCode::OK);
    assert_eq!(
        capped.1["items"].as_array().expect("capped items").len(),
        12
    );
    assert_eq!(capped.1["truncated"], true);
    assert!(capped.1.get("total").is_none());

    let mut empty_results = Vec::new();
    for query in [
        "FORBIDDEN_BOB_PRIVATE",
        "FORBIDDEN_BETA_COMPANY",
        "NO SUCH MEMORY QUERY",
        "PRIVATE CREATE ATOMIC CUSTOMER ONLY",
        "STALE HISTORY UNIQUE TERM",
        "CROSS APP SUBJECT UNIQUE TERM",
    ] {
        let response = search_json(&fresh_app, Some(alice_reader), query).await;
        assert_eq!(response.0, StatusCode::OK);
        assert_eq!(response.1["items"], serde_json::json!([]));
        assert!(!response.1.to_string().contains("FORBIDDEN_"));
        let mut normalized = response.1.clone();
        normalized["request_id"] = serde_json::Value::Null;
        empty_results.push(normalized);
    }
    assert!(empty_results.windows(2).all(|pair| pair[0] == pair[1]));
    let missing = request(&fresh_app, alice_reader, MISSING_ITEM).await;
    for scoped_item in [
        "40000000000000000000000000000099",
        "40000000000000000000000000000097",
    ] {
        let mut denied = request(&fresh_app, alice_reader, scoped_item).await;
        assert_eq!(denied.0, StatusCode::NOT_FOUND);
        assert!(!denied.1.to_string().contains(scoped_item));
        denied.1["request_id"] = Value::Null;
        let mut normalized_missing = missing.1.clone();
        normalized_missing["request_id"] = Value::Null;
        assert_eq!(denied.1, normalized_missing);
    }

    let forged =
        br#"{"query":"PRIVATE","tenant_id":"FORGED_TENANT","principal_id":"FORGED_PRINCIPAL"}"#;
    for bearer in [
        None,
        Some(alice_writer),
        Some("invalid-invalid-invalid-invalid-invalid-invalid-0000"),
    ] {
        let denied = search(&fresh_app, bearer, forged).await;
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert_eq!(denied.1["code"], "unauthenticated");
        assert!(!denied.1.to_string().contains("FORGED_"));
    }
    expire_alice_credential(migrator).await;
    assert_eq!(
        search(&fresh_app, Some(alice_reader), forged).await.0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(migrator).await;
    revoke_alice_credential(migrator).await;
    assert_eq!(
        search(&fresh_app, Some(alice_reader), forged).await.0,
        StatusCode::UNAUTHORIZED
    );
    restore_alice_credential(migrator).await;

    for malformed in [
        br#"{"query":""}"#.as_slice(),
        br#"{"query":"   \n\t"}"#,
        br#"{"query":"A","query":"B"}"#.as_slice(),
        br#"{"query":7}"#,
        br#"{"query":"A","scope":"FORGED_SCOPE"}"#,
        br#"{"query":"A"} trailing"#,
    ] {
        let denied = search(&fresh_app, Some(alice_reader), malformed).await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(denied.1["code"], "malformed");
    }
    let overlong_query = "Q".repeat(4097);
    assert_eq!(
        search_json(&fresh_app, Some(alice_reader), &overlong_query)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let oversized = vec![b'X'; 16 * 1024 + 1];
    assert_eq!(
        search(&fresh_app, Some(alice_reader), &oversized).await.0,
        StatusCode::BAD_REQUEST
    );
    for content_type in [None, Some("text/plain")] {
        let denied = search_with_content_type(
            &fresh_app,
            Some(alice_reader),
            content_type,
            br#"{"query":"PRIVATE"}"#,
        )
        .await;
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
    }

    let mut held = migrator.begin().await.expect("begin search authority lock");
    sqlx::query("SELECT authority_epoch FROM tenant_authority WHERE tenant_id = $1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *held)
        .await
        .expect("hold search authority lock");
    let blocked_app = fresh_app.clone();
    let blocked_reader = alice_reader.to_owned();
    let blocked = tokio::spawn(async move {
        search_json(&blocked_app, Some(&blocked_reader), "PRIVATE CREATE").await
    });
    wait_for_search_authority_lock_wait(migrator).await;
    revoke_alice_credential(migrator).await;
    held.commit().await.expect("release search authority lock");
    let denied = blocked.await.expect("join blocked search");
    assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
    restore_alice_credential(migrator).await;

    wait_for_idle_pool(&fresh_pool).await;
    for outcome in ["malformed", "unauthenticated"] {
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM read_audit
                 WHERE operation = 'search' AND outcome = $1",
            )
            .bind(outcome)
            .fetch_one(migrator)
            .await
            .expect("count required search rejection outcome")
                > 0,
            "missing sanitized {outcome} search audit",
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit
             WHERE operation = 'search' AND target_id IS NOT NULL",
        )
        .fetch_one(migrator)
        .await
        .expect("search audit has no query target"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit a
             WHERE operation = 'search' AND row_to_json(a)::text ~
               '(PRIVATE CREATE|FORBIDDEN_|CAP_RESULT|NO SUCH MEMORY)'",
        )
        .fetch_one(migrator)
        .await
        .expect("search audit excludes query and content"),
        0
    );
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
    .expect("set synthetic writer validity");
}

async fn persistence_counts(pool: &PgPool) -> (i64, i64, i64, i64, i64, i64) {
    let row = sqlx::query(
        "SELECT (SELECT count(*) FROM items) AS items,
                (SELECT count(*) FROM revisions) AS revisions,
                (SELECT count(*) FROM revision_subjects) AS revision_subjects,
                (SELECT count(*) FROM lexical_representations) AS lexical,
                (SELECT count(*) FROM idempotency_records) AS idempotency,
                (SELECT count(*) FROM mutation_audit) AS audits",
    )
    .fetch_one(pool)
    .await
    .expect("count atomic persistence tables");
    (
        row.try_get("items").expect("item count"),
        row.try_get("revisions").expect("revision count"),
        row.try_get("revision_subjects")
            .expect("revision subject count"),
        row.try_get("lexical").expect("lexical count"),
        row.try_get("idempotency").expect("idempotency count"),
        row.try_get("audits").expect("audit count"),
    )
}

async fn created_private_target_snapshot(pool: &PgPool, item_id: &str) -> String {
    sqlx::query_scalar(
        "SELECT row_to_json(snapshot)::text
         FROM (
           SELECT i.collection_id, i.active_revision_id, i.deleted_at,
                  c.app_id, c.audience_kind, c.owner_principal_id, c.withdrawn_at,
                  r.id AS revision_id, r.content, r.valid_from, r.valid_until,
                  (SELECT array_agg(rs.subject_id ORDER BY rs.subject_id)
                   FROM revision_subjects rs
                   WHERE rs.tenant_id = r.tenant_id AND rs.item_id = r.item_id
                     AND rs.revision_id = r.id) AS subjects,
                  (SELECT array_agg(l.document::text ORDER BY l.revision_id)
                   FROM lexical_representations l
                   WHERE l.tenant_id = r.tenant_id AND l.item_id = r.item_id
                     AND l.revision_id = r.id) AS lexical
           FROM items i
           JOIN collections c ON c.tenant_id = i.tenant_id AND c.id = i.collection_id
           JOIN revisions r
             ON r.tenant_id = i.tenant_id AND r.item_id = i.id
            AND r.id = i.active_revision_id
           WHERE i.tenant_id = $1 AND i.id = $2
         ) AS snapshot",
    )
    .bind(ALPHA_TENANT)
    .bind(item_id)
    .fetch_one(pool)
    .await
    .expect("snapshot exact created private target")
}

async fn authority_epoch(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT authority_epoch FROM tenant_authority WHERE tenant_id = $1")
        .bind(ALPHA_TENANT)
        .fetch_one(pool)
        .await
        .expect("read tenant authority epoch")
}

fn canonical_create_digest(body: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for component in [b"POST".as_slice(), b"/v1/memories".as_slice(), body] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    hasher.finalize().to_vec()
}

fn canonical_forget_digest(item_id: &str, body: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    let path = format!("/v1/items/{item_id}");
    for component in [b"DELETE".as_slice(), path.as_bytes(), body] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    hasher.finalize().to_vec()
}

async fn assert_null_subjects_rejected(pool: &PgPool, bearer: &str) {
    let mut transaction = pool.begin().await.expect("begin null-subject regression");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set writer digest for direct regression");
    set_local(&mut transaction, "app.tenant_id", ALPHA_TENANT).await;
    let result = sqlx::query(
        "SELECT * FROM create_private_memory(
           $1, $2, '00000000-0000-0000-0000-000000000099', $3, $4,
           $5, $6, NULL, NULL)",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .bind(vec![0xb6_u8; 32])
    .bind(vec![0xc7_u8; 32])
    .bind("NULL_SUBJECT_SENTINEL")
    .bind(Option::<Vec<String>>::None)
    .execute(&mut *transaction)
    .await;
    transaction
        .rollback()
        .await
        .expect("rollback direct null-subject regression");
    assert!(
        result.is_err(),
        "NULL subjects must be rejected by the database"
    );
}

async fn direct_create_without_tenant(pool: &PgPool, bearer: &str) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await.expect("begin missing-tenant regression");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set writer digest without tenant context");
    let result = sqlx::query(
        "SELECT * FROM create_private_memory(
           $1, $2, '00000000-0000-0000-0000-000000000097', $3, $4,
           $5, $6, NULL, NULL)",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .bind(vec![0xd8_u8; 32])
    .bind(vec![0xe9_u8; 32])
    .bind("MISSING_TENANT_CONTEXT_SENTINEL")
    .bind(vec![ALICE_SUBJECT.to_owned()])
    .execute(&mut *transaction)
    .await;
    match result {
        Ok(_) => transaction.commit().await,
        Err(error) => {
            transaction
                .rollback()
                .await
                .expect("rollback rejected missing-tenant regression");
            Err(error)
        }
    }
}

async fn wait_for_idle_pool(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        while pool.num_idle() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime pool must expose an idle connection for exact audit assertions");
}

async fn wait_for_authority_lock_wait(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                   SELECT 1 FROM pg_stat_activity
                   WHERE usename = 'agentic_memory_runtime'
                     AND query LIKE '%read_current_item%'
                     AND wait_event_type = 'Lock'
                 )",
            )
            .fetch_one(pool)
            .await
            .expect("observe runtime authority wait");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime request must block on the held authority lock");
}

async fn wait_for_writer_authority_lock_wait(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                   SELECT 1 FROM pg_stat_activity
                   WHERE usename = 'agentic_memory_runtime'
                     AND query LIKE '%create_private_memory%'
                     AND wait_event_type = 'Lock'
                 )",
            )
            .fetch_one(pool)
            .await
            .expect("observe runtime writer authority wait");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime create must block on the held writer authority lock");
}

async fn wait_for_write_waiters(pool: &PgPool, function_name: &str, expected: i64) {
    let pattern = format!("%{function_name}%");
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT count(*) >= $2 FROM pg_stat_activity
                 WHERE usename='agentic_memory_runtime' AND query LIKE $1
                   AND wait_event_type='Lock'",
            )
            .bind(&pattern)
            .bind(expected)
            .fetch_one(pool)
            .await
            .expect("observe concurrent write waiters");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both runtime writes must wait at the tenant gate");
}

async fn replay_while_writer_expires_on_item(
    app: &axum::Router,
    migrator: &PgPool,
    writer: &str,
    operation: &str,
    key: &str,
    item_id: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut held = migrator.begin().await.expect("begin replay item gate");
    sqlx::query("SELECT id FROM items WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .bind(item_id)
        .execute(&mut *held)
        .await
        .expect("hold exact replay item gate");
    set_writer_validity(migrator, false, false).await;
    let replay_app = app.clone();
    let replay_writer = writer.to_owned();
    let replay_key = key.to_owned();
    let replay_item = item_id.to_owned();
    let replay_body = body.to_vec();
    let replay_operation = operation.to_owned();
    let replay = tokio::spawn(async move {
        if replay_operation == "create" {
            post_memory(&replay_app, &replay_writer, &replay_key, &replay_body).await
        } else {
            put_memory(
                &replay_app,
                &replay_writer,
                &replay_key,
                &replay_item,
                &replay_body,
            )
            .await
        }
    });
    let function_name = if operation == "create" {
        "create_private_memory"
    } else {
        "correct_private_memory"
    };
    wait_for_write_waiters(migrator, function_name, 1).await;
    sqlx::query(
        "UPDATE credentials SET expires_at=clock_timestamp()-interval '1 second'
         WHERE tenant_id=$1 AND id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .execute(migrator)
    .await
    .expect("expire replay credential after observing exact item wait");
    held.commit().await.expect("release replay item gate");
    let response = replay.await.expect("join expired replay");
    set_writer_validity(migrator, false, false).await;
    response
}

async fn wait_for_correction_authority_lock_wait(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                 WHERE usename='agentic_memory_runtime'
                   AND query LIKE '%correct_private_memory%'
                   AND wait_event_type='Lock')",
            )
            .fetch_one(pool)
            .await
            .expect("observe correction authority wait");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime correction must block on tenant authority");
}

async fn wait_for_search_authority_lock_wait(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                   SELECT 1 FROM pg_stat_activity
                   WHERE usename = 'agentic_memory_runtime'
                     AND query LIKE '%search_current_memories%'
                     AND wait_event_type = 'Lock'
                 )",
            )
            .fetch_one(pool)
            .await
            .expect("observe runtime search authority wait");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime search must block on the held authority lock");
}

async fn wait_for_list_authority_lock_wait(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                   SELECT 1 FROM pg_stat_activity
                   WHERE usename = 'agentic_memory_runtime'
                     AND query LIKE '%list_current_resources%'
                     AND wait_event_type = 'Lock'
                 )",
            )
            .fetch_one(pool)
            .await
            .expect("observe runtime list authority wait");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime list must block on the held authority lock");
}

async fn restore_alice_credential(pool: &PgPool) {
    sqlx::query(
        "UPDATE credentials SET issued_at = clock_timestamp() - interval '1 minute',
                                expires_at = clock_timestamp() + interval '24 hours',
                                revoked_at = NULL
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(pool)
    .await
    .expect("restore Alice credential");
}

async fn expire_alice_credential(pool: &PgPool) {
    sqlx::query(
        "UPDATE credentials
         SET issued_at = clock_timestamp() - interval '2 hours',
             expires_at = clock_timestamp() - interval '1 hour'
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(pool)
    .await
    .expect("expire Alice search credential");
}

async fn revoke_alice_credential(pool: &PgPool) {
    sqlx::query(
        "UPDATE credentials SET revoked_at = clock_timestamp()
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .execute(pool)
    .await
    .expect("revoke Alice search credential");
}

async fn setup() -> (PgPool, PgPool, PgPool, String, String, String) {
    let migrator = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(MIGRATOR_URL))
        .await
        .expect("run `just setup` first");
    let runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(&test_database_url(RUNTIME_URL))
        .await
        .expect("restricted runtime connection");
    let purge_worker = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url(PURGE_WORKER_URL))
        .await
        .expect("restricted purge worker connection");
    let mut transaction = migrator
        .begin()
        .await
        .expect("begin synthetic fixture reset");
    sqlx::raw_sql(include_str!("fixtures/reset.sql"))
        .execute(&mut *transaction)
        .await
        .expect("reset synthetic fixture");
    let alice = random_bearer();
    let bob = random_bearer();
    let writer = random_bearer();
    insert_reader(
        &mut transaction,
        "c0000000000000000000000000000001",
        "10000000000000000000000000000001",
        &alice,
    )
    .await;
    insert_writer(&mut transaction, &writer).await;
    insert_reader(
        &mut transaction,
        "c0000000000000000000000000000002",
        "10000000000000000000000000000002",
        &bob,
    )
    .await;
    transaction
        .commit()
        .await
        .expect("commit synthetic fixture");
    (migrator, runtime, purge_worker, alice, bob, writer)
}

async fn insert_writer(transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>, bearer: &str) {
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id, id, principal_id, app_id, token_digest, credential_class, allowed_operations, issued_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'trusted_writer', ARRAY['create', 'correct', 'forget'], clock_timestamp(), clock_timestamp() + interval '24 hours')",
    )
    .bind(ALPHA_TENANT)
    .bind(WRITER_CREDENTIAL)
    .bind("10000000000000000000000000000001")
    .bind("a0000000000000000000000000000001")
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .execute(&mut **transaction)
    .await
    .expect("insert digest-only synthetic writer credential");
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
    .bind("a0000000000000000000000000000001")
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .execute(&mut **transaction)
    .await
    .expect("insert digest-only synthetic credential");
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

async fn assert_runtime_role(pool: &PgPool) {
    let row = sqlx::query(
        "SELECT current_user AS name, r.rolsuper, r.rolcreatedb, r.rolcreaterole, r.rolinherit, r.rolbypassrls,
                NOT EXISTS (SELECT 1 FROM pg_auth_members m WHERE m.member = r.oid) AS no_memberships,
                NOT EXISTS (SELECT 1 FROM pg_database d WHERE d.datdba = r.oid)
                  AND NOT EXISTS (SELECT 1 FROM pg_namespace n WHERE n.nspowner = r.oid)
                  AND NOT EXISTS (SELECT 1 FROM pg_class c WHERE c.relowner = r.oid)
                  AND NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.proowner = r.oid) AS owns_no_objects,
                NOT has_database_privilege(current_user, current_database(), 'CREATE,TEMPORARY') AS cannot_create_database_objects,
                NOT EXISTS (
                  SELECT 1
                  FROM pg_class c
                  JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p')
                    AND has_table_privilege(
                      current_user, c.oid,
                      'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER'
                    )
                ) AS cannot_access_tables_directly,
                NOT has_function_privilege(current_user, 'reader_tenant_scope(text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'current_reader_context(text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'current_writer_context(text,text,text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'resolve_current_reader(text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'resolve_current_writer(text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'lock_tenant_authority(text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'record_request_rejection(text,text,text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'record_read_audit(text,text,text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'read_current_item(text,text,text,text,text,text[])', 'EXECUTE')
                  AND has_function_privilege(current_user, 'search_current_memories(text,text,text,text,integer,text[])', 'EXECUTE')
                  AND has_function_privilege(current_user, 'search_current_memories_clause_union_v1(text,text,text,text,integer,text[])', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'lexical_clause_queries_v1(text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'han_bigram_document_v1(text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'list_current_resources(text,text,text,text,integer,text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'create_private_memory(text,text,text,bytea,bytea,text,text[],text,text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'correct_private_memory(text,text,text,bytea,bytea,text,text,text,text[],text,text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'parse_memory_validity(text)', 'EXECUTE')
                  AND has_function_privilege(current_user, 'forget_private_memory(text,text,text,bytea,bytea,text,text)', 'EXECUTE')
                  AND NOT has_function_privilege(current_user, 'process_forget_purge(text,text,bigint)', 'EXECUTE') AS constrained_functions_only
         FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(pool)
    .await
    .expect("inspect runtime role");
    assert_eq!(
        row.try_get::<String, _>("name").expect("role name"),
        "agentic_memory_runtime"
    );
    for field in [
        "rolsuper",
        "rolcreatedb",
        "rolcreaterole",
        "rolinherit",
        "rolbypassrls",
    ] {
        assert!(
            !row.try_get::<bool, _>(field).expect("role flag"),
            "unsafe role flag: {field}"
        );
    }
    for field in [
        "no_memberships",
        "owns_no_objects",
        "cannot_create_database_objects",
        "cannot_access_tables_directly",
        "constrained_functions_only",
    ] {
        assert!(
            row.try_get::<bool, _>(field).expect("role safety property"),
            "missing role safety property: {field}"
        );
    }
}

async fn assert_purge_worker_role(pool: &PgPool) {
    let row = sqlx::query(
        "SELECT current_user AS name, r.rolsuper, r.rolcreatedb, r.rolcreaterole,
                r.rolinherit, r.rolbypassrls,
                NOT EXISTS (SELECT 1 FROM pg_auth_members m WHERE m.member=r.oid) AS no_memberships,
                NOT EXISTS (SELECT 1 FROM pg_database d WHERE d.datdba=r.oid)
                  AND NOT EXISTS (SELECT 1 FROM pg_namespace n WHERE n.nspowner=r.oid)
                  AND NOT EXISTS (SELECT 1 FROM pg_class c WHERE c.relowner=r.oid)
                  AND NOT EXISTS (SELECT 1 FROM pg_proc p WHERE p.proowner=r.oid) AS owns_no_objects,
                NOT has_database_privilege(current_user,current_database(),'CREATE,TEMPORARY') AS cannot_create,
                NOT EXISTS (
                  SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                  WHERE n.nspname='public' AND c.relkind IN ('r','p')
                    AND has_table_privilege(current_user,c.oid,'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER')
                ) AS cannot_access_tables,
                NOT EXISTS (
                  SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
                  WHERE n.nspname='public'
                    AND p.oid <> ALL (ARRAY[
                      'process_forget_purge(text,text,bigint)'::regprocedure,
                      'claim_embedding_jobs(integer,integer)'::regprocedure,
                      'complete_embedding_job(text,text,bigint,text)'::regprocedure,
                      'fail_embedding_job(text,text,bigint,text)'::regprocedure,
                      'cancel_embedding_job(text,text,bigint)'::regprocedure
                    ])
                    AND has_function_privilege(current_user,p.oid,'EXECUTE')
                ) AS cannot_execute_other_functions,
                has_function_privilege(current_user,'process_forget_purge(text,text,bigint)','EXECUTE')
                  AND has_function_privilege(current_user,'claim_embedding_jobs(integer,integer)','EXECUTE')
                  AND has_function_privilege(current_user,'complete_embedding_job(text,text,bigint,text)','EXECUTE')
                  AND has_function_privilege(current_user,'fail_embedding_job(text,text,bigint,text)','EXECUTE')
                  AND has_function_privilege(current_user,'cancel_embedding_job(text,text,bigint)','EXECUTE')
                  AND NOT has_function_privilege(current_user,'resolve_current_writer(text)','EXECUTE')
                  AND NOT has_function_privilege(current_user,'forget_private_memory(text,text,text,bytea,bytea,text,text)','EXECUTE')
                  AND NOT has_function_privilege(current_user,'record_request_rejection(text,text,text)','EXECUTE') AS worker_only
         FROM pg_roles r WHERE r.rolname=current_user",
    )
    .fetch_one(pool)
    .await
    .expect("inspect purge worker role");
    assert_eq!(
        row.try_get::<String, _>("name").expect("role name"),
        "agentic_memory_purge_worker"
    );
    for field in [
        "rolsuper",
        "rolcreatedb",
        "rolcreaterole",
        "rolinherit",
        "rolbypassrls",
    ] {
        assert!(
            !row.try_get::<bool, _>(field).expect("role flag"),
            "unsafe worker flag: {field}"
        );
    }
    for field in [
        "no_memberships",
        "owns_no_objects",
        "cannot_create",
        "cannot_access_tables",
        "cannot_execute_other_functions",
        "worker_only",
    ] {
        assert!(
            row.try_get::<bool, _>(field).expect("worker restriction"),
            "missing worker restriction: {field}"
        );
    }
}

async fn assert_lock_rejects_unbound_tenant(pool: &PgPool, bearer: &str) {
    let mut transaction = pool.begin().await.expect("begin rejected lock");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest', encode($1::bytea, 'hex'), true)",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set Alice credential digest");
    set_local(&mut transaction, "app.operation", "read").await;
    set_local(&mut transaction, "app.tenant_id", BETA_TENANT).await;
    let rejected = sqlx::query("SELECT lock_tenant_authority($1)")
        .bind(BETA_TENANT)
        .execute(&mut *transaction)
        .await;
    assert!(
        rejected.is_err(),
        "lock must require matching authenticated credential context"
    );
    transaction
        .rollback()
        .await
        .expect("rollback rejected lock transaction");
    let direct_read = sqlx::query(
        "SELECT * FROM read_current_item(
           '00000000000000000000000000000001',
           'c0000000000000000000000000000001',
           '00000000-0000-0000-0000-000000000097',
           '40000000000000000000000000000001', NULL, NULL)",
    )
    .fetch_all(pool)
    .await;
    assert!(
        direct_read.is_err(),
        "direct read invocation without request-local context must fail"
    );
}

async fn set_local(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
    value: &str,
) {
    sqlx::query_scalar::<_, String>("SELECT set_config($1, $2, true)")
        .bind(key)
        .bind(value)
        .fetch_one(&mut **transaction)
        .await
        .expect("set transaction-local test context");
}

async fn request(app: &axum::Router, bearer: &str, item_id: &str) -> (StatusCode, Value) {
    request_with_auth(app, Some(bearer), item_id).await
}

async fn request_expected(
    app: &axum::Router,
    bearer: &str,
    item_id: &str,
    expected_revision_id: &str,
) -> (StatusCode, Value) {
    request_raw(
        app,
        Some(bearer),
        &format!("/v1/items/{item_id}?expected_revision_id={expected_revision_id}"),
        &[],
    )
    .await
}

async fn assert_read_rejection_audit(pool: &PgPool, request_id: &str, outcome: &str) {
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM read_audit
             WHERE request_id=$1 AND operation='read' AND outcome=$2
               AND target_id IS NULL AND tenant_id=$3
               AND principal_id='10000000000000000000000000000001'
               AND app_id='a0000000000000000000000000000001'
               AND credential_id=$4",
        )
        .bind(request_id)
        .bind(outcome)
        .bind(ALPHA_TENANT)
        .bind(ALICE_CREDENTIAL)
        .fetch_one(pool)
        .await
        .expect("read request-specific sanitized audit"),
        1
    );
}

fn assert_sanitized_unavailable(response: &(StatusCode, Value)) {
    assert_eq!(response.0, StatusCode::NOT_FOUND);
    let mut body = response.1.clone();
    body.as_object_mut()
        .expect("unavailable response object")
        .remove("request_id");
    assert_eq!(
        body,
        serde_json::json!({"status": "unavailable", "code": "unavailable"})
    );
}

async fn request_with_auth(
    app: &axum::Router,
    bearer: Option<&str>,
    item_id: &str,
) -> (StatusCode, Value) {
    request_raw(app, bearer, &format!("/v1/items/{item_id}"), &[]).await
}

async fn request_raw(
    app: &axum::Router,
    bearer: Option<&str>,
    uri: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    request_body(app, bearer, uri, Body::from(body.to_vec())).await
}

async fn request_body(
    app: &axum::Router,
    bearer: Option<&str>,
    uri: &str,
    body: Body,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().uri(uri);
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("valid request"))
        .await
        .expect("router response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded response body");
    (
        status,
        serde_json::from_slice(&body).expect("JSON response"),
    )
}

async fn post_memory(
    app: &axum::Router,
    bearer: &str,
    idempotency_key: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    post_memory_with_auth(app, Some(bearer), Some(idempotency_key), body).await
}

async fn delete_memory(
    app: &axum::Router,
    bearer: &str,
    idempotency_key: &str,
    item_id: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    delete_memory_request(
        app,
        Some(bearer),
        Some(idempotency_key),
        &format!("/v1/items/{item_id}"),
        Some("application/json"),
        body,
    )
    .await
}

async fn delete_memory_request(
    app: &axum::Router,
    bearer: Option<&str>,
    idempotency_key: Option<&str>,
    uri: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("DELETE").uri(uri);
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    if let Some(idempotency_key) = idempotency_key {
        builder = builder.header("idempotency-key", idempotency_key);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("valid forget request"),
        )
        .await
        .expect("forget response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded forget response");
    (
        status,
        serde_json::from_slice(&body).expect("JSON forget response"),
    )
}

async fn put_memory(
    app: &axum::Router,
    bearer: &str,
    idempotency_key: &str,
    item_id: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    put_memory_request(
        app,
        Some(bearer),
        Some(idempotency_key),
        &format!("/v1/items/{item_id}"),
        Some("application/json"),
        body,
    )
    .await
}

async fn put_memory_request(
    app: &axum::Router,
    bearer: Option<&str>,
    key: Option<&str>,
    uri: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("PUT").uri(uri);
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("valid correction request"),
        )
        .await
        .expect("correction response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded correction response");
    (
        status,
        serde_json::from_slice(&body).expect("JSON correction response"),
    )
}

async fn search_json(app: &axum::Router, bearer: Option<&str>, query: &str) -> (StatusCode, Value) {
    search(
        app,
        bearer,
        serde_json::json!({ "query": query }).to_string().as_bytes(),
    )
    .await
}

async fn get_json(app: &axum::Router, bearer: Option<&str>, path: &str) -> (StatusCode, Value) {
    get_json_body(app, bearer, path, &[]).await
}

async fn get_json_body(
    app: &axum::Router,
    bearer: Option<&str>,
    path: &str,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("valid list request"),
        )
        .await
        .expect("list router response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 32 * 1024)
        .await
        .expect("bounded list response");
    (
        status,
        serde_json::from_slice(&body).expect("JSON list response"),
    )
}

async fn search(app: &axum::Router, bearer: Option<&str>, body: &[u8]) -> (StatusCode, Value) {
    search_with_content_type(app, bearer, Some("application/json"), body).await
}

async fn search_with_content_type(
    app: &axum::Router,
    bearer: Option<&str>,
    content_type: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri("/v1/search");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("valid search request"),
        )
        .await
        .expect("search router response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 512 * 1024)
        .await
        .expect("bounded search response");
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("JSON search response")
    };
    (status, body)
}

async fn post_memory_with_auth(
    app: &axum::Router,
    bearer: Option<&str>,
    idempotency_key: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    post_memory_with_headers(app, bearer, idempotency_key, Some("application/json"), body).await
}

async fn post_memory_with_headers(
    app: &axum::Router,
    bearer: Option<&str>,
    idempotency_key: Option<&str>,
    content_type: Option<&str>,
    body: &[u8],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri("/v1/memories");
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    if let Some(idempotency_key) = idempotency_key {
        builder = builder.header("idempotency-key", idempotency_key);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("valid create request"),
        )
        .await
        .expect("create router response");
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded create response");
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).expect("JSON create response")
    };
    (status, body)
}

enum AdversarialBody {
    Pending,
    ByteThenPending { sent: bool },
    BodyError,
}

impl HttpBody for AdversarialBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            Self::ByteThenPending { sent } if !*sent => {
                *sent = true;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"X")))))
            }
            Self::Pending | Self::ByteThenPending { .. } => Poll::Pending,
            Self::BodyError => {
                Poll::Ready(Some(Err(std::io::Error::other("synthetic body failure"))))
            }
        }
    }
}

fn request_id(body: &Value) -> &str {
    body["request_id"].as_str().expect("request ID")
}

#[path = "benchmarks/retrieval.rs"]
mod retrieval_benchmark;

#[path = "benchmarks/office_retrieval.rs"]
mod office_retrieval_benchmark;
