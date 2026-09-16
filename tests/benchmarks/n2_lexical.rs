const N2_QUERY_SEMANTICS: &str = "bounded-question-v1";
const N2_SOURCE_REPRESENTATION: &str = "source-structure-v1";

fn n2_passage(key: &str, content: &str, locator: Value) -> Passage {
    let source_sha256 = digest(format!("{key}:{content}:{locator}").as_bytes());
    Passage {
        key: key.to_owned(),
        file_id: format!("n2-{key}"),
        format: "txt_md",
        source_revision: format!("sha256:{source_sha256}"),
        source_revision_id: source_sha256.clone(),
        source_sha256,
        extraction_set_id: format!("n2-set-{key}"),
        passage_id: "passage-0001".to_owned(),
        structural_parent: format!("n2-parent-{key}"),
        passage_order: 1,
        continuation_direction: "none",
        locator,
        text: content.to_owned(),
        item_id: format!("n2-item-{key}"),
        revision_id: format!("n2-revision-{key}"),
    }
}

#[tokio::test]
#[ignore = "resets the synthetic database; run the focused N2 controls sequentially"]
#[allow(clippy::too_many_lines)] // Keep the single reset-based public query contract together.
async fn n2_public_search_prepares_questions_without_changing_explicit_operators() {
    let (migrator, runtime, worker, alice, _bob, _writer) = setup().await;
    let long_query = (1..=33)
        .map(|index| format!("term{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let (prepared, unchanged): (String, String) = sqlx::query_as(
        "SELECT lexical_query_bounded_question_v1($1)::text,
                websearch_to_tsquery('simple',$1)::text",
    )
    .bind(long_query)
    .fetch_one(&migrator)
    .await
    .expect("compare over-bound query semantics");
    assert_eq!(prepared, unchanged);
    let corpus = vec![
        n2_passage(
            "refund",
            "Refund policy allows returns for thirty days.",
            serde_json::json!({"format":"txt_md","logical":"refund"}),
        ),
        n2_passage(
            "expired",
            "Refund policy for expired subscriptions is unavailable.",
            serde_json::json!({"format":"txt_md","logical":"expired"}),
        ),
        n2_passage(
            "warranty",
            "Warranty coverage lasts one year.",
            serde_json::json!({"format":"txt_md","logical":"warranty"}),
        ),
        n2_passage(
            "identifier",
            "SR-2048 status on 2026-09-07 is closed.",
            serde_json::json!({"format":"txt_md","logical":"identifier"}),
        ),
        n2_passage(
            "project",
            "Project status is green.",
            serde_json::json!({"format":"txt_md","logical":"project"}),
        ),
        n2_passage(
            "who",
            "Policy publication is complete.",
            serde_json::json!({"format":"txt_md","logical":"who"}),
        ),
        n2_passage(
            "native-question",
            "What is refund policy guidance.",
            serde_json::json!({"format":"txt_md","logical":"native-question"}),
        ),
        n2_passage(
            "operator-the",
            "The policy governs exceptions.",
            serde_json::json!({"format":"txt_md","logical":"operator-the"}),
        ),
    ];
    for passage in &corpus {
        activate_office_file(&migrator, &[passage]).await;
    }
    runtime.close().await;
    let runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(RUNTIME_URL)
        .await
        .expect("fresh N2 reader pool");
    let app = router(runtime.clone());

    for (query, expected) in [
        (
            "What is the refund policy?",
            vec!["refund", "expired", "native-question"],
        ),
        ("What is the status of SR-2048 on 2026-09-07?", vec!["identifier"]),
        ("What is the SR-2048 status?", vec!["identifier"]),
        (
            "\"refund policy\"",
            vec!["refund", "expired", "native-question"],
        ),
        (
            "refund OR warranty",
            vec!["refund", "expired", "warranty", "native-question"],
        ),
        ("refund -expired", vec!["refund", "native-question"]),
        (
            "refund or the warranty",
            vec!["refund", "expired", "native-question"],
        ),
        (
            "refund - the policy",
            vec!["refund", "expired", "native-question"],
        ),
        ("What is refund (-the) policy", vec!["native-question"]),
        ("What is refund (- the) policy", vec!["native-question"]),
        (
            "What is refund OR(the policy)",
            vec!["native-question", "operator-the"],
        ),
        (
            "What is refund or:the policy",
            vec!["native-question", "operator-the"],
        ),
    ] {
        let (status, response) = search_json(&app, Some(&alice), query).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        let returned = validated_search(&response, &corpus)
            .returned
            .iter()
            .map(|passage| passage.key.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(returned, expected.into_iter().collect(), "{query}");
    }
    let (status, response) = search_json(&app, Some(&alice), "what is the?").await;
    assert_eq!(status, StatusCode::OK);
    assert!(response["items"].as_array().expect("items").is_empty());
    for exact in [
        "Project A status",
        "WHO policy",
        "What is Project A status?",
        "What is WHO policy?",
    ] {
        let (status, response) = search_json(&app, Some(&alice), exact).await;
        assert_eq!(status, StatusCode::OK);
        assert!(response["items"].as_array().expect("items").is_empty(), "{exact}");
    }
    assert_eq!(N2_QUERY_SEMANTICS, "bounded-question-v1");

    runtime.close().await;
    worker.close().await;
    migrator.close().await;
}

#[tokio::test]
#[ignore = "resets the synthetic database; run the focused N2 controls sequentially"]
#[allow(clippy::too_many_lines)] // Keep the single reset-based public representation contract together.
async fn n2_indexes_bounded_source_structure_but_returns_only_original_evidence() {
    let (migrator, runtime, worker, alice, _bob, _writer) = setup().await;
    let passage = n2_passage(
        "structure",
        "Thirty days",
        serde_json::json!({
            "format":"xlsx",
            "logical":"sheet1-row2",
            "title":"Returns workbook",
            "heading":"Refund Policy",
            "table_headers":["Return window","Approval owner"],
            "parser_note":"Internal parser note"
        }),
    );
    activate_office_file(&migrator, &[&passage]).await;
    let (body_only, enriched): (bool, bool) = sqlx::query_as(
        "SELECT to_tsvector('simple',content) @@ websearch_to_tsquery('simple','\"Refund Policy\"'),
                search_document @@ websearch_to_tsquery('simple','\"Refund Policy\"')
         FROM source_passages WHERE tenant_id=$1 AND item_id=$2",
    )
    .bind(ALPHA_TENANT)
    .bind(&passage.item_id)
    .fetch_one(&migrator)
    .await
    .expect("compare body-only and source-structure representations");
    assert!(!body_only);
    assert!(enriched);
    for phrase in ["\"Returns workbook\"", "\"Refund Policy\"", "\"Return window\"", "\"Approval owner\""] {
        let matched: bool = sqlx::query_scalar(
            "SELECT search_document @@ websearch_to_tsquery('simple',$1)
             FROM source_passages WHERE tenant_id=$2 AND item_id=$3",
        )
        .bind(phrase)
        .bind(ALPHA_TENANT)
        .bind(&passage.item_id)
        .fetch_one(&migrator)
        .await
        .expect("check within-field phrase");
        assert!(matched, "{phrase}");
    }
    for phrase in ["\"days Returns\"", "\"workbook Refund\"", "\"Policy Return\"", "\"window Approval\""] {
        let matched: bool = sqlx::query_scalar(
            "SELECT search_document @@ websearch_to_tsquery('simple',$1)
             FROM source_passages WHERE tenant_id=$2 AND item_id=$3",
        )
        .bind(phrase)
        .bind(ALPHA_TENANT)
        .bind(&passage.item_id)
        .fetch_one(&migrator)
        .await
        .expect("check cross-field phrase boundary");
        assert!(!matched, "{phrase}");
    }
    let recipe: String = sqlx::query_scalar(
        "SELECT search_recipe_version FROM extraction_sets
         WHERE tenant_id=$1 AND item_id=$2 AND id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(&passage.item_id)
    .bind(&passage.extraction_set_id)
    .fetch_one(&migrator)
    .await
    .expect("read stored search recipe");
    assert_eq!(recipe, N2_SOURCE_REPRESENTATION);
    sqlx::query(
        "INSERT INTO extraction_sets
           (tenant_id,item_id,revision_id,source_revision_id,id,parser_id,parser_version,
            config_version,search_recipe_version,passage_count,content_bytes)
         SELECT tenant_id,item_id,revision_id,source_revision_id,'n2-legacy-body',
                parser_id,parser_version,config_version,'body-v0',passage_count,content_bytes
         FROM extraction_sets WHERE tenant_id=$1 AND item_id=$2 AND id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(&passage.item_id)
    .bind(&passage.extraction_set_id)
    .execute(&migrator)
    .await
    .expect("body-only and enriched recipe identities may coexist");
    let duplicate = sqlx::query(
        "INSERT INTO extraction_sets
           (tenant_id,item_id,revision_id,source_revision_id,id,parser_id,parser_version,
            config_version,search_recipe_version,passage_count,content_bytes)
         SELECT tenant_id,item_id,revision_id,source_revision_id,'n2-duplicate-recipe',
                parser_id,parser_version,config_version,search_recipe_version,
                passage_count,content_bytes
         FROM extraction_sets WHERE tenant_id=$1 AND item_id=$2 AND id=$3",
    )
    .bind(ALPHA_TENANT)
    .bind(&passage.item_id)
    .bind(&passage.extraction_set_id)
    .execute(&migrator)
    .await
    .expect_err("same pipeline and search recipe must not create a duplicate set");
    assert_eq!(
        duplicate
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("23505")
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT extraction_set_id FROM active_extraction_sets
             WHERE tenant_id=$1 AND item_id=$2 AND revision_id=$3",
        )
        .bind(ALPHA_TENANT)
        .bind(&passage.item_id)
        .bind(&passage.revision_id)
        .fetch_one(&migrator)
        .await
        .expect("recipe identity failure leaves active set unchanged"),
        passage.extraction_set_id
    );
    runtime.close().await;
    let runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(RUNTIME_URL)
        .await
        .expect("fresh N2 reader pool");
    let app = router(runtime.clone());
    let (status, response) = search_json(&app, Some(&alice), "\"Refund Policy\"").await;
    assert_eq!(status, StatusCode::OK);
    let corpus = [passage.clone()];
    let result = validated_search(&response, &corpus);
    assert_eq!(result.returned.len(), 1);
    assert_eq!(result.returned[0].text, passage.text);
    for phrase in [
        "\"Returns workbook\"",
        "\"Refund Policy\"",
        "\"Return window\"",
        "\"Approval owner\"",
    ] {
        let (status, response) = search_json(&app, Some(&alice), phrase).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(validated_search(&response, &corpus).returned.len(), 1, "{phrase}");
    }
    for phrase in [
        "\"days Returns\"",
        "\"workbook Refund\"",
        "\"Policy Return\"",
        "\"window Approval\"",
    ] {
        let (status, response) = search_json(&app, Some(&alice), phrase).await;
        assert_eq!(status, StatusCode::OK);
        assert!(response["items"].as_array().expect("items").is_empty(), "{phrase}");
    }
    let (status, response) = search_json(&app, Some(&alice), "Internal parser note").await;
    assert_eq!(status, StatusCode::OK);
    assert!(response["items"].as_array().expect("items").is_empty());
    assert_eq!(N2_SOURCE_REPRESENTATION, "source-structure-v1");
    for locator in [
        serde_json::json!({"title":"x".repeat(256),"heading":"y".repeat(512),"table_headers":vec!["z".repeat(64);32]}),
        serde_json::json!({"title":"x".repeat(257)}),
        serde_json::json!({"heading":"x".repeat(513)}),
        serde_json::json!({"table_headers":["x".repeat(257)]}),
        serde_json::json!({"table_headers":vec!["x".repeat(65);32]}),
        serde_json::json!({"table_headers":vec!["x";33]}),
        serde_json::json!({"title":" "}),
        serde_json::json!({"heading":7}),
        serde_json::json!({"table_headers":"header"}),
    ] {
        let result = sqlx::query_scalar::<_, String>(
            "SELECT source_search_document_v1('body',$1::jsonb)::text",
        )
        .bind(locator.to_string())
        .fetch_one(&migrator)
        .await;
        if locator["title"].as_str().is_some_and(|value| value.len() == 256) {
            assert!(result.is_ok(), "exact structural bounds must pass");
        } else {
            let error = result.expect_err("invalid structural field must fail");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::code)
                    .as_deref(),
                Some("22023")
            );
        }
    }
    for signature in [
        "source_search_document_v1(text,jsonb)",
        "lexical_query_bounded_question_v1(text)",
    ] {
        let executable: bool = sqlx::query_scalar(
            "SELECT has_function_privilege('agentic_memory_runtime',$1,'EXECUTE')",
        )
        .bind(signature)
        .fetch_one(&migrator)
        .await
        .expect("inspect helper privilege");
        assert!(!executable, "runtime must not call internal helper {signature}");
    }

    runtime.close().await;
    worker.close().await;
    migrator.close().await;
}

async fn n2_raw_candidates<'a>(
    migrator: &PgPool,
    query: &str,
    corpus: &'a [Passage],
) -> Vec<&'a Passage> {
    let rows = sqlx::query(
        "SELECT p.item_id,p.revision_id,p.source_revision_id,p.extraction_set_id,p.id AS passage_id
         FROM source_passages p
         JOIN active_extraction_sets active
           ON active.tenant_id=p.tenant_id AND active.item_id=p.item_id
          AND active.revision_id=p.revision_id
          AND active.source_revision_id=p.source_revision_id
          AND active.extraction_set_id=p.extraction_set_id
         JOIN items i ON i.tenant_id=p.tenant_id AND i.id=p.item_id
         JOIN collections c ON c.tenant_id=i.tenant_id AND c.id=i.collection_id
         WHERE p.tenant_id=$1 AND c.withdrawn_at IS NULL AND i.deleted_at IS NULL
           AND i.active_revision_id=p.revision_id
           AND p.search_document @@ websearch_to_tsquery('simple',$2)",
    )
    .bind(ALPHA_TENANT)
    .bind(query)
    .fetch_all(migrator)
    .await
    .expect("fetch unchanged FTS control candidates");
    rows.iter()
        .map(|row| {
            corpus
                .iter()
                .find(|passage| {
                    row.get::<String, _>("item_id") == passage.item_id
                        && row.get::<String, _>("revision_id") == passage.revision_id
                        && row.get::<String, _>("source_revision_id")
                            == passage.source_revision_id
                        && row.get::<String, _>("extraction_set_id")
                            == passage.extraction_set_id
                        && row.get::<String, _>("passage_id") == passage.passage_id
                })
                .expect("raw control candidate has exact admitted identity")
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
async fn run_n2_split(split: &str) -> Value {
    assert!(matches!(split, "development" | "held_out"));
    let manifest = n1_manifest();
    let families = manifest["families"]
        .as_array()
        .expect("N1 families")
        .iter()
        .filter(|family| family["split"] == split)
        .collect::<Vec<_>>();
    let corpus = manifest["families"]
        .as_array()
        .expect("N1 families")
        .iter()
        .flat_map(|family| {
            family["sources"]
                .as_array()
                .expect("family sources")
                .iter()
                .map(move |source| n1_source_passage(family, source))
        })
        .collect::<Vec<_>>();
    let (migrator, runtime, worker, alice, _bob, _writer) = setup().await;
    let documents = corpus.iter().fold(BTreeMap::new(), |mut documents, passage| {
        documents
            .entry((
                passage.item_id.as_str(),
                passage.revision_id.as_str(),
                passage.source_revision_id.as_str(),
                passage.extraction_set_id.as_str(),
            ))
            .or_insert_with(Vec::new)
            .push(passage);
        documents
    });
    for passages in documents.values() {
        activate_office_file(&migrator, passages).await;
    }
    runtime.close().await;
    let runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(RUNTIME_URL)
        .await
        .expect("fresh N2 reader pool");
    let app = router(runtime.clone());
    let mut cases = Vec::new();
    for family in families {
        let alternatives = n1_alternative_sets(family, &corpus);
        for (wording_index, wording) in family["variants"]
            .as_array()
            .expect("query variants")
            .iter()
            .enumerate()
        {
            let wording = wording.as_str().expect("query wording");
            let raw = n2_raw_candidates(&migrator, wording, &corpus).await;
            let prepared = n1_direct_candidates(&runtime, &alice, wording, &corpus).await;
            let (status, response) = search_json(&app, Some(&alice), wording).await;
            assert_eq!(status, StatusCode::OK);
            let packed = validated_search(&response, &corpus);
            let raw_score = n1_best_score(
                &alternatives,
                &raw,
                raw.iter().map(|passage| passage.text.len()).sum(),
                &corpus,
            );
            let prepared_score = n1_best_score(
                &alternatives,
                &prepared,
                prepared.iter().map(|passage| passage.text.len()).sum(),
                &corpus,
            );
            let packed_score =
                n1_best_score(&alternatives, &packed.returned, packed.context_bytes, &corpus);
            cases.push(serde_json::json!({
                "family_id":family["id"],
                "wording_index":wording_index,
                "answerability":family["answerability"],
                "unchanged_fts_candidate_complete":raw_score.full_support,
                "prepared_candidate_complete":prepared_score.full_support,
                "prepared_packed_complete":packed_score.full_support,
                "prepared_returned_evidence":packed.returned.iter().map(|passage|passage.key.as_str()).collect::<Vec<_>>(),
                "context_bytes":packed.context_bytes,
                "citation_valid":packed_score.citation_valid == packed_score.citation_total,
            }));
        }
    }
    let answerable = cases
        .iter()
        .filter(|case| case["answerability"] == "answerable")
        .count();
    let insufficient = cases.len() - answerable;
    let count_true = |field: &str| {
        cases
            .iter()
            .filter(|case| case["answerability"] == "answerable" && case[field] == true)
            .count()
    };
    let report = serde_json::json!({
        "status":"measured",
        "split":split,
        "manifest_sha256":N1_MANIFEST_SHA256,
        "query_semantics":N2_QUERY_SEMANTICS,
        "source_representation":N2_SOURCE_REPRESENTATION,
        "settings_locked":true,
        "summary":{
            "variants":cases.len(),
            "answerable":answerable,
            "insufficient":insufficient,
            "unchanged_fts_candidate_complete":count_true("unchanged_fts_candidate_complete"),
            "prepared_candidate_complete":count_true("prepared_candidate_complete"),
            "prepared_packed_complete":count_true("prepared_packed_complete"),
            "correct_insufficient":cases.iter().filter(|case| case["answerability"] == "insufficient" && case["prepared_returned_evidence"].as_array().is_some_and(Vec::is_empty)).count(),
            "all_citations_valid":cases.iter().all(|case| case["citation_valid"] == true),
            "max_context_bytes":cases.iter().filter_map(|case|case["context_bytes"].as_u64()).max().unwrap_or(0)
        },
        "cases":cases,
        "limits":"Synthetic, no-model paired candidate/packed comparison. The unchanged control uses PostgreSQL websearch_to_tsquery over the same active N1 source passages; the retained public path additionally applies bounded-question-v1. Authorization remains enforced and separately regression-tested by the full suite."
    });
    std::fs::create_dir_all("target").expect("benchmark output directory");
    std::fs::write(
        format!("target/n2-lexical-{split}.json"),
        serde_json::to_vec_pretty(&report).expect("N2 report JSON"),
    )
    .expect("write N2 report");
    runtime.close().await;
    worker.close().await;
    migrator.close().await;
    report
}

#[tokio::test]
#[ignore = "resets the synthetic database and runs one frozen N2 split"]
async fn n2_lexical_evaluation() {
    let split = std::env::var("N2_BENCH_SPLIT").expect("N2_BENCH_SPLIT is required");
    let report = run_n2_split(&split).await;
    println!("N2 {split}: {}", report["summary"]);
}
