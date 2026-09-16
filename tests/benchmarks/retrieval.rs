use super::*;
use serde::Serialize;
use std::time::Instant;

const BUDGET: usize = 4096;
const CYCLES: usize = 64;
const PREFIX: &str = "BENCHMARK SENTINEL ";

#[derive(Clone, Serialize)]
struct Evidence {
    item_id: String,
    revision_id: String,
    content: String,
}

struct Fact {
    name: &'static str,
    evidence: Evidence,
    subjects: Vec<&'static str>,
    current_allowed: bool,
}

struct Case {
    id: &'static str,
    query: &'static str,
    subjects: Vec<&'static str>,
    required: Vec<&'static str>,
    category: &'static str,
}

#[derive(Debug, PartialEq, Serialize)]
struct Score {
    matched: usize,
    required: usize,
    returned: usize,
    full_support: bool,
    forbidden: usize,
    context_bytes: usize,
}

fn same_revision(a: &Evidence, b: &Evidence) -> bool {
    a.item_id == b.item_id && a.revision_id == b.revision_id
}

fn score(found: &[Evidence], required: &[Evidence], eligible: &[Evidence]) -> Score {
    let matched = required
        .iter()
        .filter(|need| {
            found
                .iter()
                .any(|got| same_revision(need, got) && got.content.contains(&need.content))
        })
        .count();
    Score {
        matched,
        required: required.len(),
        returned: found.len(),
        full_support: !required.is_empty() && matched == required.len(),
        forbidden: found
            .iter()
            .filter(|got| {
                !eligible.iter().any(|allowed| {
                    same_revision(allowed, got) && allowed.content.starts_with(&got.content)
                })
            })
            .count(),
        context_bytes: found.iter().map(|got| got.content.len()).sum(),
    }
}

fn percentile(samples: &[f64], percent: usize) -> f64 {
    assert!(!samples.is_empty() && (1..=100).contains(&percent));
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    ordered[(ordered.len() * percent).div_ceil(100) - 1]
}

#[test]
fn scoring_sanity_checks() {
    let first = Evidence {
        item_id: "i1".into(),
        revision_id: "r1".into(),
        content: "owner is Mina".into(),
    };
    let second = Evidence {
        item_id: "i2".into(),
        revision_id: "r2".into(),
        content: "review is annual".into(),
    };
    let required = [first.clone(), second];
    assert_eq!(score(&[], &required, &required).matched, 0);
    assert!(!score(&[], &[], &required).full_support);
    assert_eq!(
        score(std::slice::from_ref(&first), &required, &required).matched,
        1
    );
    assert!(score(&required, &required, &required).full_support);
    for changed in [
        Evidence {
            revision_id: "old".into(),
            ..first.clone()
        },
        Evidence {
            item_id: "other".into(),
            ..first.clone()
        },
        Evidence {
            content: "owner is".into(),
            ..first.clone()
        },
    ] {
        assert_eq!(
            score(&[changed], std::slice::from_ref(&first), &required).matched,
            0
        );
    }
    assert_eq!(
        score(
            &[first.clone(), first.clone()],
            std::slice::from_ref(&first),
            &required
        )
        .matched,
        1
    );
    assert_eq!(score(std::slice::from_ref(&first), &[], &[]).forbidden, 1);
    assert_eq!(score(&[first], &required, &required).context_bytes, 13);
    assert!((percentile(&[4.0, 1.0, 3.0, 2.0], 50) - 2.0).abs() < f64::EPSILON);
    assert!((percentile(&[4.0, 1.0, 3.0, 2.0], 95) - 4.0).abs() < f64::EPSILON);
}

