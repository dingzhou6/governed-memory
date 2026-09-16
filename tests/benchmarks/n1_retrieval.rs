const N1_MANIFEST_BYTES: &[u8] = include_bytes!("../fixtures/n1-retrieval.json");
const N1_BASELINE_HEAD: &str = "e40dbe3b5b708ab323f4f616c36aeed8d9a3dcf9";
const N1_MANIFEST_SHA256: &str = "d996ad2eda7afb8ca0b898fdebe06518773cb875ac8045a51ea40abbef91091c";

fn n1_manifest() -> Value {
    serde_json::from_slice(N1_MANIFEST_BYTES).expect("valid N1 manifest")
}

fn n1_source_passage(family: &Value, source: &Value) -> Passage {
    let family_id = family["id"].as_str().expect("family id");
    let source_id = source["id"].as_str().expect("source id");
    let format = match source["format"].as_str().expect("source format") {
        "pdf" => "pdf",
        "docx" => "docx",
        "xlsx" => "xlsx",
        "csv" => "csv",
        "pptx" => "pptx",
        "txt_md" => "txt_md",
        _ => panic!("unsupported N1 source format"),
    };
    let locator = source["locator"].as_str().expect("source locator");
    let text = source["content"].as_str().expect("source content").to_owned();
    let document_id = source["document_id"].as_str();
    let source_sha256 = if let Some(document_id) = document_id {
        digest(
            &serde_json::to_vec(
                &family["sources"]
                    .as_array()
                    .expect("family sources")
                    .iter()
                    .filter(|candidate| candidate["document_id"] == document_id)
                    .map(|candidate| {
                        (
                            &candidate["format"],
                            &candidate["locator"],
                            &candidate["passage_order"],
                            &candidate["structural_parent"],
                            &candidate["continuation_direction"],
                            &candidate["content"],
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("canonical N1 logical document"),
        )
    } else {
        digest(
            &serde_json::to_vec(&(format, locator, &text)).expect("canonical N1 source content"),
        )
    };
    let identity = document_id.unwrap_or(source_id);
    let item_id = format!("n1-{family_id}-{identity}");
    let passage_order = source["passage_order"]
        .as_u64()
        .map_or(1, |value| usize::try_from(value).expect("passage order"));
    let continuation_direction = match source["continuation_direction"].as_str() {
        None | Some("none") => "none",
        Some("to_next") => "to_next",
        Some("from_previous") => "from_previous",
        Some("both") => "both",
        Some(_) => panic!("invalid continuation direction"),
    };
    Passage {
        key: source_id.to_owned(),
        file_id: item_id.clone(),
        format,
        source_revision: format!("sha256:{source_sha256}"),
        source_revision_id: source_sha256.clone(),
        source_sha256,
        extraction_set_id: format!("n1-set-{identity}"),
        passage_id: format!("passage-{passage_order:04}"),
        structural_parent: source["structural_parent"]
            .as_str()
            .map_or_else(|| format!("n1-parent-{source_id}"), ToOwned::to_owned),
        passage_order,
        continuation_direction,
        locator: serde_json::json!({"format":format,"logical":locator}),
        text,
        revision_id: format!("n1-revision-{identity}"),
        item_id,
    }
}

fn n1_expected(passage: &Passage) -> ExpectedEvidence {
    ExpectedEvidence {
        key: passage.key.clone(),
        format: passage.format,
        item_id: passage.item_id.clone(),
        revision_id: passage.revision_id.clone(),
        source_sha256: passage.source_sha256.clone(),
        source_revision: passage.source_revision.clone(),
        source_revision_id: passage.source_revision_id.clone(),
        extraction_set_id: passage.extraction_set_id.clone(),
        passage_id: passage.passage_id.clone(),
        locator: passage.locator.clone(),
        text: passage.text.clone(),
    }
}

fn n1_alternative_sets(family: &Value, corpus: &[Passage]) -> Vec<Vec<ExpectedEvidence>> {
    family["required_evidence_sets"]
        .as_array()
        .expect("required evidence alternatives")
        .iter()
        .map(|alternative| {
            alternative
                .as_array()
                .expect("evidence set")
                .iter()
                .map(|id| {
                    let id = id.as_str().expect("evidence id");
                    n1_expected(
                        corpus
                            .iter()
                            .find(|passage| passage.key == id)
                            .expect("independently labeled evidence exists"),
                    )
                })
                .collect()
        })
        .collect()
}

fn n1_best_score(
    alternatives: &[Vec<ExpectedEvidence>],
    supplied: &[&Passage],
    context_bytes: usize,
    corpus: &[Passage],
) -> VariantScore {
    if alternatives.is_empty() {
        return score(&[], supplied, context_bytes, corpus);
    }
    let mut result = alternatives
        .iter()
        .map(|required| score(required, supplied, context_bytes, corpus))
        .max_by_key(|result| {
            (
                result.full_support,
                result.matched,
                std::cmp::Reverse(result.required),
            )
        })
        .expect("nonempty alternatives");
    let acceptable = alternatives.iter().flatten().collect::<Vec<_>>();
    let acceptable_supplied = supplied
        .iter()
        .filter(|passage| {
            acceptable
                .iter()
                .any(|expected| evidence_matches(expected, passage))
        })
        .count();
    result.evidence_precision = ratio(acceptable_supplied, supplied.len());
    result
}

#[test]
#[allow(clippy::too_many_lines)] // The frozen manifest contract remains explicit and auditable.
fn n1_manifest_has_frozen_family_split_and_independent_labels() {
    let manifest = n1_manifest();
    assert_eq!(digest(N1_MANIFEST_BYTES), N1_MANIFEST_SHA256);
    assert_eq!(manifest["schema_version"], 2);
    assert_eq!(manifest["split_policy"], "scenario_family");
    assert_eq!(manifest["context_budget_bytes"], CONTEXT_BUDGET);
    assert_eq!(manifest["launch_languages"], serde_json::json!(["en", "zh-Hans"]));
    assert_eq!(
        manifest["unsupported_features"],
        serde_json::json!(["historical_recall", "company_group_policy"])
    );
    let families = manifest["families"].as_array().expect("N1 families");
    assert_eq!(families.len(), 40);
    assert_eq!(
        families
            .iter()
            .filter(|family| family["split"] == "development")
            .count(),
        20
    );
    assert_eq!(
        families
            .iter()
            .filter(|family| family["split"] == "held_out")
            .count(),
        20
    );
    assert!(
        families
            .iter()
            .filter(|family| family["answerability"] == "insufficient")
            .count()
            >= 8
    );
    let mut family_ids = BTreeSet::new();
    let mut evidence_ids = BTreeSet::new();
    let mut categories = BTreeSet::new();
    let mut variants = 0usize;
    for family in families {
        let family_id = family["id"].as_str().expect("family id");
        assert!(family_ids.insert(family_id));
        let wordings = family["variants"].as_array().expect("query wordings");
        assert!((2..=4).contains(&wordings.len()));
        variants += wordings.len();
        for category in family["categories"].as_array().expect("categories") {
            categories.insert(category.as_str().expect("category"));
        }
        let source_ids = family["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .map(|source| {
                let id = source["id"].as_str().expect("source id");
                assert!(evidence_ids.insert(id), "duplicate evidence label {id}");
                assert!(!source["content"].as_str().expect("source content").contains(id));
                id
            })
            .collect::<BTreeSet<_>>();
        let alternatives = family["required_evidence_sets"]
            .as_array()
            .expect("required alternatives");
        assert_eq!(
            alternatives.is_empty(),
            family["answerability"] == "insufficient"
        );
        for alternative in alternatives {
            assert!(!alternative.as_array().expect("alternative set").is_empty());
            for id in alternative.as_array().expect("alternative set") {
                assert!(source_ids.contains(id.as_str().expect("evidence id")));
            }
        }
    }
    assert_eq!(variants, 100);
    assert!(
        families
            .iter()
            .filter(|family| family["split"] == "development")
            .all(|family| family["variants"].as_array().expect("variants").len() == 2)
    );
    assert!(
        families
            .iter()
            .filter(|family| family["split"] == "held_out")
            .all(|family| family["variants"].as_array().expect("variants").len() == 3)
    );
    let development_text = families
        .iter()
        .filter(|family| family["split"] == "development")
        .flat_map(|family| {
            family["variants"]
                .as_array()
                .expect("variants")
                .iter()
                .map(Value::to_string)
                .chain(
                    family["sources"]
                        .as_array()
                        .expect("sources")
                        .iter()
                        .map(|source| source["content"].to_string()),
                )
        })
        .collect::<BTreeSet<_>>();
    assert!(
        families
            .iter()
            .filter(|family| family["split"] == "held_out")
            .flat_map(|family| {
                family["variants"]
                    .as_array()
                    .expect("variants")
                    .iter()
                    .map(Value::to_string)
                    .chain(
                        family["sources"]
                            .as_array()
                            .expect("sources")
                            .iter()
                            .map(|source| source["content"].to_string()),
                    )
            })
            .all(|text| !development_text.contains(&text)),
        "held-out queries and sources must be authored independently"
    );
    for required in [
        "natural_question",
        "semantic_paraphrase",
        "exact_code",
        "exact_date",
        "negation",
        "table_headers",
        "split_passages",
        "multi_source_comparison",
        "speaker_subject_ambiguity",
        "language_en",
        "language_zh_hans",
        "no_answer",
        "acceptable_alternative",
    ] {
        assert!(categories.contains(required), "missing N1 category {required}");
    }
    assert!(!categories.contains("historical_recall"));
    assert!(!categories.contains("company_group_policy"));
    let denials = manifest["denial_controls"].as_array().expect("denial controls");
    assert_eq!(denials.len(), 4);
    assert_eq!(
        denials
            .iter()
            .map(|control| control["kind"].as_str().expect("denial kind"))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["foreign_tenant", "private", "restricted", "stale_source"])
    );
    assert!(denials.iter().all(|control| {
        control["authorized"].is_string()
            && control["denied"].is_string()
            && (control["query"].is_string() || control["authorized_query"].is_string())
    }));
    for (scale, expected_hash) in [
        (
            Scale::Thousand,
            "000eb31c7ddde83c5986347f0a5673544df1822aca23aa4a0e972294e1fdab49",
        ),
        (
            Scale::TenThousand,
            "5008d5f0728b091cd2a617d82592b9a9b443a63a3d91d4f8d4b23510bfd15bbf",
        ),
    ] {
        let controls = queries(scale);
        assert_eq!(controls.len(), 13);
        assert_eq!(
            digest(
                &serde_json::to_vec(
                    &controls
                        .iter()
                        .map(|query| {
                            (
                                &query.id,
                                &query.text,
                                &query.category,
                                &query.answer,
                                &query.required,
                            )
                        })
                        .collect::<Vec<_>>()
                )
                .expect("office control query manifest")
            ),
            expected_hash
        );
    }
}

#[test]
fn n1_scorer_accepts_alternatives_and_rejects_mutated_evidence() {
    let manifest = n1_manifest();
    let family = &manifest["families"][7];
    let corpus = family["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .map(|source| n1_source_passage(family, source))
        .collect::<Vec<_>>();
    let alternatives = n1_alternative_sets(family, &corpus);
    assert_eq!(alternatives.len(), 2);
    let first = n1_best_score(&alternatives, &[&corpus[0]], corpus[0].text.len(), &corpus);
    let second = n1_best_score(&alternatives, &[&corpus[1]], corpus[1].text.len(), &corpus);
    assert!(first.full_support && second.full_support);
    let both = n1_best_score(
        &alternatives,
        &[&corpus[0], &corpus[1]],
        corpus.iter().map(|passage| passage.text.len()).sum(),
        &corpus,
    );
    assert!(both.full_support);
    assert!((both.evidence_precision - 1.0).abs() < f64::EPSILON);
    let mut changed = corpus[0].clone();
    changed.text.push_str(" evaluator mutation");
    let rejected = n1_best_score(&alternatives, &[&changed], changed.text.len(), &corpus);
    assert!(!rejected.full_support);
    assert_eq!(rejected.citation_valid, 0);
}

#[test]
fn n1_split_passages_share_one_reciprocal_logical_document() {
    let manifest = n1_manifest();
    for family_id in ["dev-11", "held-11"] {
        let family = manifest["families"]
            .as_array()
            .expect("families")
            .iter()
            .find(|family| family["id"] == family_id)
            .expect("split family");
        let passages = family["sources"]
            .as_array()
            .expect("split sources")
            .iter()
            .map(|source| n1_source_passage(family, source))
            .collect::<Vec<_>>();
        assert_eq!(passages.len(), 2);
        assert!(same_extraction(&passages[0], &passages[1]));
        assert_eq!(passages[0].source_sha256, passages[1].source_sha256);
        assert_eq!(passages[0].structural_parent, passages[1].structural_parent);
        assert_eq!((passages[0].passage_order, passages[1].passage_order), (1, 2));
        assert_eq!(passages[0].continuation_direction, "to_next");
        assert_eq!(passages[1].continuation_direction, "from_previous");
        assert!(
            continues_to_next(&passages[0])
                && continues_to_previous(&passages[1])
                && passages[0].passage_order + 1 == passages[1].passage_order
        );
    }
    for family_id in ["dev-12", "held-12"] {
        let family = manifest["families"]
            .as_array()
            .expect("families")
            .iter()
            .find(|family| family["id"] == family_id)
            .expect("multi-source family");
        let sources = family["sources"]
            .as_array()
            .expect("multi-source evidence")
            .iter()
            .map(|source| n1_source_passage(family, source))
            .collect::<Vec<_>>();
        assert_eq!(sources.len(), 2);
        assert!(!same_extraction(&sources[0], &sources[1]));
        assert_ne!(sources[0].item_id, sources[1].item_id);
        assert_ne!(sources[0].source_revision_id, sources[1].source_revision_id);
    }
}

async fn n1_visible(app: &axum::Router, bearer: &str, query: &str) -> bool {
    let (status, response) = search_json(app, Some(bearer), query).await;
    assert_eq!(status, StatusCode::OK);
    !response["items"]
        .as_array()
        .expect("search items")
        .is_empty()
}

async fn n1_direct_candidates<'a>(
    runtime: &PgPool,
    bearer: &str,
    query: &str,
    corpus: &'a [Passage],
) -> Vec<&'a Passage> {
    static REQUEST_SEQUENCE: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(1);
    let sequence = REQUEST_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let request_id = format!("00000000-0000-4000-8000-{sequence:012}");
    let mut transaction = runtime.begin().await.expect("begin N1 candidate query");
    sqlx::query_scalar::<_, String>(
        "SELECT set_config('app.credential_digest',encode($1::bytea,'hex'),true)",
    )
    .bind(Sha256::digest(bearer.as_bytes()).as_slice())
    .fetch_one(&mut *transaction)
    .await
    .expect("set N1 candidate credential");
    sqlx::query_scalar::<_, String>("SELECT set_config('app.operation','search',true)")
        .fetch_one(&mut *transaction)
        .await
        .expect("set N1 candidate operation");
    sqlx::query_scalar::<_, String>("SELECT set_config('app.tenant_id',$1,true)")
        .bind(ALPHA_TENANT)
        .fetch_one(&mut *transaction)
        .await
        .expect("set N1 candidate tenant");
    let rows = sqlx::query(
        "SELECT item_id,revision_id,hit_kind,source_revision_id,extraction_set_id,passage_id
         FROM search_current_memories($1,$2,$3,$4,$5,NULL)",
    )
    .bind(ALPHA_TENANT)
    .bind(ALICE_CREDENTIAL)
    .bind(request_id)
    .bind(query)
    .bind(i32::try_from(CONTEXT_BUDGET).expect("bounded context budget"))
    .fetch_all(&mut *transaction)
    .await
    .expect("fetch N1 direct candidates");
    transaction.commit().await.expect("commit N1 candidate query");
    rows.iter()
        .filter(|row| row.get::<String, _>("hit_kind") == "passage")
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
                .expect("candidate has exact admitted identity")
        })
        .collect()
}