async fn save_fact(
    app: &axum::Router,
    writer: &str,
    name: &'static str,
    content: &str,
    subjects: Vec<&'static str>,
    validity: Option<(&str, &str)>,
) -> Fact {
    let mut input =
        serde_json::json!({"content":format!("{PREFIX}{content}"), "subjects":subjects});
    if let Some((from, until)) = validity {
        input["valid_from"] = from.into();
        input["valid_until"] = until.into();
    }
    let (status, receipt) = post_memory(
        app,
        writer,
        &format!("pilot-create-{name}"),
        input.to_string().as_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fixture create {name}");
    Fact {
        name,
        evidence: Evidence {
            item_id: receipt["item_id"].as_str().expect("created item").into(),
            revision_id: receipt["revision_id"]
                .as_str()
                .expect("created revision")
                .into(),
            content: input["content"].as_str().expect("fixture content").into(),
        },
        subjects: subjects
            .into_iter()
            .filter(|subject| *subject != ALICE_SUBJECT)
            .collect(),
        current_allowed: validity.is_none(),
    }
}

fn decode_evidence(body: &Value) -> Option<Vec<Evidence>> {
    body["items"]
        .as_array()?
        .iter()
        .map(|item| {
            Some(Evidence {
                item_id: item["item_id"].as_str()?.into(),
                revision_id: item["revision_id"].as_str()?.into(),
                content: item["excerpt"].as_str()?.into(),
            })
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut result, byte| {
            write!(result, "{byte:02x}").expect("hex string");
            result
        })
}

pub(super) fn tool_version(command: &str, args: &[&str]) -> String {
    let result = std::process::Command::new(command)
        .args(args)
        .output()
        .expect("manifest command");
    assert!(result.status.success(), "manifest command failed");
    String::from_utf8(result.stdout)
        .expect("manifest text")
        .trim()
        .to_owned()
}

fn write_report(value: &Value) {
    std::fs::create_dir_all("target").expect("benchmark output directory");
    std::fs::write(
        "target/retrieval-benchmark.json",
        serde_json::to_vec_pretty(value).expect("report JSON"),
    )
    .expect("write benchmark report");
}

#[tokio::test]
#[ignore = "resets the synthetic database; run just benchmark separately"]
#[allow(clippy::too_many_lines)] // One sequential experiment keeps fixture transitions and measurement order explicit.
async fn before_after() {
    // Invalidate the old artifact first, including if setup fails before the final report.
    write_report(&serde_json::json!({"status":"running_or_interrupted"}));
    let setup_start = Instant::now();
    let (migrator, runtime, worker, reader, bob, writer) = setup().await;
    let ingest = router(runtime.clone());
    let mut facts = Vec::new();
    for (name, content) in [
        ("amber", "Amber orchid policy requires quarterly review"),
        ("cobalt", "Cobalt lantern service owner is Mina"),
        ("jade", "Jade river renewal occurs in November"),
        ("golden", "Golden pine support contact is Rhea"),
        ("hazel", "Hazel comet region is Singapore"),
        ("indigo", "Indigo bridge requires two approvers"),
        ("navy", "Navy garden export format is CSV"),
        ("delta", "Delta harbor retention is thirty days"),
    ] {
        facts.push(save_fact(&ingest, &writer, name, content, vec![ALICE_SUBJECT], None).await);
    }
    let old_revision = facts[0].evidence.clone();
    let correction = serde_json::json!({"expected_revision_id":facts[0].evidence.revision_id,"content":format!("{PREFIX}Amber orchid policy requires annual review"),"subjects":[ALICE_SUBJECT]});
    let (status, receipt) = put_memory(
        &ingest,
        &writer,
        "pilot-correction",
        &facts[0].evidence.item_id,
        correction.to_string().as_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    facts[0].evidence.revision_id = receipt["revision_id"]
        .as_str()
        .expect("corrected revision")
        .into();
    facts[0].evidence.content = correction["content"]
        .as_str()
        .expect("corrected content")
        .into();
    let forgotten = facts.last_mut().expect("delta fixture");
    let body = serde_json::json!({"expected_revision_id":forgotten.evidence.revision_id});
    assert_eq!(
        delete_memory(
            &ingest,
            &writer,
            "pilot-forget-delta-01",
            &forgotten.evidence.item_id,
            body.to_string().as_bytes()
        )
        .await
        .0,
        StatusCode::OK
    );
    forgotten.current_allowed = false;

    sqlx::query("INSERT INTO subjects (tenant_id,id,app_id,kind) VALUES ($1,'pilot_customer','a0000000000000000000000000000001','customer'),($1,'pilot_project','a0000000000000000000000000000001','project'),($1,'pilot_wrong_customer','a0000000000000000000000000000001','customer')")
        .bind(ALPHA_TENANT).execute(&migrator).await.expect("seed synthetic scope identities");
    facts.push(
        save_fact(
            &ingest,
            &writer,
            "zephyr",
            "Zephyr project owner is Nia",
            vec!["pilot_customer", "pilot_project"],
            None,
        )
        .await,
    );
    for (name, content, validity) in [
        (
            "future",
            "Future temporal launch is Friday",
            ("2099-01-01T00:00:00Z", "2100-01-01T00:00:00Z"),
        ),
        (
            "expired",
            "Expired temporal discount is ten percent",
            ("2020-01-01T00:00:00Z", "2020-01-02T00:00:00Z"),
        ),
    ] {
        facts.push(
            save_fact(
                &ingest,
                &writer,
                name,
                content,
                vec![ALICE_SUBJECT],
                Some(validity),
            )
            .await,
        );
    }
    // Seeded records must be indexed before they can demonstrate search isolation.
    sqlx::query("INSERT INTO lexical_representations (tenant_id,item_id,revision_id,document) SELECT tenant_id,item_id,id,to_tsvector('simple',content) FROM revisions WHERE item_id=ANY($1)")
        .bind(vec![ALLOWED_ITEM, BOB_PRIVATE_ITEM, FOREIGN_ITEM]).execute(&migrator).await.expect("index seeded controls");
    let mut controls = Vec::new();
    for (id, token, query) in [
        (ALLOWED_ITEM, &reader, "ALLOWED_ALPHA_HANDBOOK"),
        (BOB_PRIVATE_ITEM, &bob, "FORBIDDEN_BOB_PRIVATE"),
    ] {
        let (status, body) = search_json(&ingest, Some(token), query).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["items"][0]["item_id"], id,
            "authorized searchable positive control"
        );
        controls
            .push(serde_json::json!({"control":query,"status":status.as_u16(),"matched_item":id}));
    }
    let foreign_matches: i64 = sqlx::query_scalar("SELECT count(*) FROM lexical_representations WHERE item_id=$1 AND document @@ websearch_to_tsquery('simple',$2)")
        .bind(FOREIGN_ITEM).bind("FORBIDDEN_BETA_COMPANY").fetch_one(&migrator).await.expect("foreign lexical positive control");
    assert_eq!(foreign_matches, 1);
    controls.push(serde_json::json!({"control":"foreign_record_matches_query_in_privileged_fixture","matches":foreign_matches}));

    let mut transition = migrator
        .begin()
        .await
        .expect("withdrawal fixture transaction");
    sqlx::query("SELECT tenant_id FROM tenant_authority WHERE tenant_id=$1 FOR UPDATE")
        .bind(ALPHA_TENANT)
        .execute(&mut *transition)
        .await
        .expect("lock fixture authority");
    sqlx::query("UPDATE collections SET withdrawn_at=clock_timestamp() WHERE tenant_id=$1 AND id='30000000000000000000000000000001'").bind(ALPHA_TENANT).execute(&mut *transition).await.expect("withdraw positive source");
    sqlx::query("UPDATE tenant_authority SET authority_epoch=authority_epoch+1 WHERE tenant_id=$1")
        .bind(ALPHA_TENANT)
        .execute(&mut *transition)
        .await
        .expect("advance fixture authority");
    transition.commit().await.expect("commit withdrawal");
    for (name, id, revision, content) in [
        (
            "withdrawn",
            ALLOWED_ITEM,
            "50000000000000000000000000000001",
            "ALLOWED_ALPHA_HANDBOOK",
        ),
        (
            "bob",
            BOB_PRIVATE_ITEM,
            "50000000000000000000000000000002",
            "FORBIDDEN_BOB_PRIVATE",
        ),
        (
            "foreign",
            FOREIGN_ITEM,
            "50000000000000000000000000000003",
            "FORBIDDEN_BETA_COMPANY",
        ),
    ] {
        facts.push(Fact {
            name,
            evidence: Evidence {
                item_id: id.into(),
                revision_id: revision.into(),
                content: content.into(),
            },
            subjects: vec![],
            current_allowed: false,
        });
    }
    revoke_alice_credential(&migrator).await;
    let (status, body) = search_json(&ingest, Some(&reader), "amber orchid").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthenticated");
    controls.push(serde_json::json!({"control":"revoked_reader","status":status.as_u16()}));
    restore_alice_credential(&migrator).await;
    drop(ingest);
    runtime.close().await;
    worker.close().await;
    let preparation_ms = setup_start.elapsed().as_secs_f64() * 1000.0;

    // Required labels remain evaluator-only. API requests contain query, scope and budget only.
    let cases = vec![
        Case {
            id: "corrected",
            query: "amber orchid",
            subjects: vec![],
            required: vec!["amber"],
            category: "lexical",
        },
        Case {
            id: "owner",
            query: "cobalt lantern",
            subjects: vec![],
            required: vec!["cobalt"],
            category: "lexical",
        },
        Case {
            id: "renewal",
            query: "jade river",
            subjects: vec![],
            required: vec!["jade"],
            category: "lexical",
        },
        Case {
            id: "contact",
            query: "golden pine",
            subjects: vec![],
            required: vec!["golden"],
            category: "lexical",
        },
        Case {
            id: "region",
            query: "hazel comet",
            subjects: vec![],
            required: vec!["hazel"],
            category: "lexical",
        },
        Case {
            id: "approvers",
            query: "indigo bridge",
            subjects: vec![],
            required: vec!["indigo"],
            category: "lexical",
        },
        Case {
            id: "export",
            query: "navy garden",
            subjects: vec![],
            required: vec!["navy"],
            category: "lexical",
        },
        Case {
            id: "two_facts",
            query: "\"cobalt lantern\" OR \"jade river\"",
            subjects: vec![],
            required: vec!["cobalt", "jade"],
            category: "lexical",
        },
        Case {
            id: "scoped",
            query: "zephyr project",
            subjects: vec!["pilot_customer", "pilot_project"],
            required: vec!["zephyr"],
            category: "lexical",
        },
        Case {
            id: "omitted_scope",
            query: "zephyr project",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "partial_scope",
            query: "zephyr project",
            subjects: vec!["pilot_customer"],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "wrong_scope",
            query: "zephyr project",
            subjects: vec!["pilot_wrong_customer", "pilot_project"],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "forgotten",
            query: "delta harbor",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "future",
            query: "future temporal",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "expired",
            query: "expired temporal",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "withdrawn",
            query: "ALLOWED_ALPHA_HANDBOOK",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "other_user",
            query: "FORBIDDEN_BOB_PRIVATE",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "other_company",
            query: "FORBIDDEN_BETA_COMPANY",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "missing",
            query: "unrecorded lunar invoice",
            subjects: vec![],
            required: vec![],
            category: "negative",
        },
        Case {
            id: "paraphrase_owner",
            query: "Who manages the blue light service?",
            subjects: vec![],
            required: vec!["cobalt"],
            category: "paraphrase",
        },
        Case {
            id: "paraphrase_renewal",
            query: "When should the river subscription be extended?",
            subjects: vec![],
            required: vec!["jade"],
            category: "paraphrase",
        },
    ];
    let fresh_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(RUNTIME_URL)
        .await
        .expect("fresh application pool");
    let app = router(fresh_pool.clone());
    let first_start = Instant::now();
    let (first_status, first_body) = search_json(&app, Some(&reader), cases[0].query).await;
    let first_ms = first_start.elapsed().as_secs_f64() * 1000.0;
    let first_found = decode_evidence(&first_body);
    let first_valid = first_status == StatusCode::OK
        && first_found.as_ref().is_some_and(|found| {
            let measured = score(
                found,
                std::slice::from_ref(&facts[0].evidence),
                std::slice::from_ref(&facts[0].evidence),
            );
            measured.full_support && measured.forbidden == 0
        });
    if !first_valid {
        write_report(
            &serde_json::json!({"status":"failed","phase":"first_search_new_pool","http_status":first_status.as_u16(),"elapsed_ms":first_ms,"malformed":first_found.is_none(),"harness_sha256":sha256(include_bytes!("retrieval.rs"))}),
        );
    }
    assert!(
        first_valid,
        "first search failed; inspect benchmark artifact"
    );

    let mut observations = Vec::new();
    let mut raw = Vec::new();
    let mut failures = Vec::new();
    let mut times = vec![Vec::new(); cases.len()];
    for cycle in 0..CYCLES {
        for position in 0..cases.len() {
            let index = (position + cycle) % cases.len();
            let case = &cases[index];
            let required: Vec<_> = case
                .required
                .iter()
                .map(|name| {
                    facts
                        .iter()
                        .find(|fact| fact.name == *name)
                        .expect("oracle fact")
                        .evidence
                        .clone()
                })
                .collect();
            // ponytail: one subject per kind in this fixture; extend the oracle for same-kind alternatives.
            let eligible: Vec<_> = facts
                .iter()
                .filter(|fact| {
                    fact.current_allowed
                        && fact
                            .subjects
                            .iter()
                            .all(|subject| case.subjects.contains(subject))
                })
                .map(|fact| fact.evidence.clone())
                .collect();
            let full = score(&eligible, &required, &eligible);
            assert!(
                full.context_bytes <= BUDGET,
                "full-context comparator must fit declared ceiling"
            );
            assert!(
                required.is_empty() || full.full_support,
                "oracle baseline must contain required facts"
            );
            let mut input = serde_json::json!({"query":case.query,"max_context_bytes":BUDGET});
            if !case.subjects.is_empty() {
                input["scope"] = serde_json::json!({"subjects":case.subjects});
            }
            let request_bytes = serde_json::to_vec(&input).expect("query JSON");
            let start = Instant::now();
            let (status, response) = search(&app, Some(&reader), &request_bytes).await;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            times[index].push(elapsed);
            let decoded = decode_evidence(&response);
            let malformed = decoded.is_none();
            let found = decoded.unwrap_or_default();
            let measured = score(&found, &required, &eligible);
            let serialized = response.to_string();
            let hidden_metadata = std::iter::once(&old_revision)
                .chain(
                    facts
                        .iter()
                        .filter(|fact| {
                            !eligible
                                .iter()
                                .any(|allowed| same_revision(&fact.evidence, allowed))
                        })
                        .map(|fact| &fact.evidence),
                )
                .any(|hidden| {
                    serialized.contains(&hidden.revision_id)
                        || serialized.contains(&hidden.content)
                        || (!eligible
                            .iter()
                            .any(|allowed| allowed.item_id == hidden.item_id)
                            && serialized.contains(&hidden.item_id))
                });
            let bad = status != StatusCode::OK
                || malformed
                || measured.forbidden > 0
                || hidden_metadata
                || measured.context_bytes > BUDGET
                || response["context_bytes"].as_u64() != u64::try_from(measured.context_bytes).ok()
                || (case.category == "negative" && !found.is_empty())
                || (case.category == "lexical" && !measured.full_support);
            if bad {
                failures.push(serde_json::json!({"case_id":case.id,"cycle":cycle,"status":status.as_u16(),"malformed":malformed,"forbidden":measured.forbidden,"hidden_metadata":hidden_metadata}));
            }
            raw.push(serde_json::json!({"case_id":case.id,"cycle":cycle,"position":position,"status":status.as_u16(),"elapsed_ms":elapsed,"score":measured,"hidden_metadata":hidden_metadata,"failed":bad,"truncated":response["truncated"]}));
            if cycle == 0 {
                observations.push(serde_json::json!({"case_id":case.id,"category":case.category,"query":case.query,"scope_subjects":case.subjects,"required_evidence":required,"returned_evidence":found,"status":status.as_u16(),"search":measured,"no_memory":score(&[],&required,&eligible),"full_current_authorized":full,"full_current_evidence":eligible}));
            }
        }
    }
    let mut aggregates = serde_json::Map::new();
    for variant in ["no_memory", "search", "full_current_authorized"] {
        let answerable: Vec<_> = observations
            .iter()
            .filter(|case| case[variant]["required"].as_u64().expect("required count") > 0)
            .collect();
        let expected: u64 = answerable
            .iter()
            .map(|case| case[variant]["required"].as_u64().expect("required count"))
            .sum();
        let matched: u64 = answerable
            .iter()
            .map(|case| case[variant]["matched"].as_u64().expect("matched count"))
            .sum();
        let bytes: u64 = answerable
            .iter()
            .map(|case| case[variant]["context_bytes"].as_u64().expect("byte count"))
            .sum();
        let full = answerable
            .iter()
            .filter(|case| case[variant]["full_support"] == true)
            .count();
        aggregates.insert(variant.into(),serde_json::json!({"answerable_cases":answerable.len(),"required_evidence":expected,"matched_evidence":matched,"fully_supported_cases":full,"context_bytes_answerable_sum":bytes}));
    }
    let all_times: Vec<_> = times.iter().flatten().copied().collect();
    let per_case: Vec<_> = cases.iter().zip(&times).map(|(case,samples)|serde_json::json!({"case_id":case.id,"samples":samples.len(),"p50_ms":percentile(samples,50),"p95_ms":percentile(samples,95)})).collect();
    let negatives: Vec<_> = observations
        .iter()
        .filter(|case| case["category"] == "negative")
        .collect();
    let report = serde_json::json!({
        "status":if failures.is_empty(){"passed"}else{"failed"},
        "manifest":{"harness_sha256":sha256(include_bytes!("retrieval.rs")),"core_sha256":sha256(include_bytes!("../../src/lib.rs")),"migration_sha256":sha256(include_bytes!("../../migrations/0001_read_slice.sql")),"cargo_lock_sha256":sha256(include_bytes!("../../Cargo.lock")),"fixture_sha256":sha256(include_bytes!("../fixtures/reset.sql")),"git_head":tool_version("git",&["rev-parse","HEAD"]),"rustc":tool_version("rustc",&["--version"]),"cargo":tool_version("cargo",&["--version"]),"host":tool_version("uname",&["-sm"]),"profile":if cfg!(debug_assertions){"debug"}else{"release"},"unix_seconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_secs(),"dataset":"hand-authored development pilot; harness hash pins corpus, queries and oracle; no held-out quality claim","transport":"in-process HTTP router; real local PostgreSQL; no TCP HTTP transport","clients":1,"cycles":CYCLES,"search_budget_bytes":BUDGET},
        "preparation_ms":preparation_ms,"first_search_new_pool_ms":first_ms,"first_search_status":first_status.as_u16(),"first_search_note":"database caches may be warm; pool connection already established; not machine cold start",
        "quality":aggregates,"negative_controls":{"cases":negatives.len(),"empty":negatives.iter().filter(|case|case["search"]["returned"]==0).count()},"fixture_controls":controls,
        "latency":{"samples":all_times.len(),"p50_ms":percentile(&all_times,50),"p95_ms":percentile(&all_times,95),"per_case":per_case,"raw":raw},
        "cases":observations,"gate_failures":failures,
        "limits":"Evidence availability, not model answer accuracy. Bytes exclude metadata/framing and are not tokens. Paraphrase misses count in overall recall. Full current authorized comparator is oracle-assisted, not basic RAG. Repeats are latency observations, not independent quality cases."
    });
    let encoded = report.to_string();
    assert!(
        [&reader, &bob, &writer]
            .iter()
            .all(|secret| !encoded.contains(secret.as_str())),
        "artifact must not contain credentials"
    );
    write_report(&report);
    println!(
        "retrieval pilot: {} cases, {} operations; evidence {}/{}; full support {}/{}; exclusions {}/{}; p50 {:.3} ms, p95 {:.3} ms",
        cases.len(),
        all_times.len(),
        report["quality"]["search"]["matched_evidence"],
        report["quality"]["search"]["required_evidence"],
        report["quality"]["search"]["fully_supported_cases"],
        report["quality"]["search"]["answerable_cases"],
        report["negative_controls"]["empty"],
        report["negative_controls"]["cases"],
        percentile(&all_times, 50),
        percentile(&all_times, 95)
    );
    fresh_pool.close().await;
    migrator.close().await;
    assert!(
        failures.is_empty(),
        "benchmark failed; inspect target/retrieval-benchmark.json"
    );
}