async fn n1_authority_controls(
    migrator: &PgPool,
    app: &axum::Router,
    alice: &str,
    bob: &str,
) -> Value {
    let beta = random_bearer();
    sqlx::query(
        "INSERT INTO credentials
         (tenant_id,id,principal_id,app_id,token_digest,credential_class,allowed_operations,issued_at,expires_at)
         VALUES ($1,'n1-beta-reader','20000000000000000000000000000001','a0000000000000000000000000000002',$2,'agent_reader',ARRAY['list','search','read'],clock_timestamp(),clock_timestamp()+interval '1 hour')",
    )
    .bind(BETA_TENANT)
    .bind(Sha256::digest(beta.as_bytes()).as_slice())
    .execute(migrator)
    .await
    .expect("seed N1 beta positive-control reader");
    sqlx::raw_sql(
        "INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document)
           SELECT tenant_id,item_id,id,to_tsvector('simple',content) FROM revisions
           ON CONFLICT DO NOTHING;
         INSERT INTO collection_grants (tenant_id,collection_id,principal_id,can_read)
           VALUES ('00000000000000000000000000000002','30000000000000000000000000000003','20000000000000000000000000000001',true)",
    )
    .execute(migrator)
    .await
    .expect("make authority controls independently retrievable");
    sqlx::raw_sql(
        "INSERT INTO items (tenant_id,id,collection_id)
           VALUES ('00000000000000000000000000000001','n1-stale-item','30000000000000000000000000000001');
         INSERT INTO revisions (tenant_id,item_id,id,content) VALUES
           ('00000000000000000000000000000001','n1-stale-item','n1-stale-old','N1 STALE SOURCE OLD MARKER'),
           ('00000000000000000000000000000001','n1-stale-item','n1-stale-current','N1 STALE SOURCE CURRENT MARKER');
         UPDATE items SET active_revision_id='n1-stale-current'
           WHERE tenant_id='00000000000000000000000000000001' AND id='n1-stale-item';
         INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document) VALUES
           ('00000000000000000000000000000001','n1-stale-item','n1-stale-old',to_tsvector('simple','N1 STALE SOURCE OLD MARKER')),
           ('00000000000000000000000000000001','n1-stale-item','n1-stale-current',to_tsvector('simple','N1 STALE SOURCE CURRENT MARKER'))",
    )
    .execute(migrator)
    .await
    .expect("seed N1 stale/current public-search control");

    let outcomes = serde_json::json!({
        "restricted":{"authorized":n1_visible(app,alice,"ALLOWED_ALPHA_HANDBOOK").await,"denied":!n1_visible(app,bob,"ALLOWED_ALPHA_HANDBOOK").await},
        "private":{"authorized":n1_visible(app,bob,"FORBIDDEN_BOB_PRIVATE").await,"denied":!n1_visible(app,alice,"FORBIDDEN_BOB_PRIVATE").await},
        "foreign_tenant":{"authorized":n1_visible(app,&beta,"FORBIDDEN_BETA_COMPANY").await,"denied":!n1_visible(app,alice,"FORBIDDEN_BETA_COMPANY").await},
        "stale_source":{"authorized":n1_visible(app,alice,"N1 STALE SOURCE CURRENT MARKER").await,"denied":!n1_visible(app,alice,"N1 STALE SOURCE OLD MARKER").await}
    });
    assert!(
        outcomes
            .as_object()
            .expect("authority outcomes")
            .values()
            .all(|outcome| outcome["authorized"] == true && outcome["denied"] == true),
        "authority controls: {outcomes}"
    );
    outcomes
}

async fn n1_split_activation_controls(app: &axum::Router, alice: &str, corpus: &[Passage]) -> Value {
    let mut controls = serde_json::Map::new();
    for (family_id, query, evidence_ids) in [
        (
            "dev-11",
            "Helix exception",
            ["dev-11-a", "dev-11-b"],
        ),
        (
            "held-11",
            "Borealis disposal",
            ["held-11-a", "held-11-b"],
        ),
    ] {
        let (status, response) = search_json(app, Some(alice), query).await;
        assert_eq!(status, StatusCode::OK);
        let search = validated_search(&response, corpus);
        assert_eq!(search.direct.len(), 1, "{family_id} direct passage");
        assert_eq!(search.expansions.len(), 1, "{family_id} continuation");
        assert_eq!(
            search
                .returned
                .iter()
                .map(|passage| passage.key.as_str())
                .collect::<Vec<_>>(),
            evidence_ids
        );
        controls.insert(
            family_id.to_owned(),
            serde_json::json!({
                "same_logical_document":same_extraction(search.returned[0],search.returned[1]),
                "direct":search.direct[0].key,
                "reciprocal_continuation":search.expansions[0].key
            }),
        );
    }
    Value::Object(controls)
}

#[allow(clippy::too_many_lines)]
async fn run_n1_baseline() -> Value {
    let manifest = n1_manifest();
    let families = manifest["families"].as_array().expect("N1 families");
    let corpus = families
        .iter()
        .flat_map(|family| {
            family["sources"]
                .as_array()
                .expect("family sources")
                .iter()
                .map(move |source| n1_source_passage(family, source))
        })
        .collect::<Vec<_>>();
    let (migrator, runtime, worker, alice, bob, _writer) = setup().await;
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
        .expect("fresh N1 reader pool");
    let app = router(runtime.clone());
    let authority_controls = n1_authority_controls(&migrator, &app, &alice, &bob).await;
    let split_passage_controls = n1_split_activation_controls(&app, &alice, &corpus).await;
    let mut cases = Vec::new();
    let mut failure_counts = BTreeMap::from([
        ("missing_candidate_support", 0usize),
        ("incomplete_packed_support", 0),
        ("false_positive_no_answer", 0),
        ("irrelevant_evidence", 0),
        ("invalid_citation", 0),
        ("byte_budget_exceeded", 0),
        ("authorization_leak", 0),
    ]);
    for family in families {
        let alternatives = n1_alternative_sets(family, &corpus);
        for (wording_index, wording) in family["variants"]
            .as_array()
            .expect("query variants")
            .iter()
            .enumerate()
        {
            let wording = wording.as_str().expect("query wording");
            let direct_candidates = n1_direct_candidates(&runtime, &alice, wording, &corpus).await;
            let (status, response) = search(
                &app,
                Some(&alice),
                serde_json::json!({"query":wording,"max_context_bytes":CONTEXT_BUDGET})
                    .to_string()
                    .as_bytes(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let result = validated_search(&response, &corpus);
            let candidate_bytes = direct_candidates
                .iter()
                .map(|passage| passage.text.len())
                .sum();
            let candidate =
                n1_best_score(&alternatives, &direct_candidates, candidate_bytes, &corpus);
            let packed =
                n1_best_score(&alternatives, &result.returned, result.context_bytes, &corpus);
            let insufficient = family["answerability"] == "insufficient";
            let mut failures = Vec::new();
            if !insufficient && !candidate.full_support {
                failures.push("missing_candidate_support");
            }
            if !insufficient && !packed.full_support {
                failures.push("incomplete_packed_support");
            }
            if insufficient && !result.returned.is_empty() {
                failures.push("false_positive_no_answer");
            }
            if packed.evidence_precision < 1.0 {
                failures.push("irrelevant_evidence");
            }
            if packed.citation_valid != packed.citation_total {
                failures.push("invalid_citation");
            }
            if result.context_bytes > CONTEXT_BUDGET {
                failures.push("byte_budget_exceeded");
            }
            for failure in &failures {
                *failure_counts.entry(failure).or_default() += 1;
            }
            cases.push(serde_json::json!({
                "family_id":family["id"],
                "split":family["split"],
                "wording_index":wording_index,
                "query":wording,
                "answerability":family["answerability"],
                "categories":family["categories"],
                "candidate_support":candidate,
                "packed_complete_support":packed,
                "byte_budget":{"used":result.context_bytes,"limit":CONTEXT_BUDGET,"within_limit":result.context_bytes<=CONTEXT_BUDGET},
                "returned_evidence_ids":result.returned.iter().map(|passage|passage.key.as_str()).collect::<Vec<_>>(),
                "failures":failures
            }));
        }
    }
    let split_summary = ["development", "held_out"]
        .into_iter()
        .map(|split| {
            let selected = cases
                .iter()
                .filter(|case| case["split"] == split)
                .collect::<Vec<_>>();
            let answerable = selected
                .iter()
                .filter(|case| case["answerability"] == "answerable")
                .count();
            let candidate_complete = selected
                .iter()
                .filter(|case| {
                    case["answerability"] == "answerable"
                        && case["candidate_support"]["full_support"] == true
                })
                .count();
            let packed_complete = selected
                .iter()
                .filter(|case| {
                    case["answerability"] == "answerable"
                        && case["packed_complete_support"]["full_support"] == true
                })
                .count();
            let insufficient = selected
                .iter()
                .filter(|case| case["answerability"] == "insufficient")
                .count();
            let correct_insufficient = selected
                .iter()
                .filter(|case| {
                    case["answerability"] == "insufficient"
                        && case["returned_evidence_ids"]
                            .as_array()
                            .expect("evidence ids")
                            .is_empty()
                })
                .count();
            (
                split,
                serde_json::json!({"variants":selected.len(),"answerable_variants":answerable,"candidate_complete":candidate_complete,"packed_complete":packed_complete,"insufficient_variants":insufficient,"correct_insufficient":correct_insufficient}),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let report = serde_json::json!({
        "status":"baseline_recorded",
        "baseline":{"git_head":N1_BASELINE_HEAD,"runtime":"Rust/PostgreSQL unchanged FTS plus current automatic reciprocal continuation","synthetic_only":true,"hosted_ai":false},
        "manifest":{"schema_version":manifest["schema_version"],"sha256":N1_MANIFEST_SHA256,"families":families.len(),"variants":cases.len(),"development_families":20,"held_out_families":20,"insufficient_families":8,"context_budget_bytes":CONTEXT_BUDGET,"launch_languages":manifest["launch_languages"],"unsupported_features":manifest["unsupported_features"]},
        "scoring":{"acceptable_alternatives":"complete support selects the best independently labeled evidence set; precision counts the union of all independently acceptable evidence","candidate_support":"exact governed direct lexical rows returned by the same restricted public search database function before Rust packing and continuation expansion","packed_complete_support":"complete required evidence in final bounded HTTP response","citation_validity":"exact item/revision/source/set/passage/locator/text match against admitted source manifest"},
        "split_summary":split_summary,
        "failure_categories":failure_counts,
        "authority_controls":authority_controls,
        "split_passage_controls":split_passage_controls,
        "cases":cases,
        "limits":"Deterministic no-model baseline. Candidate support uses the restricted-role public search database function's bounded direct candidate window; packed support uses the final HTTP response. This is not an uncapped-match, answer-model quality, semantic retrieval, historical recall, Company Group policy, capacity, or production performance claim."
    });
    std::fs::create_dir_all("target").expect("benchmark output directory");
    std::fs::write(
        "target/n1-retrieval-baseline.json",
        serde_json::to_vec_pretty(&report).expect("N1 report JSON"),
    )
    .expect("write N1 report");
    runtime.close().await;
    worker.close().await;
    migrator.close().await;
    report
}

#[tokio::test]
#[ignore = "resets the synthetic database and runs the frozen 40-family N1 baseline; run just benchmark-n1"]
async fn n1_retrieval_baseline() {
    let report = run_n1_baseline().await;
    println!(
        "N1 baseline: {} families, {} variants",
        report["manifest"]["families"], report["manifest"]["variants"]
    );
}
