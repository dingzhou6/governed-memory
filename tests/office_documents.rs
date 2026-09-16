use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use calamine::{DataRef, Reader, SheetVisible, Xlsx, XlsxFormulaMetadata, open_workbook};
use governed_memory::router;
use ooxmlsdk::parts::{
    presentation_document::PresentationDocument, wordprocessing_document::WordprocessingDocument,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    io::Write as _,
    path::Path,
};
use tower::ServiceExt;

const MIGRATOR_URL: &str =
    "postgres://agentic_memory_migrator:synthetic-migrator-only@127.0.0.1:55432/agentic_memory";
const RUNTIME_URL: &str =
    "postgres://agentic_memory_runtime:synthetic-runtime-only@127.0.0.1:55432/agentic_memory";
const TENANT: &str = "00000000000000000000000000000001";
const SUBJECT: &str = "s0000000000000000000000000000001";
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_ROWS: usize = 100;
const MAX_COLUMNS: usize = 32;
const MAX_CELLS: usize = 1_024;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_SERIALIZED_OUTPUT_BYTES: usize = MAX_OUTPUT_BYTES + 4 * 1024;
const MAX_ZIP_ENTRIES: usize = 32;
const MAX_ZIP_ENTRY_BYTES: u64 = 128 * 1024;
const MAX_ZIP_EXPANDED_BYTES: u64 = 256 * 1024;
const MAX_SHARED_STRINGS: usize = 1_024;
const MAX_PDF_PAGES: usize = 16;
const MAX_OFFICE_ZIP_ENTRIES: usize = 64;
const MAX_OFFICE_ZIP_ENTRY_BYTES: u64 = 512 * 1024;
const MAX_OFFICE_ZIP_EXPANDED_BYTES: u64 = 2 * 1024 * 1024;
const MAX_OFFICE_BLOCKS: usize = 100;
const MAX_OFFICE_TABLE_ROWS: usize = 100;
// Sampled parent-side enforcement; production still needs a native sandbox ceiling.
const MAX_ISOLATED_PARSER_RSS_BYTES: u64 = 48 * 1024 * 1024;
const MEMORY_HOG_BYTES: usize = 80 * 1024 * 1024;
const MAX_OFFICE_TABLE_CELLS: usize = 1_024;
const ACTUAL_LARGE_PARSE_SAMPLES: usize = 5;
const GENERATED_CSV_RECORDS: usize = 70;
const GENERATED_XLSX_LAST_ROW: usize = 60;

#[derive(Clone, Copy, Serialize)]
struct ActualLargeOracle {
    format: &'static str,
    source_sha256: &'static str,
    passage_count: usize,
    passage_locator_text_bytes: usize,
    serialized_document_bytes: usize,
    locator: &'static str,
    text: &'static str,
    cells: &'static [FrozenLiteralCell],
}

#[derive(Clone, Copy, Serialize)]
struct FrozenLiteralCell {
    locator: &'static str,
    value_type: &'static str,
    value: &'static str,
}

const ACTUAL_LARGE_ORACLES: [ActualLargeOracle; 5] = [
    ActualLargeOracle {
        format: "pdf",
        source_sha256: "d2c6df8b46b1f2883be1fd4adf81e1867891ce573aa3ed4f113e42be9f413b44",
        passage_count: 16,
        passage_locator_text_bytes: 1_317,
        serialized_document_bytes: 2_099,
        locator: "page=16;block=1;type=paragraph;bbox=72.0,700.0,144.0,12.0",
        text: "ACTUAL PDF LATE MARIGOLD",
        cells: &[],
    },
    ActualLargeOracle {
        format: "docx",
        source_sha256: "d027217db37c6c7f54dd23c2024c1bf0dee92f79a23f9324325a837a4ee51e84",
        passage_count: 82,
        passage_locator_text_bytes: 8_093,
        serialized_document_bytes: 11_252,
        locator: "heading=Client renewal handbook/Escalation policy;paragraph=81",
        text: "ACTUAL DOCX LATE JUNIPER",
        cells: &[],
    },
    ActualLargeOracle {
        format: "xlsx",
        source_sha256: "df077fd1a82fb78112ae14ccfd2de5e29cbc1a79998042b934141624ea541e34",
        passage_count: 61,
        passage_locator_text_bytes: 6_121,
        serialized_document_bytes: 23_296,
        locator: "sheet=Rates;headers=A1:C1;cells=A60:C60",
        text: "Product: Product 060\nRegion: R060\nNote: ACTUAL XLSX LATE MULBERRY",
        cells: &[
            FrozenLiteralCell {
                locator: "Rates!A60",
                value_type: "string",
                value: "Product 060",
            },
            FrozenLiteralCell {
                locator: "Rates!B60",
                value_type: "string",
                value: "R060",
            },
            FrozenLiteralCell {
                locator: "Rates!C60",
                value_type: "string",
                value: "ACTUAL XLSX LATE MULBERRY",
            },
        ],
    },
    ActualLargeOracle {
        format: "csv",
        source_sha256: "77f4171c8bf2c3e2affcc73175bac496cf03aa210ddc8c446a0fbb4b3c9b7365",
        passage_count: 70,
        passage_locator_text_bytes: 6_973,
        serialized_document_bytes: 33_470,
        locator: "record=70;cells=A71:C71",
        text: "Account: Account 070\nOwner: Owner 070\nRenewal note: ACTUAL CSV LATE SAFFRON",
        cells: &[
            FrozenLiteralCell {
                locator: "record=70;column=A;header=\"Account\"",
                value_type: "string",
                value: "Account 070",
            },
            FrozenLiteralCell {
                locator: "record=70;column=B;header=\"Owner\"",
                value_type: "string",
                value: "Owner 070",
            },
            FrozenLiteralCell {
                locator: "record=70;column=C;header=\"Renewal note\"",
                value_type: "string",
                value: "ACTUAL CSV LATE SAFFRON",
            },
        ],
    },
    ActualLargeOracle {
        format: "pptx",
        source_sha256: "6e3856c39ac0f80432704a6ad877ed65b96a3dd2c9246160db6191a77a7b5b88",
        passage_count: 86,
        passage_locator_text_bytes: 5_200,
        serialized_document_bytes: 8_503,
        locator: "slide=2;shape-tree=82;shape-id=179",
        text: "ACTUAL PPTX LATE VERMILION",
        cells: &[],
    },
];

const COMPILED_OFFICE_DOCUMENTS_HARNESS: &[u8] = include_bytes!("office_documents.rs");

struct ActualLargeFixture {
    format: &'static str,
    extension: &'static str,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Document {
    source_sha256: String,
    source_revision: String,
    passages: Vec<Passage>,
    unsupported: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ExtractionEnvelope {
    document: Document,
    extraction_nanos: u64,
}

#[derive(Debug, Serialize)]
struct IsolatedExtraction {
    document: Document,
    extraction_nanos: u64,
    process_wall_nanos: u64,
    max_sampled_child_rss_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ProcessMetrics {
    process_wall_nanos: u64,
    max_sampled_child_rss_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Passage {
    locator: String,
    text: String,
    cells: Vec<CellEvidence>,
}

#[derive(Clone, Debug)]
struct SheetCell {
    value_type: String,
    value: String,
    formula: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum CellEvidence {
    Literal {
        locator: String,
        value_type: String,
        value: String,
    },
    Formula {
        locator: String,
        expression: String,
        cached_type: String,
        cached_value: String,
    },
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One end-to-end test keeps authored fixture evidence beside its public-flow checks.
async fn trusted_office_fixtures_are_searchable_with_exact_source_locators() {
    let documents = [
        isolated_extract("fixtures/office/control.txt").expect("isolated TXT extraction"),
        isolated_extract("fixtures/office/control.md").expect("isolated Markdown extraction"),
        isolated_extract("fixtures/office/renewals.csv").expect("isolated CSV extraction"),
        isolated_extract("fixtures/office/rates.xlsx").expect("isolated XLSX extraction"),
        isolated_extract("fixtures/office/brief.pdf").expect("isolated PDF extraction"),
        isolated_extract("fixtures/office/handbook.docx").expect("isolated DOCX extraction"),
        isolated_extract("fixtures/office/briefing.pptx").expect("isolated PPTX extraction"),
    ];

    assert_eq!(
        documents[0].source_sha256,
        "2b2f91bf990c9c05513db5f8177aebcf37f1f78c9d5156b0ac8ae6b0ffb909bf"
    );
    assert_eq!(
        documents[1].source_sha256,
        "06fc3f5e1d6c5ea1f1533aa5023b277aad569944b956b4378e5dc0a89a2a1231"
    );
    assert_eq!(
        documents[2].source_sha256,
        "5477a48d3171f8c60ce6041c318d45bffa2d8f2d4ed99697818c9124ca98db47"
    );
    assert_eq!(
        documents[3].source_sha256,
        "39275f9bf50c51f82b3afeb269865039be4bcf2337bb91e3c72d4c6d21d3c1bd"
    );
    assert_eq!(
        documents[4].source_sha256,
        "36d0bd2ef12cb8c4e2c9cc8c4bcfe2415d4c4ecbc1709fe49ae78e22e20eb88c"
    );
    assert_eq!(
        documents[5].source_sha256,
        "13976e311e1b20dd8d156c9adb40adb9d6183b86f6f2ee355ba4c440133023b8"
    );
    assert_eq!(
        documents[6].source_sha256,
        "856ca0acdf6d335e70992a3fb4d43a747af9c14d5c1eb2af4b3833a63835ce48"
    );
    assert_eq!(
        documents
            .iter()
            .map(|document| document.passages.len())
            .collect::<Vec<_>>(),
        [1, 1, 1, 4, 7, 2, 6]
    );
    assert_eq!(documents[0].passages[0].locator, "line=1-2");
    assert_eq!(
        documents[0].passages[0].text,
        "UNICODE CONTROL: naïve café 東京\nThe retention marker is AMBER LANTERN."
    );
    assert_eq!(documents[1].passages[0].locator, "line=1-3");
    assert_eq!(
        documents[1].passages[0].text,
        "# Travel policy\n\nThe approved rail marker is SILVER HERON."
    );
    assert_eq!(documents[2].passages[0].locator, "record=1;cells=A2:C2");
    assert_eq!(
        documents[2].passages[0].text,
        "Account: Acme, Ltd.\nOwner: Zoë\nRenewal note: Calls\nrenew in October"
    );
    assert_eq!(
        documents[2].passages[0].cells,
        [
            CellEvidence::Literal {
                locator: "record=1;column=A;header=\"Account\"".into(),
                value_type: "string".into(),
                value: "Acme, Ltd.".into(),
            },
            CellEvidence::Literal {
                locator: "record=1;column=B;header=\"Owner\"".into(),
                value_type: "string".into(),
                value: "Zoë".into(),
            },
            CellEvidence::Literal {
                locator: "record=1;column=C;header=\"Renewal note\"".into(),
                value_type: "string".into(),
                value: "Calls\nrenew in October".into(),
            },
        ]
    );
    assert_eq!(
        documents[3].passages[0].locator,
        "sheet=Rates;headers=A1:D1;cells=A2:D2"
    );
    assert_eq!(
        documents[3].passages[0].text,
        "Product: Core\nRegion: SG\nBase rate: 0.075\nAdjusted rate formula: =C2+0.005\nAdjusted rate cached float: 0.08"
    );
    assert_eq!(
        documents[3].passages[1].locator,
        "sheet=Rates;headers=A1:D1;cells=A3:D3"
    );
    assert_eq!(
        documents[3].passages[1].text,
        "Product: LAST ROW FALCON\nRegion: NZ\nBase rate: 0.125\nAdjusted rate formula: =C3+0.015\nAdjusted rate cached float: 0.14"
    );
    assert_eq!(
        documents[3].passages[2].locator,
        "sheet=Exceptions;headers=F19:G19;cells=F20:G20"
    );
    assert_eq!(
        documents[3].passages[2].text,
        "Region: APAC\nException: ORCHID WAIVER"
    );
    assert_eq!(
        documents[3].passages[3].locator,
        "sheet=Exceptions;headers=F19:G19;cells=F21:G21"
    );
    assert_eq!(
        documents[3].passages[3].text,
        "Region: EMEA\nException: LAST EXCEPTION KESTREL"
    );
    assert_eq!(
        documents[4]
            .passages
            .iter()
            .map(|passage| (passage.locator.as_str(), passage.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "page=1;block=1;type=title;bbox=247.5,696.0,117.0,18.0",
                "Renewal brief"
            ),
            (
                "page=1;block=2;type=paragraph;bbox=78.0,646.0,190.0,10.0",
                "The first-page marker is COPPER OTTER."
            ),
            (
                "page=1;block=3;type=key_value;bbox=78.0,616.0,230.0,10.0",
                "Unicode names: Zoe and cafe remain searchable."
            ),
            (
                "page=2;block=1;type=table;bbox=108.0,530.0,396.0,56.0",
                "Owner | Decision\nZoe | PDF TABLE OSPREY"
            ),
            (
                "page=2;block=2;type=title;bbox=240.5,696.0,126.0,18.0",
                "Approval notes"
            ),
            (
                "page=2;block=3;type=paragraph;bbox=78.0,646.0,180.0,10.0",
                "The late-page marker is VIOLET IBIS."
            ),
            (
                "page=2;block=4;type=paragraph;bbox=78.0,616.0,220.0,10.0",
                "Approve the synthetic renewal before Friday."
            ),
        ]
    );
    assert_eq!(
        documents[5]
            .passages
            .iter()
            .map(|passage| (passage.locator.as_str(), passage.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "heading=Client renewal handbook/Escalation policy;paragraph=1",
                "The Unicode owner is Zoë at the café. The paragraph marker is GOLDEN BADGER."
            ),
            (
                "heading=Client renewal handbook/Escalation policy;table=1;row=2;headers=A1:B1;cells=A2:B2",
                "Region: APAC\nEscalation: TABLE ROW KINGFISHER"
            ),
        ]
    );
    assert_eq!(
        documents[6]
            .passages
            .iter()
            .map(|passage| (passage.locator.as_str(), passage.text.as_str()))
            .collect::<Vec<_>>(),
        [
            ("slide=1;shape-tree=1;shape-id=4", "Quarterly briefing"),
            (
                "slide=1;shape-tree=2;shape-id=2",
                "Unicode team: Zoë and 東京"
            ),
            (
                "slide=1;shape-tree=3;shape-id=3",
                "The opening slide marker is SCARLET TERN."
            ),
            ("slide=2;shape-tree=1;shape-id=4", "Renewal actions"),
            (
                "slide=2;shape-tree=2;shape-id=2",
                "The last-slide marker is INDIGO PUFFIN."
            ),
            (
                "slide=2;shape-tree=3;graphic-id=6;table=1;headers=A1:B1;cells=A2:B2",
                "Owner: Mika\nAction: NATIVE TABLE ALBATROSS"
            ),
        ]
    );
    assert_eq!(
        documents[3].passages[0].cells,
        [
            CellEvidence::Literal {
                locator: "Rates!A2".into(),
                value_type: "string".into(),
                value: "Core".into(),
            },
            CellEvidence::Literal {
                locator: "Rates!B2".into(),
                value_type: "string".into(),
                value: "SG".into(),
            },
            CellEvidence::Literal {
                locator: "Rates!C2".into(),
                value_type: "float".into(),
                value: "0.075".into(),
            },
            CellEvidence::Formula {
                locator: "Rates!D2".into(),
                expression: "=C2+0.005".into(),
                cached_type: "float".into(),
                cached_value: "0.08".into(),
            },
        ]
    );
    assert_eq!(
        documents[3].passages[1].cells,
        [
            CellEvidence::Literal {
                locator: "Rates!A3".into(),
                value_type: "string".into(),
                value: "LAST ROW FALCON".into(),
            },
            CellEvidence::Literal {
                locator: "Rates!B3".into(),
                value_type: "string".into(),
                value: "NZ".into(),
            },
            CellEvidence::Literal {
                locator: "Rates!C3".into(),
                value_type: "float".into(),
                value: "0.125".into(),
            },
            CellEvidence::Formula {
                locator: "Rates!D3".into(),
                expression: "=C3+0.015".into(),
                cached_type: "float".into(),
                cached_value: "0.14".into(),
            },
        ]
    );
    assert_eq!(
        documents[3].passages[2].cells,
        [
            CellEvidence::Literal {
                locator: "Exceptions!F20".into(),
                value_type: "string".into(),
                value: "APAC".into(),
            },
            CellEvidence::Literal {
                locator: "Exceptions!G20".into(),
                value_type: "string".into(),
                value: "ORCHID WAIVER".into(),
            },
        ]
    );
    assert_eq!(
        documents[3].passages[3].cells,
        [
            CellEvidence::Literal {
                locator: "Exceptions!F21".into(),
                value_type: "string".into(),
                value: "EMEA".into(),
            },
            CellEvidence::Literal {
                locator: "Exceptions!G21".into(),
                value_type: "string".into(),
                value: "LAST EXCEPTION KESTREL".into(),
            },
        ]
    );
    assert!(
        documents[..2]
            .iter()
            .flat_map(|document| &document.passages)
            .all(|passage| passage.cells.is_empty())
    );
    assert!(
        documents
            .iter()
            .all(|document| document.unsupported.is_empty())
    );
    assert!(
        documents
            .iter()
            .all(|document| document.source_sha256.len() == 64
                && document.source_revision == format!("sha256:{}", document.source_sha256))
    );

    let (_migrator, runtime, reader, writer) = setup().await;
    let app = router(runtime);
    let passage_queries = [
        (0, 0, "AMBER LANTERN"),
        (1, 0, "SILVER HERON"),
        (2, 0, "renew in October"),
        (3, 0, "C2+0.005"),
        (3, 1, "LAST ROW FALCON"),
        (3, 2, "ORCHID WAIVER"),
        (3, 3, "LAST EXCEPTION KESTREL"),
        (4, 0, "Renewal brief"),
        (4, 1, "COPPER OTTER"),
        (4, 2, "Unicode names"),
        (4, 3, "PDF TABLE OSPREY"),
        (4, 4, "Approval notes"),
        (4, 5, "VIOLET IBIS"),
        (4, 6, "before Friday"),
        (5, 0, "GOLDEN BADGER"),
        (5, 1, "TABLE ROW KINGFISHER"),
        (6, 0, "Quarterly briefing"),
        (6, 1, "東京"),
        (6, 2, "SCARLET TERN"),
        (6, 3, "Renewal actions"),
        (6, 4, "INDIGO PUFFIN"),
        (6, 5, "NATIVE TABLE ALBATROSS"),
    ];
    assert_eq!(
        passage_queries.len(),
        documents
            .iter()
            .map(|document| document.passages.len())
            .sum::<usize>(),
        "every extracted passage requires a public-flow query"
    );
    for (index, (document_index, passage_index, query)) in passage_queries.into_iter().enumerate() {
        let document = &documents[document_index];
        let passage = &document.passages[passage_index];
        let content = normalized_passage_content(document, passage)
            .expect("bounded normalized passage content");
        let created = post_json(
            &app,
            &writer,
            &format!("office-document-pilot-{index}"),
            serde_json::json!({"content": content, "subjects": [SUBJECT]}),
        )
        .await;
        assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
        let item_id = created.1["item_id"].as_str().expect("created item ID");
        let revision_id = created.1["revision_id"]
            .as_str()
            .expect("created revision ID");

        let searched = search(&app, &reader, query).await;
        assert_eq!(searched.0, StatusCode::OK);
        let hit = searched.1["items"]
            .as_array()
            .expect("search results")
            .iter()
            .find(|hit| hit["item_id"] == item_id)
            .expect("matching public search hit");
        assert_eq!(hit["revision_id"], revision_id);
        let excerpt = hit["excerpt"].as_str().expect("search excerpt");
        assert!(excerpt.len() <= 512);
        assert!(excerpt.contains(&passage.locator));

        let read = get_json(
            &app,
            &reader,
            &format!("/v1/items/{item_id}?expected_revision_id={revision_id}"),
        )
        .await;
        assert_eq!(read.0, StatusCode::OK);
        assert_eq!(read.1["revision_id"], revision_id);
        assert_eq!(read.1["content"], content);
        let public_content = read.1["content"].as_str().expect("public exact content");
        if document_index == 2 {
            assert_eq!(
                public_cell_evidence(public_content),
                [
                    CellEvidence::Literal {
                        locator: "record=1;column=A;header=\"Account\"".into(),
                        value_type: "string".into(),
                        value: "Acme, Ltd.".into(),
                    },
                    CellEvidence::Literal {
                        locator: "record=1;column=B;header=\"Owner\"".into(),
                        value_type: "string".into(),
                        value: "Zoë".into(),
                    },
                    CellEvidence::Literal {
                        locator: "record=1;column=C;header=\"Renewal note\"".into(),
                        value_type: "string".into(),
                        value: "Calls\nrenew in October".into(),
                    },
                ]
            );
        } else if document_index == 3 {
            assert_eq!(public_cell_evidence(public_content), passage.cells);
        }
    }
}

fn public_cell_evidence(content: &str) -> Vec<CellEvidence> {
    serde_json::from_str(
        content
            .rsplit_once("\ncell_evidence: ")
            .expect("public structured cell evidence")
            .1,
    )
    .expect("valid public structured cell evidence")
}

fn normalized_passage_content(document: &Document, passage: &Passage) -> Result<String, String> {
    let mut content = format!(
        "source_sha256: {}\nsource_revision: {}\nlocator: {}\n{}",
        document.source_sha256, document.source_revision, passage.locator, passage.text
    );
    if !passage.cells.is_empty() {
        write!(
            content,
            "\ncell_evidence: {}",
            serde_json::to_string(&passage.cells).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    }
    if content.len() > MAX_OUTPUT_BYTES {
        return Err("normalized passage output limit exceeded".into());
    }
    Ok(content)
}

#[test]
fn csv_fields_preserve_exact_inert_evidence() {
    let document = extract_csv(
        "Name,Note,Formula\nZoë,\"Calls\nMonday\",=1+1\n".as_bytes(),
        "00",
    )
    .expect("extract CSV field evidence");

    assert_eq!(document.passages[0].locator, "record=1;cells=A2:C2");
    assert_eq!(
        document.passages[0].text,
        "Name: Zoë\nNote: Calls\nMonday\nFormula: =1+1"
    );
    assert_eq!(
        document.passages[0].cells,
        [
            CellEvidence::Literal {
                locator: "record=1;column=A;header=\"Name\"".into(),
                value_type: "string".into(),
                value: "Zoë".into(),
            },
            CellEvidence::Literal {
                locator: "record=1;column=B;header=\"Note\"".into(),
                value_type: "string".into(),
                value: "Calls\nMonday".into(),
            },
            CellEvidence::Literal {
                locator: "record=1;column=C;header=\"Formula\"".into(),
                value_type: "string".into(),
                value: "=1+1".into(),
            },
        ]
    );
}

#[test]
fn normalized_cell_evidence_cannot_bypass_the_public_content_limit() {
    let document = document("00".into(), vec![], vec![]);
    let passage = Passage {
        locator: "record=1;cells=A2:A2".into(),
        text: "x".repeat(MAX_OUTPUT_BYTES),
        cells: vec![CellEvidence::Literal {
            locator: "record=1;column=A;header=\"Name\"".into(),
            value_type: "string".into(),
            value: "bounded".into(),
        }],
    };

    assert_eq!(
        normalized_passage_content(&document, &passage).unwrap_err(),
        "normalized passage output limit exceeded"
    );
}

#[test]
fn actual_large_binary_generator_and_oracle_are_deterministic() {
    let first = actual_large_fixtures();
    let second = actual_large_fixtures();
    assert_eq!(first.len(), ACTUAL_LARGE_ORACLES.len());
    for ((fixture, repeated), oracle) in first.iter().zip(&second).zip(ACTUAL_LARGE_ORACLES) {
        assert_eq!(fixture.format, oracle.format);
        assert_eq!(fixture.format, repeated.format);
        assert_eq!(fixture.extension, repeated.extension);
        assert_eq!(fixture.bytes, repeated.bytes);
        assert!(!fixture.bytes.is_empty());
        assert!(fixture.bytes.len() <= MAX_SOURCE_BYTES);
        assert_eq!(hex_digest(&fixture.bytes), oracle.source_sha256);
        let path = temporary_office_path(
            "actual-large-sanity",
            fixture.extension,
            fixture.bytes.len(),
        );
        std::fs::write(&path, &fixture.bytes).expect("write sanity fixture");
        let document = extract_office_path(&path).expect("parse sanity fixture");
        std::fs::remove_file(path).expect("remove sanity fixture");
        assert!(actual_large_evidence_matches(&oracle, &document));

        let mut corrupted = document.clone();
        corrupted
            .passages
            .last_mut()
            .expect("late passage")
            .locator
            .push_str("-corrupt");
        assert!(!actual_large_evidence_matches(&oracle, &corrupted));
        let mut truncated = document.clone();
        truncated
            .passages
            .last_mut()
            .expect("late passage")
            .text
            .pop();
        assert!(!actual_large_evidence_matches(&oracle, &truncated));
        let mut missing = document.clone();
        missing.passages.pop();
        assert!(!actual_large_evidence_matches(&oracle, &missing));
        let mut wrong_revision = document.clone();
        wrong_revision.source_revision.push_str("-corrupt");
        assert!(!actual_large_evidence_matches(&oracle, &wrong_revision));
        if !oracle.cells.is_empty() {
            let mut changed_cell = document;
            let CellEvidence::Literal { value, .. } = changed_cell
                .passages
                .last_mut()
                .expect("late passage")
                .cells
                .last_mut()
                .expect("late cell")
            else {
                panic!("late benchmark cell must be literal")
            };
            value.push_str("-corrupt");
            assert!(!actual_large_evidence_matches(&oracle, &changed_cell));
        }
    }
    assert_eq!(
        hex_digest(COMPILED_OFFICE_DOCUMENTS_HARNESS),
        hex_digest(
            &std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/office_documents.rs"))
                .expect("read runtime harness")
        )
    );
}

#[test]
fn actual_parse_report_replaces_a_stale_pass_before_work() {
    let path = std::env::temp_dir().join(format!(
        "agentic-memory-office-parse-status-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, br#"{"status":"passed","passed":true}"#).expect("write stale pass");
    begin_actual_parse_report(&path);
    let status: Value = serde_json::from_slice(&std::fs::read(&path).expect("read running report"))
        .expect("parse running report");
    assert_eq!(status["status"], "running");
    assert_eq!(status["passed"], false);
    std::fs::remove_file(path).expect("remove status fixture");
}

#[test]
#[ignore = "release-only real-parser benchmark; run just benchmark-office-parse"]
#[allow(clippy::too_many_lines)] // Keep the emitted manifest/report fields beside their hard gates.
fn actual_large_office_files_parse_benchmark() {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&target).expect("create target directory");
    let manifest_path = target.join("office-actual-parse-manifest.json");
    let report_path = target.join("office-actual-parse-benchmark.json");
    begin_actual_parse_report(&report_path);
    require_release_profile();
    let _ = std::fs::remove_file(&manifest_path);
    let fixtures = actual_large_fixtures();

    let mut manifest_entries = Vec::new();
    let mut reports = Vec::new();
    for (fixture, oracle) in fixtures.iter().zip(ACTUAL_LARGE_ORACLES) {
        assert_eq!(fixture.format, oracle.format);
        let input_path = target.join(format!(
            "office-actual-parse-{}.{}",
            fixture.format, fixture.extension
        ));
        std::fs::write(&input_path, &fixture.bytes).expect("write actual large fixture");
        let source_sha256 = hex_digest(&fixture.bytes);
        assert_eq!(source_sha256, oracle.source_sha256);
        let expanded_bytes = expanded_container_bytes(fixture);
        if let Some(expanded_bytes) = expanded_bytes {
            let limit = if fixture.format == "xlsx" {
                MAX_ZIP_EXPANDED_BYTES
            } else {
                MAX_OFFICE_ZIP_EXPANDED_BYTES
            };
            assert!(expanded_bytes <= limit);
        }
        manifest_entries.push(serde_json::json!({
            "format": fixture.format,
            "input_artifact": input_path.file_name().and_then(|name| name.to_str()).expect("input file name"),
            "input_bytes": fixture.bytes.len(),
            "source_sha256": source_sha256,
            "expanded_bytes": expanded_bytes,
            "oracle": oracle,
        }));

        let mut samples = Vec::new();
        for _ in 0..ACTUAL_LARGE_PARSE_SAMPLES {
            let extraction = isolated_extract_path(&input_path)
                .unwrap_or_else(|error| panic!("isolated {} parse: {error}", fixture.format));
            assert!(extraction.document.unsupported.is_empty());
            assert!(actual_large_evidence_matches(&oracle, &extraction.document));
            samples.push(serde_json::json!({
                "extraction_nanos": extraction.extraction_nanos,
                "process_wall_nanos": extraction.process_wall_nanos,
                "max_sampled_child_rss_bytes": extraction.max_sampled_child_rss_bytes,
            }));
        }
        let extraction_times = metric_values(&samples, "extraction_nanos");
        let process_walls = metric_values(&samples, "process_wall_nanos");
        let max_sampled_child_rss_bytes = samples
            .iter()
            .filter_map(|sample| sample["max_sampled_child_rss_bytes"].as_u64())
            .max();
        reports.push(serde_json::json!({
            "format": fixture.format,
            "source_sha256": source_sha256,
            "input_bytes": fixture.bytes.len(),
            "expanded_bytes": expanded_bytes,
            "passage_count": oracle.passage_count,
            "passage_locator_text_bytes": oracle.passage_locator_text_bytes,
            "serialized_document_bytes": oracle.serialized_document_bytes,
            "oracle_passed": true,
            "failures": [],
            "samples": samples,
            "extraction_nanos_p50": percentile(&extraction_times, 50),
            "extraction_nanos_p95": percentile(&extraction_times, 95),
            "process_wall_nanos_p50": percentile(&process_walls, 50),
            "process_wall_nanos_p95": percentile(&process_walls, 95),
            "max_sampled_child_rss_bytes": max_sampled_child_rss_bytes,
        }));
    }

    let manifest = serde_json::json!({
        "schema": 1,
        "mode": "actual_enlarged_office_binary_parse",
        "entries": manifest_entries,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialize parse manifest");
    write_bytes_atomically(&manifest_path, &manifest_bytes);
    let harness_sha256 = hex_digest(COMPILED_OFFICE_DOCUMENTS_HARNESS);
    let report = serde_json::json!({
        "schema": 1,
        "mode": "actual_enlarged_office_binary_parse",
        "status": "passed",
        "passed": true,
        "sample_count_per_format": ACTUAL_LARGE_PARSE_SAMPLES,
        "git_baseline": command_output("git", &["rev-parse", "HEAD"]),
        "git_worktree_dirty": !command_output("git", &["status", "--porcelain"]).is_empty(),
        "rustc": command_output("rustc", &["--version"]),
        "profile": "release",
        "harness_sha256": harness_sha256,
        "manifest_sha256": hex_digest(&manifest_bytes),
        "limits": {
            "source_bytes": MAX_SOURCE_BYTES,
            "normalized_output_bytes": MAX_OUTPUT_BYTES,
            "serialized_envelope_bytes": MAX_SERIALIZED_OUTPUT_BYTES,
            "pdf_pages": MAX_PDF_PAGES,
            "office_blocks": MAX_OFFICE_BLOCKS,
            "rows": MAX_ROWS,
            "cells": MAX_CELLS,
            "sampled_child_rss_bytes": MAX_ISOLATED_PARSER_RSS_BYTES,
        },
        "input_scope": "bounded structural enlargement under the fixture admission caps; not general large-file capacity",
        "timing_scope": "child read + admission + parse/extract + source hash; excludes child process startup and result serialization",
        "rss_scope": "maximum parent ps sample of live child total RSS; not a native hard ceiling or true peak; brief spikes and nonresident mappings may escape",
        "artifact_stability": {"manifest": "deterministic inputs and oracle", "report": "raw timings and RSS samples vary by run"},
        "formats": reports,
    });
    write_bytes_atomically(
        &report_path,
        &serde_json::to_vec_pretty(&report).expect("serialize parse report"),
    );
    let written: Value =
        serde_json::from_slice(&std::fs::read(&report_path).expect("read completed parse report"))
            .expect("parse completed parse report");
    assert_eq!(written["harness_sha256"], harness_sha256);
    assert_eq!(written["status"], "passed");
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("print report")
    );
}

fn require_release_profile() {
    #[cfg(debug_assertions)]
    panic!("parser benchmark requires --release");
}

fn actual_large_evidence_matches(oracle: &ActualLargeOracle, document: &Document) -> bool {
    if document.source_sha256 != oracle.source_sha256
        || document.source_revision != format!("sha256:{}", oracle.source_sha256)
        || !document.unsupported.is_empty()
        || document.passages.len() != oracle.passage_count
        || passage_locator_text_bytes(document) != oracle.passage_locator_text_bytes
        || serde_json::to_vec(document).map_or(true, |bytes| {
            bytes.len() != oracle.serialized_document_bytes
        })
    {
        return false;
    }

    let mut matching = document
        .passages
        .iter()
        .filter(|passage| passage.locator == oracle.locator && passage.text == oracle.text);
    let Some(passage) = matching.next() else {
        return false;
    };
    if matching.next().is_some() || passage.cells.len() != oracle.cells.len() {
        return false;
    }
    passage
        .cells
        .iter()
        .zip(oracle.cells)
        .all(|(actual, expected)| {
            matches!(
                actual,
                CellEvidence::Literal {
                    locator,
                    value_type,
                    value,
                } if locator == expected.locator
                    && value_type == expected.value_type
                    && value == expected.value
            )
        })
}

fn passage_locator_text_bytes(document: &Document) -> usize {
    document
        .passages
        .iter()
        .map(|passage| passage.locator.len() + passage.text.len())
        .sum()
}

fn begin_actual_parse_report(path: &Path) {
    let report = serde_json::json!({
        "schema": 1,
        "mode": "actual_enlarged_office_binary_parse",
        "status": "running",
        "passed": false,
    });
    let bytes = serde_json::to_vec_pretty(&report).expect("serialize running parse report");
    write_bytes_atomically(path, &bytes);
    let written: Value =
        serde_json::from_slice(&std::fs::read(path).expect("read running parse report"))
            .expect("parse running parse report");
    assert_eq!(written["status"], "running");
    assert_eq!(written["passed"], false);
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("artifact file name");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    if temporary.exists() {
        std::fs::remove_file(&temporary).expect("remove stale temporary artifact");
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .expect("create temporary artifact");
    file.write_all(bytes).expect("write temporary artifact");
    file.sync_all().expect("sync temporary artifact");
    drop(file);
    std::fs::rename(&temporary, path).expect("replace artifact atomically");
    sync_parent_directory(path);
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .expect("sync artifact directory");
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) {}

fn metric_values(samples: &[Value], field: &str) -> Vec<u64> {
    samples
        .iter()
        .map(|sample| sample[field].as_u64().expect("numeric metric"))
        .collect()
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    assert!(!values.is_empty() && (1..=100).contains(&percentile));
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() * percentile).div_ceil(100) - 1]
}

fn command_output(command: &str, arguments: &[&str]) -> String {
    let output = std::process::Command::new(command)
        .args(arguments)
        .output()
        .expect("benchmark metadata command");
    assert!(output.status.success(), "benchmark metadata command failed");
    String::from_utf8(output.stdout)
        .expect("UTF-8 benchmark metadata")
        .trim()
        .to_owned()
}

#[test]
fn csv_field_citations_reject_missing_or_ambiguous_headers() {
    assert_eq!(
        extract_csv(b"Name,\nZoe,Calls\n", "00").unwrap_err(),
        "CSV requires non-empty unique headers"
    );
    assert_eq!(
        extract_csv(b"Name,   \nZoe,Calls\n", "00").unwrap_err(),
        "CSV requires non-empty unique headers"
    );
    assert_eq!(
        extract_csv(b"Name,Name\nZoe,Calls\n", "00").unwrap_err(),
        "CSV requires non-empty unique headers"
    );
    assert_eq!(
        extract_csv(b"Name, Name \nZoe,Calls\n", "00").unwrap_err(),
        "CSV requires non-empty unique headers"
    );
}

#[test]
fn extraction_rejects_untrusted_shapes_before_activation() {
    assert!(extract_text(&vec![b'x'; MAX_SOURCE_BYTES + 1]).is_err());
    assert!(extract_text(b"").is_err());
    assert!(extract_text(&[0xff]).is_err());
    assert!(extract_csv(b"a,b\n1\n", "00").is_err());
    assert!(extract_csv(b"a,b\n", "00").is_err());
    assert!(extract_csv(b"a\n\xff\n", "00").is_err());
    let too_many_columns = format!(
        "{}\n{}\n",
        vec!["h"; MAX_COLUMNS + 1].join(","),
        vec!["v"; MAX_COLUMNS + 1].join(",")
    );
    assert!(extract_csv(too_many_columns.as_bytes(), "00").is_err());
    let too_many_rows = format!("h\n{}", "v\n".repeat(MAX_ROWS + 1));
    assert!(extract_csv(too_many_rows.as_bytes(), "00").is_err());
    let headers = (0..MAX_COLUMNS)
        .map(|column| format!("h{column}"))
        .collect::<Vec<_>>()
        .join(",");
    let row = vec!["v"; MAX_COLUMNS].join(",");
    let too_many_cells = format!("{headers}\n{}", format!("{row}\n").repeat(33));
    assert_eq!(
        extract_csv(too_many_cells.as_bytes(), "00").unwrap_err(),
        "CSV row/cell limit exceeded"
    );
    let long_headers = (0..MAX_COLUMNS)
        .map(|column| format!("H{column:02}{}", "H".repeat(297)))
        .collect::<Vec<_>>()
        .join(",");
    let output_heavy = format!("{long_headers}\n{}", format!("{row}\n").repeat(4));
    assert_eq!(
        extract_csv(output_heavy.as_bytes(), "00").unwrap_err(),
        "CSV output limit exceeded"
    );
    let oversized_output = temporary_office_path("oversized-output", "json", 0);
    std::fs::write(
        &oversized_output,
        vec![b'x'; MAX_SERIALIZED_OUTPUT_BYTES + 1],
    )
    .expect("write oversized parser output");
    assert_eq!(
        read_bounded_parser_output(&oversized_output).unwrap_err(),
        "serialized parser output limit exceeded"
    );
    std::fs::remove_file(oversized_output).expect("remove oversized parser output");

    let mut cells = BTreeMap::new();
    for (row, value) in [(0, "Name"), (1, "FIRST"), (2, "LAST ROW LITERAL")] {
        cells.insert(
            (row, 0),
            SheetCell {
                value_type: "string".into(),
                value: value.into(),
                formula: None,
            },
        );
    }
    let mut retained_bytes = 0;
    let passages = sheet_passages("Rows", &cells, &mut retained_bytes).expect("complete rows");
    assert_eq!(passages.len(), 2);
    assert_eq!(passages[1].locator, "sheet=Rows;headers=A1:A1;cells=A3:A3");
    assert_eq!(passages[1].text, "Name: LAST ROW LITERAL");

    cells.get_mut(&(2, 0)).expect("last cell").value = "x".repeat(MAX_OUTPUT_BYTES);
    assert!(sheet_passages("Rows", &cells, &mut 0).is_err());
}

#[test]
fn xlsx_zip_preflight_rejects_expanded_entry() {
    use std::io::{Cursor, Write};
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "xl/styles.xml",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .expect("start synthetic ZIP entry");
    writer
        .write_all(&vec![
            0;
            usize::try_from(MAX_ZIP_ENTRY_BYTES)
                .expect("test limit fits usize")
                + 1
        ])
        .expect("write synthetic ZIP entry");
    let archive = writer.finish().expect("finish synthetic ZIP").into_inner();
    assert_eq!(
        preflight_zip(Cursor::new(archive)).unwrap_err(),
        "XLSX ZIP entry size limit exceeded"
    );
}

#[test]
fn actual_xlsx_containers_reject_hidden_drawing_and_cacheless_formula_content() {
    use std::io::Cursor;

    let hidden = rewritten_xlsx(
        Some(("xl/workbook.xml", |xml| {
            xml.replace(
                "<x:sheet name=\"Rates\"",
                "<x:sheet state=\"hidden\" name=\"Rates\"",
            )
        })),
        None,
    );
    assert_eq!(
        preflight_zip(Cursor::new(hidden)).unwrap_err(),
        "hidden XLSX sheets are unsupported"
    );

    let drawing = rewritten_xlsx(None, Some(("xl/drawings/drawing1.xml", "<drawing/>")));
    assert_eq!(
        preflight_zip(Cursor::new(drawing)).unwrap_err(),
        "unsupported XLSX component: xl/drawings/drawing1.xml"
    );

    let invalid_strings = rewritten_xlsx(
        Some(("xl/sharedStrings.xml", |xml| {
            xml.replace(" />", " uniqueCount=\"2048\" />")
        })),
        None,
    );
    assert_eq!(
        preflight_zip(Cursor::new(invalid_strings)).unwrap_err(),
        "excessive XLSX sharedStrings uniqueCount"
    );

    let cacheless = rewritten_xlsx(
        Some(("xl/worksheets/sheet1.xml", |xml| {
            xml.replace("<x:f>C2+0.005</x:f><x:v>0.08</x:v>", "<x:f>C2+0.005</x:f>")
        })),
        None,
    );
    let path = std::env::temp_dir().join(format!(
        "agentic-memory-cacheless-formula-{}.xlsx",
        std::process::id()
    ));
    std::fs::write(&path, &cacheless).expect("write cacheless XLSX fixture");
    let result = extract_xlsx(&path, hex_digest(&cacheless));
    std::fs::remove_file(path).expect("remove cacheless XLSX fixture");
    assert_eq!(
        result.unwrap_err(),
        "cacheless XLSX formulas are unsupported"
    );

    let truncated = rewritten_xlsx(
        Some(("xl/workbook.xml", |xml| xml.replace("</x:workbook>", ""))),
        None,
    );
    assert!(preflight_zip(Cursor::new(truncated)).is_err());
}

#[test]
fn actual_pdf_and_ooxml_negatives_are_rejected_before_activation() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/office");
    for name in ["malformed.pdf", "encrypted.pdf", "visual-only.pdf"] {
        let path = fixtures.join(name);
        let bytes = std::fs::read(&path).expect("read negative PDF fixture");
        assert!(extract_pdf(&path, hex_digest(&bytes)).is_err(), "{name}");
    }

    let unknown = rewritten_office(
        "handbook.docx",
        None,
        Some(("word/charts/chart1.xml", b"<chart/>")),
    );
    assert_ooxml_rejected(&unknown, OfficeKind::Docx, "unsupported OOXML component");

    let external = rewritten_office(
        "briefing.pptx",
        Some(("_rels/.rels", |xml| {
            xml.replace(" Target=", " TargetMode=\"External\" Target=")
        })),
        None,
    );
    assert_ooxml_rejected(
        &external,
        OfficeKind::Pptx,
        "external OOXML relationships are unsupported",
    );

    let malformed = rewritten_office(
        "handbook.docx",
        Some(("[Content_Types].xml", |xml| xml.replace("</Types>", ""))),
        None,
    );
    assert_ooxml_rejected(
        &malformed,
        OfficeKind::Docx,
        "invalid OOXML admission XML root",
    );

    let duplicate = rewritten_office(
        "handbook.docx",
        Some(("_rels/.rels", |xml| {
            xml.replace(" Type=", " x:Type=\"duplicate\" Type=")
                .replace("<Relationships ", "<Relationships xmlns:x=\"urn:x\" ")
        })),
        None,
    );
    assert_ooxml_rejected(&duplicate, OfficeKind::Docx, "attribute");

    let notes = rewritten_office(
        "briefing.pptx",
        Some(("ppt/notesSlides/notesSlide1.xml", |xml| {
            xml.replace("</p:notes>", "<a:t xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">SECRET NOTE</a:t></p:notes>")
        })),
        None,
    );
    assert_ooxml_rejected(&notes, OfficeKind::Pptx, "PPTX notes text is unsupported");

    let oversized = rewritten_office(
        "handbook.docx",
        Some(("word/numbering.xml", |_| {
            "x".repeat(
                usize::try_from(MAX_OFFICE_ZIP_ENTRY_BYTES).expect("test limit fits usize") + 1,
            )
        })),
        None,
    );
    assert_ooxml_rejected(
        &oversized,
        OfficeKind::Docx,
        "oversized OOXML central entry",
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the adversarial mutation corpus and its expected evidence together.
fn reviewed_office_admission_and_evidence_regressions() {
    let mixed_pdf = mixed_text_image_pdf();
    let mixed_path = temporary_office_path("mixed-text-image", "pdf", mixed_pdf.len());
    std::fs::write(&mixed_path, &mixed_pdf).expect("write mixed PDF");
    assert!(extract_pdf(&mixed_path, hex_digest(&mixed_pdf)).is_err());
    std::fs::remove_file(mixed_path).expect("remove mixed PDF");
    let inline_pdf = inline_image_pdf();
    let inline_path = temporary_office_path("inline-image", "pdf", inline_pdf.len());
    std::fs::write(&inline_path, &inline_pdf).expect("write inline-image PDF");
    assert!(extract_pdf(&inline_path, hex_digest(&inline_pdf)).is_err());
    std::fs::remove_file(inline_path).expect("remove inline-image PDF");

    for (bytes, kind, expected) in [
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/slides/slide2.xml", |xml| {
                    xml.replace("<p:cNvPr id=\"6\"", "<p:cNvPr hidden=\"1\" id=\"6\"")
                })),
                None,
            ),
            OfficeKind::Pptx,
            "hidden PPTX drawing",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/notesSlides/notesSlide1.xml", |xml| {
                    xml.replace(
                        "</p:notes>",
                        "<a:t xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><![CDATA[SECRET NOTE]]></a:t></p:notes>",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "PPTX notes text",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replace(
                        "<w:r><w:t>The paragraph",
                        "<w:r><w:rPr><w:vanish/></w:rPr><w:t>The paragraph",
                    )
                })),
                None,
            ),
            OfficeKind::Docx,
            "hidden/revision",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replace(
                        "<w:r><w:t>The paragraph",
                        "<w:r><w:rPr><w:rPrChange w:id=\"7\" w:author=\"Synthetic\"><w:rPr><w:b/></w:rPr></w:rPrChange></w:rPr><w:t>The paragraph",
                    )
                })),
                None,
            ),
            OfficeKind::Docx,
            "hidden/revision",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                None,
                Some((
                    "ppt/slides/slide99.xml",
                    b"<p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"/>",
                )),
            ),
            OfficeKind::Pptx,
            "unsupported OOXML component",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/styles.xml", |_| "<w:styles".to_owned())),
                None,
            ),
            OfficeKind::Docx,
            "malformed OOXML component",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/settings.xml", |xml| {
                    xml.replace("</w:settings>", "<w:t>SECRET</w:t></w:settings>")
                })),
                None,
            ),
            OfficeKind::Docx,
            "unsupported text in unindexed",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/_rels/presentation.xml.rels", |xml| {
                    xml.replace(
                        "/ppt/slides/slide2.xml",
                        "/ppt/slides/missing.xml",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "relationship target missing",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/_rels/presentation.xml.rels", |xml| {
                    xml.replace("R5272de47c1c543a2", "R57467f49aab7434d")
                })),
                None,
            ),
            OfficeKind::Pptx,
            "duplicate OOXML relationship ID",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("[Content_Types].xml", |xml| {
                    xml.replace(
                        "PartName=\"/ppt/slides/slide2.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"",
                        "PartName=\"/ppt/slides/slide2.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml\"",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "content type mismatch",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/notesSlides/notesSlide1.xml", |xml| {
                    xml.replace(
                        "<a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" />",
                        "<a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:r><a:t>&#83;&#69;&#67;&#82;&#69;&#84;</a:t></a:r></a:p>",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "PPTX notes text",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/notesMasters/notesMaster1.xml", |xml| {
                    xml.replace(
                        "<a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" />",
                        "<a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:r><a:t>&#83;</a:t></a:r></a:p>",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "notes text",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/settings.xml", |xml| {
                    xml.replace("</w:settings>", "<w:t>&#83;</w:t></w:settings>")
                })),
                None,
            ),
            OfficeKind::Docx,
            "referenced text",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/settings.xml", |xml| {
                    xml.replace("</w:settings>", "&undefined;</w:settings>")
                })),
                None,
            ),
            OfficeKind::Docx,
            "undeclared OOXML entity",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| format!("{xml}TRAILING JUNK"))),
                None,
            ),
            OfficeKind::Docx,
            "unsupported text",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("[Content_Types].xml", |xml| {
                    xml.replace(
                        "application/vnd.openxmlformats-package.relationships+xml",
                        "application/xml",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "relationships content type mismatch",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/slides/_rels/slide1.xml.rels", |xml| {
                    xml.replace(
                        "<Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"/ppt/slideLayouts/slideLayout1.xml\" Id=\"R1215003528c44023\" />",
                        "",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "required relationship edge set mismatch",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/slideLayouts/_rels/slideLayout1.xml.rels", |xml| {
                    xml.replace(
                        "<Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"/ppt/slideMasters/slideMaster1.xml\" Id=\"R553fc82c525b42e9\" />",
                        "",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "required relationship edge set mismatch",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/settings.xml", |xml| {
                    xml.replace(
                        "</w:settings>",
                        "<w:docVars><w:docVar w:name=\"EVIDENCE\" w:val=\"SECRET\"/></w:docVars></w:settings>",
                    )
                })),
                None,
            ),
            OfficeKind::Docx,
            "invariant OOXML component",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replace(
                        "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
                        "urn:wrong-word-namespace",
                    )
                })),
                None,
            ),
            OfficeKind::Docx,
            "namespace",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    format!("{xml}<![CDATA[TRAILING SECRET]]>")
                })),
                None,
            ),
            OfficeKind::Docx,
            "unsupported text",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/_rels/presentation.xml.rels", |xml| {
                    format!("{xml}TRAILING JUNK")
                })),
                None,
            ),
            OfficeKind::Pptx,
            "admission XML text",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("[Content_Types].xml", |xml| format!("{xml}TRAILING JUNK"))),
                None,
            ),
            OfficeKind::Pptx,
            "admission XML text",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/_rels/presentation.xml.rels", |xml| {
                    xml.replacen(
                        "<Relationships",
                        "<!DOCTYPE Relationships><Relationships",
                        1,
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "admission document types",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/_rels/presentation.xml.rels", |xml| {
                    xml.replace(
                        "http://schemas.openxmlformats.org/package/2006/relationships",
                        "urn:wrong",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "admission",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("[Content_Types].xml", |xml| {
                    xml.replace(
                        "http://schemas.openxmlformats.org/package/2006/content-types",
                        "urn:wrong",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "admission",
        ),
        (
            rewritten_office(
                "briefing.pptx",
                Some(("[Content_Types].xml", |xml| {
                    xml.replace(
                        "</Types>",
                        "<Override PartName=\"/ppt/_rels/presentation.xml.rels\" ContentType=\"application/xml\"/></Types>",
                    )
                })),
                None,
            ),
            OfficeKind::Pptx,
            "relationship part content type mismatch",
        ),
        (
            rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replacen(
                        "<w:p><w:pPr><w:spacing",
                        "<w:p xmlns:w=\"urn:wrong\"><w:pPr><w:spacing",
                        1,
                    )
                })),
                None,
            ),
            OfficeKind::Docx,
            "namespace",
        ),
    ] {
        assert_ooxml_rejected(&bytes, kind, expected);
    }

    let too_many = rewritten_office(
        "handbook.docx",
        Some(("word/document.xml", |xml| {
            xml.replace(
                "<w:sectPr",
                &("<w:p><w:r><w:t>X</w:t></w:r></w:p>".repeat(101) + "<w:sectPr"),
            )
        })),
        None,
    );
    assert!(extract_rewritten_office(&too_many, OfficeKind::Docx).is_err());

    let duplicate_numeric_slide_id = rewritten_office(
        "briefing.pptx",
        Some(("ppt/presentation.xml", |xml| {
            xml.replace("id=\"257\"", "id=\"256\"")
        })),
        None,
    );
    assert_eq!(
        extract_rewritten_office(&duplicate_numeric_slide_id, OfficeKind::Pptx).unwrap_err(),
        "duplicate PPTX numeric slide ID"
    );

    let too_many_pptx_paragraphs = rewritten_office(
        "briefing.pptx",
        Some(("ppt/slides/slide1.xml", |xml| {
            xml.replacen(
                "</p:txBody>",
                &("<a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:r><a:t>X</a:t></a:r></a:p>".repeat(101)
                    + "</p:txBody>"),
                1,
            )
        })),
        None,
    );
    assert!(extract_rewritten_office(&too_many_pptx_paragraphs, OfficeKind::Pptx).is_err());

    let invalid_numeric_slide_id = rewritten_office(
        "briefing.pptx",
        Some(("ppt/presentation.xml", |xml| {
            xml.replace("id=\"257\"", "id=\"0\"")
        })),
        None,
    );
    assert_eq!(
        extract_rewritten_office(&invalid_numeric_slide_id, OfficeKind::Pptx).unwrap_err(),
        "PPTX numeric slide ID is out of range"
    );
    for (id, boundary_slide_id, accepted) in [
        (
            "255",
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/presentation.xml", |xml| {
                    xml.replace("id=\"257\"", "id=\"255\"")
                })),
                None,
            ),
            false,
        ),
        (
            "2147483647",
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/presentation.xml", |xml| {
                    xml.replace("id=\"257\"", "id=\"2147483647\"")
                })),
                None,
            ),
            true,
        ),
        (
            "2147483648",
            rewritten_office(
                "briefing.pptx",
                Some(("ppt/presentation.xml", |xml| {
                    xml.replace("id=\"257\"", "id=\"2147483648\"")
                })),
                None,
            ),
            false,
        ),
    ] {
        assert_eq!(
            extract_rewritten_office(&boundary_slide_id, OfficeKind::Pptx).is_ok(),
            accepted,
            "slide ID boundary {id}"
        );
    }

    for revised_table in [
        rewritten_office(
            "handbook.docx",
            Some(("word/document.xml", |xml| {
                xml.replace(
                    "</w:tblGrid>",
                    "<w:tblGridChange w:id=\"7\"><w:tblGrid><w:gridCol w:w=\"9360\"/></w:tblGrid></w:tblGridChange></w:tblGrid>",
                )
            })),
            None,
        ),
        rewritten_office(
            "handbook.docx",
            Some(("word/document.xml", |xml| {
                xml.replace("</w:tblGrid>", "<w:gridCol w:w=\"1000\"/></w:tblGrid>")
                    .replacen(
                        "<w:tcPr><w:tcW",
                        "<w:tcPr><w:gridSpan w:val=\"2\"/><w:tcW",
                        1,
                    )
            })),
            None,
        ),
    ] {
        assert!(extract_rewritten_office(&revised_table, OfficeKind::Docx).is_err());
    }

    for unsupported_document_structure in [
        rewritten_office(
            "handbook.docx",
            Some(("word/document.xml", |xml| {
                xml.replace(
                    "</w:document>",
                    "<w:body><w:p><w:r><w:t>HIDDEN SECOND BODY</w:t></w:r></w:p></w:body></w:document>",
                )
            })),
            None,
        ),
        rewritten_office(
            "handbook.docx",
            Some(("word/document.xml", |xml| {
                xml.replacen(
                    "<w:body>",
                    "<w:body><w:unknown><w:p><w:r><w:t>HIDDEN CHILD</w:t></w:r></w:p></w:unknown>",
                    1,
                )
            })),
            None,
        ),
    ] {
        assert!(
            extract_rewritten_office(&unsupported_document_structure, OfficeKind::Docx).is_err()
        );
    }

    for direct_body_content in ["DIRECT BODY TEXT", "<![CDATA[DIRECT BODY CDATA]]>", "&#83;"] {
        let direct_body_content = match direct_body_content {
            "DIRECT BODY TEXT" => rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replacen("<w:body>", "<w:body>DIRECT BODY TEXT", 1)
                })),
                None,
            ),
            "<![CDATA[DIRECT BODY CDATA]]>" => rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replacen("<w:body>", "<w:body><![CDATA[DIRECT BODY CDATA]]>", 1)
                })),
                None,
            ),
            _ => rewritten_office(
                "handbook.docx",
                Some(("word/document.xml", |xml| {
                    xml.replacen("<w:body>", "<w:body>&#83;", 1)
                })),
                None,
            ),
        };
        assert!(extract_rewritten_office(&direct_body_content, OfficeKind::Docx).is_err());
    }

    let foreign_text_namespace = rewritten_office(
        "handbook.docx",
        Some(("word/document.xml", |xml| {
            xml.replacen("<w:t>", "<x:t xmlns:x=\"urn:wrong\">", 1)
                .replacen("</w:t>", "</x:t>", 1)
        })),
        None,
    );
    assert!(extract_rewritten_office(&foreign_text_namespace, OfficeKind::Docx).is_err());

    let aliased_word_style = rewritten_office(
        "handbook.docx",
        Some(("word/document.xml", |xml| {
            xml.replacen(
                "<w:pStyle",
                "<x:pStyle xmlns:x=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"",
                1,
            )
        })),
        None,
    );
    assert!(extract_rewritten_office(&aliased_word_style, OfficeKind::Docx).is_err());

    let aliased_shape_text = rewritten_office(
        "briefing.pptx",
        Some(("ppt/slides/slide2.xml", |xml| {
            xml.replacen(
                "<p:txBody>",
                "<a:txBody xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">",
                1,
            )
            .replacen("</p:txBody>", "</a:txBody>", 1)
        })),
        None,
    );
    assert!(extract_rewritten_office(&aliased_shape_text, OfficeKind::Pptx).is_err());

    for invalid_attribute_namespace in [
        rewritten_office(
            "briefing.pptx",
            Some(("ppt/slides/slide1.xml", |xml| {
                xml.replacen("<p:sld ", "<p:sld x:show=\"1\" ", 1)
            })),
            None,
        ),
        rewritten_office(
            "briefing.pptx",
            Some(("ppt/slides/slide1.xml", |xml| {
                xml.replacen("<p:sld ", "<p:sld xmlns:x=\"urn:foreign\" x:show=\"1\" ", 1)
            })),
            None,
        ),
    ] {
        assert!(extract_rewritten_office(&invalid_attribute_namespace, OfficeKind::Pptx).is_err());
    }

    let unsupported_word_attribute = rewritten_office(
        "handbook.docx",
        Some(("word/document.xml", |xml| {
            xml.replacen("<w:p>", "<w:p w:unsupported=\"DISCARDED\">", 1)
        })),
        None,
    );
    assert!(extract_rewritten_office(&unsupported_word_attribute, OfficeKind::Docx).is_err());
    let misplaced_pptx_attribute = rewritten_office(
        "briefing.pptx",
        Some(("ppt/slides/slide2.xml", |xml| {
            xml.replacen("<a:t>", "<a:t show=\"0\">", 1)
        })),
        None,
    );
    assert!(extract_rewritten_office(&misplaced_pptx_attribute, OfficeKind::Pptx).is_err());

    for malformed_admission_child in [
        rewritten_office(
            "handbook.docx",
            Some(("_rels/.rels", |xml| {
                xml.replacen("<Relationship ", "<x:Relationship ", 1)
            })),
            None,
        ),
        rewritten_office(
            "handbook.docx",
            Some(("_rels/.rels", |xml| {
                xml.replacen(
                    "<Relationship ",
                    "<x:Relationship xmlns:x=\"http://schemas.openxmlformats.org/package/2006/relationships\" ",
                    1,
                )
            })),
            None,
        ),
        rewritten_office(
            "handbook.docx",
            Some(("[Content_Types].xml", |xml| {
                xml.replacen("<Default ", "<Default xmlns=\"urn:wrong\" ", 1)
            })),
            None,
        ),
        rewritten_office(
            "handbook.docx",
            Some(("_rels/.rels", |xml| {
                xml.replacen("/>", "><Relationship/></Relationship>", 1)
            })),
            None,
        ),
    ] {
        assert!(extract_rewritten_office(&malformed_admission_child, OfficeKind::Docx).is_err());
    }

    for duplicate_singleton in [
        rewritten_office(
            "briefing.pptx",
            Some(("ppt/slides/slide2.xml", |xml| {
                xml.replacen(
                    "<a:t>The last-slide marker is INDIGO PUFFIN.</a:t>",
                    "<a:t>DISCARDED</a:t><a:t>The last-slide marker is INDIGO PUFFIN.</a:t>",
                    1,
                )
            })),
            None,
        ),
        rewritten_office(
            "briefing.pptx",
            Some(("ppt/slides/slide2.xml", |xml| {
                xml.replacen(
                    "<p:txBody>",
                    "<p:txBody><a:bodyPr xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"/><a:lstStyle xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"/><a:p xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:r><a:t>DISCARDED</a:t></a:r></a:p></p:txBody><p:txBody>",
                    1,
                )
            })),
            None,
        ),
    ] {
        assert!(extract_rewritten_office(&duplicate_singleton, OfficeKind::Pptx).is_err());
    }

    assert!(extract_office_fixture("fixtures/office/handbook.docx").is_ok());
    assert!(extract_office_fixture("fixtures/office/briefing.pptx").is_ok());

    for row_property in [
        "<w:trPr><w:hidden/></w:trPr>",
        "<w:trPr><w:gridBefore w:val=\"1\"/></w:trPr>",
        "<w:trPr><w:gridAfter w:val=\"1\"/></w:trPr>",
    ] {
        let row_property = rewritten_office(
            "handbook.docx",
            Some((
                "word/document.xml",
                match row_property {
                    "<w:trPr><w:hidden/></w:trPr>" => |xml: String| {
                        xml.replacen("<w:tr>", "<w:tr><w:trPr><w:hidden/></w:trPr>", 1)
                    },
                    "<w:trPr><w:gridBefore w:val=\"1\"/></w:trPr>" => |xml: String| {
                        xml.replacen(
                            "<w:tr>",
                            "<w:tr><w:trPr><w:gridBefore w:val=\"1\"/></w:trPr>",
                            1,
                        )
                    },
                    _ => |xml: String| {
                        xml.replacen(
                            "<w:tr>",
                            "<w:tr><w:trPr><w:gridAfter w:val=\"1\"/></w:trPr>",
                            1,
                        )
                    },
                },
            )),
            None,
        );
        assert!(extract_rewritten_office(&row_property, OfficeKind::Docx).is_err());
    }

    for table_geometry in [
        rewritten_office(
            "briefing.pptx",
            Some(("ppt/slides/slide2.xml", |xml| {
                xml.replacen("<a:tc>", "<a:tc gridSpan=\"2\">", 1)
            })),
            None,
        ),
        rewritten_office(
            "briefing.pptx",
            Some(("ppt/slides/slide2.xml", |xml| {
                xml.replace("</a:tblGrid>", "<a:gridCol w=\"1000\" /></a:tblGrid>")
            })),
            None,
        ),
    ] {
        assert!(extract_rewritten_office(&table_geometry, OfficeKind::Pptx).is_err());
    }

    let duplicate_raw = office_with_duplicate_entries("handbook.docx", "word/settings.xml", 1);
    assert_ooxml_rejected(&duplicate_raw, OfficeKind::Docx, "duplicate");
    let many_duplicates = office_with_duplicate_entries("handbook.docx", "word/settings.xml", 70);
    assert_ooxml_rejected(&many_duplicates, OfficeKind::Docx, "entry limit");
    let original_docx = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/office/handbook.docx"),
    )
    .expect("read DOCX for raw ZIP corruption tests");
    assert!(validate_raw_office_zip(b"PK\x03\x04truncated").is_err());
    let mut fake_signature = original_docx.clone();
    fake_signature.extend_from_slice(b"PK\x05\x06FAKE");
    assert!(validate_raw_office_zip(&fake_signature).is_err());
    let mut malformed_central = original_docx;
    let central = malformed_central
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .expect("central directory signature");
    malformed_central[central] = b'X';
    assert!(validate_raw_office_zip(&malformed_central).is_err());

    let original_pptx = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/office/briefing.pptx"),
    )
    .expect("read PPTX for physical ZIP regressions");
    let mut central_size_mismatch = original_pptx.clone();
    let slide_record =
        office_central_record_offset(&central_size_mismatch, "ppt/slides/slide1.xml");
    let declared_size = office_zip_u32(&central_size_mismatch, slide_record + 24);
    office_put_zip_u32(
        &mut central_size_mismatch,
        slide_record + 24,
        declared_size + 1,
    );
    assert!(validate_raw_office_zip(&central_size_mismatch).is_err());

    let mut both_headers_uncompressed_high = original_pptx.clone();
    let slide_record =
        office_central_record_offset(&both_headers_uncompressed_high, "ppt/slides/slide1.xml");
    let local_record = usize::try_from(office_zip_u32(
        &both_headers_uncompressed_high,
        slide_record + 42,
    ))
    .expect("small test ZIP local offset");
    let declared_size = office_zip_u32(&both_headers_uncompressed_high, slide_record + 24) + 1;
    office_put_zip_u32(
        &mut both_headers_uncompressed_high,
        slide_record + 24,
        declared_size,
    );
    office_put_zip_u32(
        &mut both_headers_uncompressed_high,
        local_record + 22,
        declared_size,
    );
    assert!(validate_raw_office_zip(&both_headers_uncompressed_high).is_err());

    let mut redirected_hidden = rewritten_office(
        "briefing.pptx",
        Some(("ppt/slideMasters/theme/theme2.xml", |xml| {
            format!("{xml}HIDDEN LOCAL CONTENT")
        })),
        None,
    );
    let hidden_record =
        office_central_record_offset(&redirected_hidden, "ppt/slideMasters/theme/theme2.xml");
    let visible_record = office_central_record_offset(&redirected_hidden, "ppt/theme/theme1.xml");
    let visible_offset = office_zip_u32(&redirected_hidden, visible_record + 42);
    office_put_zip_u32(&mut redirected_hidden, hidden_record + 42, visible_offset);
    assert!(validate_raw_office_zip(&redirected_hidden).is_err());

    let mut entry_disk = original_pptx.clone();
    let slide_record = office_central_record_offset(&entry_disk, "ppt/slides/slide1.xml");
    office_put_zip_u16(&mut entry_disk, slide_record + 34, 1);
    assert!(validate_raw_office_zip(&entry_disk).is_err());

    let mut second_central = original_pptx.clone();
    let central_offset = office_central_directory_offset(&original_pptx);
    second_central.extend_from_slice(&original_pptx[central_offset..]);
    let second_eocd = second_central.len() - 22;
    let second_directory_offset = u32::try_from(original_pptx.len()).expect("small test ZIP");
    office_put_zip_u32(
        &mut second_central,
        second_eocd + 16,
        second_directory_offset,
    );
    assert!(validate_raw_office_zip(&second_central).is_err());

    let mut zip64_extra = original_pptx;
    let old_eocd = zip64_extra.len() - 22;
    let record = office_central_record_offset(&zip64_extra, "ppt/slides/slide1.xml");
    let name_length = usize::from(office_zip_u16(&zip64_extra, record + 28));
    let insert_at = record + 46 + name_length;
    let zip64_field = [
        0x01, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    zip64_extra.splice(insert_at..insert_at, zip64_field);
    office_put_zip_u16(&mut zip64_extra, record + 30, 20);
    let new_eocd = old_eocd + 20;
    let old_central_size = office_zip_u32(&zip64_extra, new_eocd + 12);
    office_put_zip_u32(&mut zip64_extra, new_eocd + 12, old_central_size + 20);
    assert!(validate_raw_office_zip(&zip64_extra).is_err());

    assert!(reject_nonempty_notes(b"<p:notes><a:t>&#83;</a:t></p:notes>").is_err());
    assert!(reject_nonempty_notes(b"<p:notes><a:t>&undefined;</a:t></p:notes>").is_err());

    let separated = rewritten_office(
        "handbook.docx",
        Some(("word/document.xml", |xml| {
            xml.replace(
                "<w:t>APAC</w:t>",
                "<w:t>APAC</w:t></w:r></w:p><w:p><w:r><w:t>SECOND PARAGRAPH</w:t>",
            )
        })),
        None,
    );
    let separated = extract_rewritten_office(&separated, OfficeKind::Docx)
        .expect("multiple table-cell paragraphs remain separated");
    assert_eq!(
        separated.passages[1].text,
        "Region: APAC\nSECOND PARAGRAPH\nEscalation: TABLE ROW KINGFISHER"
    );

    let reordered = rewritten_office(
        "briefing.pptx",
        Some(("ppt/presentation.xml", |xml| {
            xml.replace("R57467f49aab7434d", "SYNTHETIC_SWAP_ID")
                .replace("R5272de47c1c543a2", "R57467f49aab7434d")
                .replace("SYNTHETIC_SWAP_ID", "R5272de47c1c543a2")
        })),
        None,
    );
    let reordered = extract_rewritten_office(&reordered, OfficeKind::Pptx)
        .expect("valid declared slide reordering");
    assert_eq!(reordered.passages[0].text, "Renewal actions");
    assert_eq!(
        reordered.passages[0].locator,
        "slide=1;shape-tree=1;shape-id=4"
    );
    assert_eq!(reordered.passages[3].text, "Quarterly briefing");

    let sibling_heading = rewritten_office(
        "handbook.docx",
        Some(("word/document.xml", |xml| {
            xml.replace(
                "<w:sectPr",
                "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:r><w:t>Replacement policy</w:t></w:r></w:p><w:p><w:r><w:t>SIBLING HEADING EVIDENCE</w:t></w:r></w:p><w:sectPr",
            )
        })),
        None,
    );
    let sibling_heading = extract_rewritten_office(&sibling_heading, OfficeKind::Docx)
        .expect("sibling heading extraction");
    assert_eq!(
        sibling_heading
            .passages
            .last()
            .expect("sibling passage")
            .locator,
        "heading=Client renewal handbook/Replacement policy;paragraph=2"
    );
}

#[test]
fn xlsx_xml_parsing_handles_namespaces_quotes_whitespace_and_escapes() {
    assert!(
        validate_xlsx_control_part(
            "xl/workbook.xml",
            b"<workbook xmlns='urn:test'><sheets><sheet state = 'visible'/></sheets></workbook>"
        )
        .is_ok()
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/workbook.xml",
            b"<w:workbook xmlns:w='urn:test'><w:sheets><w:sheet w:state = 'veryHidden'/></w:sheets></w:workbook>"
        )
        .unwrap_err(),
        "hidden XLSX sheets are unsupported"
    );
    assert!(
        validate_xlsx_control_part(
            "xl/sharedStrings.xml",
            b"<s:sst xmlns:s='urn:test' s:uniqueCount = '&#49;'><s:si><s:t>x</s:t></s:si></s:sst>"
        )
        .is_ok()
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/_rels/workbook.xml.rels",
            b"<Relationships xmlns='urn:test'><Relationship Type = 'urn:test/worksheet' TargetMode = 'External'/></Relationships>"
        )
        .unwrap_err(),
        "unsupported XLSX relationship"
    );
    assert!(
        validate_xlsx_control_part("xl/sharedStrings.xml", b"<sst uniqueCount='1><si/></sst>")
            .is_err()
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/sharedStrings.xml",
            b"<sst uniqueCount='0'><sst uniqueCount='2048'/></sst>"
        )
        .unwrap_err(),
        "nested XLSX sharedStrings root"
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/sharedStrings.xml",
            b"<sst uniqueCount='0'/><sst uniqueCount='2048'/>"
        )
        .unwrap_err(),
        "multiple XLSX XML root elements"
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/sharedStrings.xml",
            b"<sst xmlns:a='urn:test' uniqueCount='0' a:uniqueCount='2048'/>"
        )
        .unwrap_err(),
        "duplicate XLSX sharedStrings uniqueCount"
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/_rels/workbook.xml.rels",
            b"<Relationships xmlns:r='urn:test'><Relationship TargetMode='External' r:TargetMode='Internal' Type='urn:test/worksheet'/></Relationships>"
        )
        .unwrap_err(),
        "unsupported XLSX relationship"
    );
    assert_eq!(
        validate_xlsx_control_part(
            "xl/_rels/workbook.xml.rels",
            b"<Relationships xmlns:r='urn:test'><Relationship Type='urn:test/drawing' r:Type='urn:test/worksheet'/></Relationships>"
        )
        .unwrap_err(),
        "unsupported XLSX relationship"
    );
}

#[test]
fn isolated_parser_runner_enforces_wall_limit() {
    use std::time::Duration;

    assert_eq!(
        run_isolated_parser("hang", None, None, Duration::from_millis(100)).unwrap_err(),
        "isolated parser wall timeout"
    );
}

#[test]
fn isolated_parser_runner_enforces_sampled_rss_limit() {
    let start = std::time::Instant::now();
    assert_eq!(
        run_isolated_parser("memory-hog", None, None, std::time::Duration::from_secs(5))
            .unwrap_err(),
        "isolated parser RSS limit exceeded"
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
}

#[test]
fn isolated_parser_runner_reaps_a_stalled_rss_probe_at_wall_deadline() {
    let child_pids = ObservedChildPids::default();
    let start = std::time::Instant::now();
    assert_eq!(
        run_isolated_parser_with_probe(
            "hang",
            None,
            None,
            std::time::Duration::from_millis(100),
            &RssProbe::Stall(&child_pids),
        )
        .unwrap_err(),
        "isolated parser wall timeout"
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert_child_is_absent(child_pids.parser.load(std::sync::atomic::Ordering::Relaxed));
    assert_child_is_absent(child_pids.probe.load(std::sync::atomic::Ordering::Relaxed));
}

fn assert_child_is_absent(pid: u32) {
    assert_ne!(pid, 0, "child PID was not observed");
    assert!(
        !std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("check child liveness")
            .success(),
        "child {pid} remained alive"
    );
}

#[test]
fn isolated_parser_child() {
    match std::env::var("OFFICE_PARSER_CHILD").as_deref() {
        Ok(mode @ ("parse" | "parse-absolute")) => {
            let source = std::env::var("OFFICE_PARSER_SOURCE").expect("isolated source");
            let output = std::env::var("OFFICE_PARSER_OUTPUT").expect("isolated output");
            let extraction_start = std::time::Instant::now();
            let document = if mode == "parse" {
                extract_office_fixture(&source)
            } else {
                extract_office_path(Path::new(&source))
            }
            .expect("isolated fixture extraction");
            let envelope = ExtractionEnvelope {
                document,
                extraction_nanos: elapsed_nanos(extraction_start),
            };
            let serialized =
                serde_json::to_vec(&envelope).expect("serialize isolated extraction envelope");
            assert!(serialized.len() <= MAX_SERIALIZED_OUTPUT_BYTES);
            std::fs::write(output, serialized).expect("write isolated extraction");
        }
        Ok("hang") => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        Ok("memory-hog") => {
            let mut allocation = vec![0_u8; MEMORY_HOG_BYTES];
            for byte in allocation.iter_mut().step_by(4096) {
                *byte = 1;
            }
            std::hint::black_box(&allocation);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        _ => {}
    }
}

fn isolated_extract(relative: &str) -> Result<Document, String> {
    let output = std::env::temp_dir().join(format!(
        "agentic-memory-office-parser-{}-{}.json",
        std::process::id(),
        relative.replace('/', "-")
    ));
    let _ = std::fs::remove_file(&output);
    let run = run_isolated_parser(
        "parse",
        Some(relative),
        Some(&output),
        std::time::Duration::from_secs(5),
    );
    let bytes = run.and_then(|_| read_bounded_parser_output(&output));
    let _ = std::fs::remove_file(&output);
    serde_json::from_slice::<ExtractionEnvelope>(&bytes?)
        .map(|envelope| envelope.document)
        .map_err(|error| error.to_string())
}

fn isolated_extract_path(path: &Path) -> Result<IsolatedExtraction, String> {
    let output = std::env::temp_dir().join(format!(
        "agentic-memory-office-parser-{}-benchmark.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&output);
    let metrics = run_isolated_parser(
        "parse-absolute",
        path.to_str(),
        Some(&output),
        std::time::Duration::from_secs(5),
    );
    let bytes = metrics
        .and_then(|metrics| read_bounded_parser_output(&output).map(|bytes| (metrics, bytes)));
    let _ = std::fs::remove_file(&output);
    let (metrics, bytes) = bytes?;
    let envelope: ExtractionEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    Ok(IsolatedExtraction {
        document: envelope.document,
        extraction_nanos: envelope.extraction_nanos,
        process_wall_nanos: metrics.process_wall_nanos,
        max_sampled_child_rss_bytes: metrics.max_sampled_child_rss_bytes,
    })
}

fn elapsed_nanos(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn read_bounded_parser_output(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(
            u64::try_from(MAX_SERIALIZED_OUTPUT_BYTES)
                .map_err(|_| "serialized output limit overflow")?
                + 1,
        )
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_SERIALIZED_OUTPUT_BYTES {
        return Err("serialized parser output limit exceeded".into());
    }
    Ok(bytes)
}

fn run_isolated_parser(
    mode: &str,
    source: Option<&str>,
    output: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<ProcessMetrics, String> {
    run_isolated_parser_with_probe(mode, source, output, timeout, &RssProbe::Ps)
}

#[derive(Default)]
struct ObservedChildPids {
    parser: std::sync::atomic::AtomicU32,
    probe: std::sync::atomic::AtomicU32,
}

enum RssProbe<'a> {
    Ps,
    Stall(&'a ObservedChildPids),
}

fn run_isolated_parser_with_probe(
    mode: &str,
    source: Option<&str>,
    output: Option<&Path>,
    timeout: std::time::Duration,
    rss_probe: &RssProbe<'_>,
) -> Result<ProcessMetrics, String> {
    use std::process::{Command, Stdio};

    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command
        .args(["--exact", "isolated_parser_child", "--nocapture"])
        .env("OFFICE_PARSER_CHILD", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(source) = source {
        command.env("OFFICE_PARSER_SOURCE", source);
    }
    if let Some(output) = output {
        command.env("OFFICE_PARSER_OUTPUT", output);
    }
    let process_start = std::time::Instant::now();
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    if let RssProbe::Stall(child_pids) = rss_probe {
        child_pids
            .parser
            .store(child.id(), std::sync::atomic::Ordering::Relaxed);
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut max_sampled_child_rss_bytes: Option<u64> = None;
    loop {
        let Ok(status) = child.try_wait() else {
            kill_and_reap(&mut child)?;
            return Err("isolated parser supervision failed".into());
        };
        if let Some(status) = status {
            return if status.success() {
                Ok(ProcessMetrics {
                    process_wall_nanos: elapsed_nanos(process_start),
                    max_sampled_child_rss_bytes,
                })
            } else {
                Err("isolated parser process rejected".into())
            };
        }
        match sampled_child_total_rss_bytes(child.id(), deadline, rss_probe) {
            Ok(rss) if rss > MAX_ISOLATED_PARSER_RSS_BYTES => {
                kill_and_reap(&mut child)?;
                return Err("isolated parser RSS limit exceeded".into());
            }
            Ok(rss) => {
                max_sampled_child_rss_bytes =
                    Some(max_sampled_child_rss_bytes.map_or(rss, |maximum| maximum.max(rss)));
            }
            Err(RssProbeError::Deadline) => {
                kill_and_reap(&mut child)?;
                return Err("isolated parser wall timeout".into());
            }
            Err(RssProbeError::Failed) => match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    return Ok(ProcessMetrics {
                        process_wall_nanos: elapsed_nanos(process_start),
                        max_sampled_child_rss_bytes,
                    });
                }
                Ok(Some(_)) => return Err("isolated parser process rejected".into()),
                _ => {
                    kill_and_reap(&mut child)?;
                    return Err("isolated parser RSS supervision failed".into());
                }
            },
        }
        if std::time::Instant::now() >= deadline {
            kill_and_reap(&mut child)?;
            return Err("isolated parser wall timeout".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

enum RssProbeError {
    Deadline,
    Failed,
}

fn sampled_child_total_rss_bytes(
    pid: u32,
    deadline: std::time::Instant,
    mode: &RssProbe<'_>,
) -> Result<u64, RssProbeError> {
    use std::{io::Read, process::Stdio};

    let mut command = match mode {
        RssProbe::Ps => {
            let mut command = std::process::Command::new("ps");
            command.args(["-o", "rss=", "-p", &pid.to_string()]);
            command
        }
        RssProbe::Stall(_) => {
            let executable = std::env::current_exe().map_err(|_| RssProbeError::Failed)?;
            let mut command = std::process::Command::new(executable);
            command
                .args(["--exact", "isolated_parser_child", "--nocapture"])
                .env("OFFICE_PARSER_CHILD", "hang");
            command
        }
    };
    let mut probe = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RssProbeError::Failed)?;
    if let RssProbe::Stall(child_pids) = mode {
        child_pids
            .probe
            .store(probe.id(), std::sync::atomic::Ordering::Relaxed);
    }
    loop {
        match probe.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err(RssProbeError::Failed),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(None) => {
                kill_and_reap(&mut probe).map_err(|_| RssProbeError::Failed)?;
                return Err(RssProbeError::Deadline);
            }
            Err(_) => {
                kill_and_reap(&mut probe).map_err(|_| RssProbeError::Failed)?;
                return Err(RssProbeError::Failed);
            }
        }
    }

    let mut stdout = Vec::new();
    probe
        .stdout
        .take()
        .ok_or(RssProbeError::Failed)?
        .read_to_end(&mut stdout)
        .map_err(|_| RssProbeError::Failed)?;
    let kib = String::from_utf8(stdout)
        .map_err(|_| RssProbeError::Failed)?
        .trim()
        .parse::<u64>()
        .map_err(|_| RssProbeError::Failed)?;
    kib.checked_mul(1024).ok_or(RssProbeError::Failed)
}

fn kill_and_reap(child: &mut std::process::Child) -> Result<(), String> {
    let kill = child.kill();
    let wait = child.wait();
    wait.map_err(|error| error.to_string())?;
    match kill {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

type XlsxRewrite<'a> = (&'a str, fn(String) -> String);

fn actual_large_fixtures() -> Vec<ActualLargeFixture> {
    vec![
        ActualLargeFixture {
            format: "pdf",
            extension: "pdf",
            bytes: actual_large_pdf(),
        },
        ActualLargeFixture {
            format: "docx",
            extension: "docx",
            bytes: actual_large_docx(),
        },
        ActualLargeFixture {
            format: "xlsx",
            extension: "xlsx",
            bytes: actual_large_xlsx(),
        },
        ActualLargeFixture {
            format: "csv",
            extension: "csv",
            bytes: actual_large_csv(),
        },
        ActualLargeFixture {
            format: "pptx",
            extension: "pptx",
            bytes: actual_large_pptx(),
        },
    ]
}

fn actual_large_csv() -> Vec<u8> {
    let mut csv = String::from("Account,Owner,Renewal note\n");
    for record in 1..=GENERATED_CSV_RECORDS {
        let note = if record == GENERATED_CSV_RECORDS {
            "ACTUAL CSV LATE SAFFRON"
        } else {
            "Synthetic renewal control"
        };
        writeln!(csv, "Account {record:03},Owner {record:03},{note}").expect("write synthetic CSV");
    }
    csv.into_bytes()
}

fn actual_large_xlsx() -> Vec<u8> {
    let mut rows = String::from(
        "<x:row r=\"1\"><x:c r=\"A1\" t=\"str\"><x:v>Product</x:v></x:c><x:c r=\"B1\" t=\"str\"><x:v>Region</x:v></x:c><x:c r=\"C1\" t=\"str\"><x:v>Note</x:v></x:c></x:row>",
    );
    for row in 2..=GENERATED_XLSX_LAST_ROW {
        let note = if row == GENERATED_XLSX_LAST_ROW {
            "ACTUAL XLSX LATE MULBERRY"
        } else {
            "Synthetic rate control"
        };
        write!(
            rows,
            "<x:row r=\"{row}\"><x:c r=\"A{row}\" t=\"str\"><x:v>Product {row:03}</x:v></x:c><x:c r=\"B{row}\" t=\"str\"><x:v>R{row:03}</x:v></x:c><x:c r=\"C{row}\" t=\"str\"><x:v>{note}</x:v></x:c></x:row>"
        )
        .expect("write synthetic XLSX rows");
    }
    let source = zip_fixture_part("rates.xlsx", "xl/worksheets/sheet1.xml");
    let xml = replace_xml_between(&source, "<x:sheetData>", "</x:sheetData>", &rows);
    replace_fixture_part(
        "rates.xlsx",
        "xl/worksheets/sheet1.xml",
        xml.as_bytes(),
        false,
    )
}

fn actual_large_docx() -> Vec<u8> {
    let source = zip_fixture_part("handbook.docx", "word/document.xml");
    let mut paragraphs = String::new();
    for paragraph in 2..=81 {
        let text = if paragraph == 81 {
            "ACTUAL DOCX LATE JUNIPER"
        } else {
            "Synthetic handbook control paragraph"
        };
        write!(paragraphs, "<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
            .expect("write synthetic DOCX paragraphs");
    }
    let marker = "<w:sectPr";
    let position = source.find(marker).expect("DOCX section marker");
    let mut xml = source;
    xml.insert_str(position, &paragraphs);
    replace_fixture_part("handbook.docx", "word/document.xml", xml.as_bytes(), true)
}

fn actual_large_pptx() -> Vec<u8> {
    let source = zip_fixture_part("briefing.pptx", "ppt/slides/slide2.xml");
    let shape_start = source
        .find("<p:sp><p:nvSpPr><p:cNvPr id=\"2\"")
        .expect("PPTX control shape");
    let shape_end = source[shape_start..]
        .find("</p:sp>")
        .map(|offset| shape_start + offset + "</p:sp>".len())
        .expect("PPTX control shape end");
    let template = &source[shape_start..shape_end];
    let mut shapes = String::new();
    for index in 0..80 {
        let shape_id = 100 + index;
        let text = if index == 79 {
            "ACTUAL PPTX LATE VERMILION"
        } else {
            "Synthetic briefing control"
        };
        let shape = template
            .replacen("id=\"2\"", &format!("id=\"{shape_id}\""), 1)
            .replace("The last-slide marker is INDIGO PUFFIN.", text);
        shapes.push_str(&shape);
    }
    let insertion = source.find("<p:graphicFrame>").expect("PPTX table frame");
    let mut xml = source;
    xml.insert_str(insertion, &shapes);
    replace_fixture_part(
        "briefing.pptx",
        "ppt/slides/slide2.xml",
        xml.as_bytes(),
        true,
    )
}

fn actual_large_pdf() -> Vec<u8> {
    let mut objects = Vec::new();
    let kids = (0..MAX_PDF_PAGES)
        .map(|page| format!("{} 0 R", 4 + page * 2))
        .collect::<Vec<_>>()
        .join(" ");
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(format!("<< /Type /Pages /Kids [{kids}] /Count {MAX_PDF_PAGES} >>").into_bytes());
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    for page in 1..=MAX_PDF_PAGES {
        let content_reference = 5 + (page - 1) * 2;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_reference} 0 R >>"
            )
            .into_bytes(),
        );
        let text = if page == MAX_PDF_PAGES {
            "ACTUAL PDF LATE MARIGOLD"
        } else {
            "Synthetic PDF control page"
        };
        let stream = format!("BT /F1 12 Tf 72 700 Td ({text}) Tj ET");
        objects.push(
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            )
            .into_bytes(),
        );
    }
    pdf_from_objects(&objects)
}

fn zip_fixture_part(fixture: &str, target: &str) -> String {
    use std::io::{Cursor, Read};

    let bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/office")
            .join(fixture),
    )
    .expect("read office fixture");
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("open office fixture");
    let mut text = String::new();
    archive
        .by_name(target)
        .expect("fixture part")
        .read_to_string(&mut text)
        .expect("UTF-8 fixture part");
    text
}

fn replace_xml_between(source: &str, start: &str, end: &str, replacement: &str) -> String {
    let content_start = source.find(start).expect("XML start marker") + start.len();
    let content_end = source[content_start..]
        .find(end)
        .map(|offset| content_start + offset)
        .expect("XML end marker");
    format!(
        "{}{}{}",
        &source[..content_start],
        replacement,
        &source[content_end..]
    )
}

fn replace_fixture_part(
    fixture: &str,
    target: &str,
    replacement: &[u8],
    compressed: bool,
) -> Vec<u8> {
    use std::io::{Cursor, Read, Write};
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    let source = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/office")
            .join(fixture),
    )
    .expect("read office fixture");
    let mut input = ZipArchive::new(Cursor::new(source)).expect("open office fixture");
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    let method = if compressed {
        CompressionMethod::Deflated
    } else {
        CompressionMethod::Stored
    };
    let options = SimpleFileOptions::default().compression_method(method);
    for index in 0..input.len() {
        let mut entry = input.by_index(index).expect("office fixture entry");
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read office part");
        output
            .start_file(&name, options)
            .expect("start office part");
        output
            .write_all(if name == target { replacement } else { &bytes })
            .expect("write office part");
    }
    output.finish().expect("finish office fixture").into_inner()
}

fn expanded_container_bytes(fixture: &ActualLargeFixture) -> Option<u64> {
    use std::io::Cursor;

    matches!(fixture.format, "docx" | "xlsx" | "pptx").then(|| {
        let mut archive = zip::ZipArchive::new(Cursor::new(&fixture.bytes)).expect("generated ZIP");
        (0..archive.len())
            .map(|index| archive.by_index(index).expect("generated ZIP entry").size())
            .sum()
    })
}

fn rewritten_xlsx(replacement: Option<XlsxRewrite<'_>>, added: Option<(&str, &str)>) -> Vec<u8> {
    use std::io::{Cursor, Read, Write};
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    let source = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/office/rates.xlsx"),
    )
    .expect("read positive XLSX fixture");
    let mut input = ZipArchive::new(Cursor::new(source)).expect("open positive XLSX fixture");
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for index in 0..input.len() {
        let mut entry = input.by_index(index).expect("read XLSX fixture entry");
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .expect("decompress XLSX fixture entry");
        if replacement.is_some_and(|(target, _)| target == name) {
            let (_, rewrite) = replacement.expect("matching replacement");
            bytes = rewrite(String::from_utf8(bytes).expect("UTF-8 XML")).into_bytes();
        }
        output
            .start_file(name, options)
            .expect("start XLSX fixture entry");
        output.write_all(&bytes).expect("write XLSX fixture entry");
    }
    if let Some((name, contents)) = added {
        output
            .start_file(name, options)
            .expect("start added XLSX entry");
        output
            .write_all(contents.as_bytes())
            .expect("write added XLSX entry");
    }
    output.finish().expect("finish XLSX fixture").into_inner()
}

fn rewritten_office(
    fixture: &str,
    replacement: Option<XlsxRewrite<'_>>,
    added: Option<(&str, &[u8])>,
) -> Vec<u8> {
    use std::io::{Cursor, Read, Write};
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    let source = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/office")
            .join(fixture),
    )
    .expect("read positive OOXML fixture");
    let mut input = ZipArchive::new(Cursor::new(source)).expect("open positive OOXML fixture");
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for index in 0..input.len() {
        let mut entry = input.by_index(index).expect("read OOXML fixture entry");
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read OOXML entry");
        if replacement.is_some_and(|(target, _)| target == name) {
            let (_, rewrite) = replacement.expect("matching replacement");
            bytes = rewrite(String::from_utf8(bytes).expect("UTF-8 OOXML part")).into_bytes();
        }
        output.start_file(name, options).expect("start OOXML entry");
        output.write_all(&bytes).expect("write OOXML entry");
    }
    if let Some((name, contents)) = added {
        output
            .start_file(name, options)
            .expect("start added OOXML entry");
        output.write_all(contents).expect("write added OOXML entry");
    }
    output.finish().expect("finish OOXML fixture").into_inner()
}

fn office_with_duplicate_entries(fixture: &str, name: &str, count: usize) -> Vec<u8> {
    use std::io::{Cursor, Read, Write};
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    let source = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/office")
            .join(fixture),
    )
    .expect("read positive OOXML fixture");
    let mut input = ZipArchive::new(Cursor::new(source)).expect("open positive OOXML fixture");
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut placeholders = Vec::with_capacity(count);
    for index in 0..count {
        let placeholder = format!("word/{index:08}.xml");
        assert_eq!(placeholder.len(), name.len(), "test ZIP name lengths");
        output
            .start_file(&placeholder, options)
            .expect("start shadow entry");
        output.write_all(b"shadow").expect("write duplicate entry");
        placeholders.push(placeholder);
    }
    for index in 0..input.len() {
        let mut entry = input.by_index(index).expect("read OOXML fixture entry");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read OOXML entry");
        output
            .start_file(entry.name(), options)
            .expect("start original OOXML entry");
        output
            .write_all(&bytes)
            .expect("write original OOXML entry");
    }
    let mut bytes = output.finish().expect("finish OOXML fixture").into_inner();
    for placeholder in placeholders {
        let positions = bytes
            .windows(placeholder.len())
            .enumerate()
            .filter_map(|(offset, candidate)| {
                (candidate == placeholder.as_bytes()).then_some(offset)
            })
            .collect::<Vec<_>>();
        for offset in &positions {
            let end = offset + name.len();
            bytes[*offset..end].copy_from_slice(name.as_bytes());
        }
        assert_eq!(positions.len(), 2, "local and central ZIP names replaced");
    }
    bytes
}

fn office_zip_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("test ZIP u16"))
}

fn office_zip_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("test ZIP u32"))
}

fn office_put_zip_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn office_put_zip_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn office_central_directory_offset(bytes: &[u8]) -> usize {
    let eocd = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .expect("test ZIP EOCD");
    usize::try_from(office_zip_u32(bytes, eocd + 16)).expect("test ZIP central offset")
}

fn office_central_record_offset(bytes: &[u8], expected_name: &str) -> usize {
    let eocd = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .expect("test ZIP EOCD");
    let mut cursor = office_central_directory_offset(bytes);
    for _ in 0..office_zip_u16(bytes, eocd + 10) {
        assert_eq!(&bytes[cursor..cursor + 4], b"PK\x01\x02");
        let name_length = usize::from(office_zip_u16(bytes, cursor + 28));
        let extra_length = usize::from(office_zip_u16(bytes, cursor + 30));
        let comment_length = usize::from(office_zip_u16(bytes, cursor + 32));
        let name_start = cursor + 46;
        let name_end = name_start + name_length;
        if &bytes[name_start..name_end] == expected_name.as_bytes() {
            return cursor;
        }
        cursor = name_end + extra_length + comment_length;
    }
    panic!("test ZIP central record missing: {expected_name}")
}

fn assert_ooxml_rejected(bytes: &[u8], kind: OfficeKind, expected: &str) {
    let path = temporary_office_path("negative", "zip", bytes.len());
    std::fs::write(&path, bytes).expect("write negative OOXML fixture");
    let error = preflight_ooxml(&path, kind).expect_err("negative OOXML must reject");
    std::fs::remove_file(path).expect("remove negative OOXML fixture");
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error:?}"
    );
}

fn extract_rewritten_office(bytes: &[u8], kind: OfficeKind) -> Result<Document, String> {
    let extension = match kind {
        OfficeKind::Docx => "docx",
        OfficeKind::Pptx => "pptx",
    };
    let path = temporary_office_path("rewritten", extension, bytes.len());
    std::fs::write(&path, bytes).expect("write rewritten OOXML fixture");
    let result = match kind {
        OfficeKind::Docx => extract_docx(&path, hex_digest(bytes)),
        OfficeKind::Pptx => extract_pptx(&path, hex_digest(bytes)),
    };
    std::fs::remove_file(path).expect("remove rewritten OOXML fixture");
    result
}

fn temporary_office_path(label: &str, extension: &str, size: usize) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agentic-memory-{label}-{}-{size}.{extension}",
        std::process::id()
    ))
}

fn mixed_text_image_pdf() -> Vec<u8> {
    let content =
        b"BT /F1 12 Tf 72 700 Td (VISIBLE TEXT WITH IMAGE) Tj ET q 100 0 0 100 72 500 cm /Im1 Do Q";
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 5 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        [
            format!("<< /Length {} >>\nstream\n", content.len()).as_bytes(),
            content,
            b"\nendstream",
        ]
        .concat(),
        b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length 3 >>\nstream\n\xff\0\0\nendstream".to_vec(),
    ];
    pdf_from_objects(&objects)
}

fn inline_image_pdf() -> Vec<u8> {
    let content = b"BT /F1 12 Tf 72 700 Td (VISIBLE INLINE IMAGE TEXT) Tj ET BI /W 1 /H 1 /BPC 8 /CS /RGB ID abc EI";
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        [
            format!("<< /Length {} >>\nstream\n", content.len()).as_bytes(),
            content,
            b"\nendstream",
        ]
        .concat(),
    ];
    pdf_from_objects(&objects)
}

fn pdf_from_objects(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0_usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn extract_office_fixture(relative: &str) -> Result<Document, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(relative);
    extract_office_path(&path)
}

fn extract_office_path(path: &Path) -> Result<Document, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("source byte limit exceeded".into());
    }
    let hash = hex_digest(&bytes);
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("txt" | "md") => extract_text_with_hash(&bytes, hash),
        Some("csv") => extract_csv(&bytes, &hash),
        Some("xlsx") => extract_xlsx(path, hash),
        Some("pdf") => extract_pdf(path, hash),
        Some("docx") => extract_docx(path, hash),
        Some("pptx") => extract_pptx(path, hash),
        _ => Err("unsupported fixture type".into()),
    }
}

fn extract_text(bytes: &[u8]) -> Result<Document, String> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("source byte limit exceeded".into());
    }
    extract_text_with_hash(bytes, hex_digest(bytes))
}

fn extract_text_with_hash(bytes: &[u8], hash: String) -> Result<Document, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "text must be UTF-8")?
        .trim_end_matches(['\r', '\n']);
    if text.is_empty() {
        return Err("empty text is unsupported".into());
    }
    let lines = text.lines().count();
    if lines > MAX_ROWS || text.len() > MAX_OUTPUT_BYTES {
        return Err("text row/output limit exceeded".into());
    }
    Ok(document(
        hash,
        vec![Passage {
            locator: format!("line=1-{lines}"),
            text: text.to_owned(),
            cells: vec![],
        }],
        vec![],
    ))
}

fn extract_pdf(path: &Path, hash: String) -> Result<Document, String> {
    use oxidize_pdf::{
        parser::{ContentOperation, ContentParser, PdfDocument},
        pipeline::Element,
    };

    let pdf = PdfDocument::open(path).map_err(|error| error.to_string())?;
    let page_count = usize::try_from(pdf.page_count().map_err(|error| error.to_string())?)
        .map_err(|_| "PDF page count overflow")?;
    if page_count == 0 || page_count > MAX_PDF_PAGES {
        return Err("PDF page limit exceeded".into());
    }
    for page in 0..u32::try_from(page_count).map_err(|_| "PDF page count overflow")? {
        let parsed_page = pdf.get_page(page).map_err(|error| error.to_string())?;
        if parsed_page
            .get_resources()
            .and_then(|resources| resources.get("XObject"))
            .and_then(oxidize_pdf::parser::PdfObject::as_dict)
            .is_some_and(|xobjects| !xobjects.0.is_empty())
        {
            return Err("PDF XObject/image content is unsupported".into());
        }
        for stream in parsed_page
            .content_streams_with_document(&pdf)
            .map_err(|error| error.to_string())?
        {
            let operations = ContentParser::parse(&stream).map_err(|error| error.to_string())?;
            if operations.iter().any(|operation| {
                matches!(
                    operation,
                    ContentOperation::BeginInlineImage
                        | ContentOperation::InlineImage { .. }
                        | ContentOperation::PaintXObject(_)
                )
            }) {
                return Err("PDF image/XObject operator is unsupported".into());
            }
        }
        if !pdf
            .get_page_annotations(page)
            .map_err(|error| error.to_string())?
            .is_empty()
        {
            return Err("PDF annotations/forms are unsupported".into());
        }
    }
    let elements = pdf.partition().map_err(|error| error.to_string())?;
    let mut per_page_blocks = vec![0_usize; page_count];
    let mut passages = Vec::new();
    let mut retained = 0_usize;
    for element in elements {
        if matches!(element, Element::Image(_)) {
            return Err("PDF visual content is unsupported".into());
        }
        let text = element.display_text().trim().to_owned();
        if text.is_empty() {
            continue;
        }
        let page = usize::try_from(element.page()).map_err(|_| "PDF page number overflow")?;
        if page >= page_count {
            return Err("PDF element page is out of range".into());
        }
        per_page_blocks[page] += 1;
        let bbox = element.bbox();
        let locator = format!(
            "page={};block={};type={};bbox={:.1},{:.1},{:.1},{:.1}",
            page + 1,
            per_page_blocks[page],
            element.type_name(),
            bbox.x,
            bbox.y,
            bbox.width,
            bbox.height
        );
        retained = retained
            .checked_add(locator.len() + text.len())
            .ok_or("PDF output size overflow")?;
        if retained > MAX_OUTPUT_BYTES {
            return Err("PDF output limit exceeded".into());
        }
        passages.push(Passage {
            locator,
            text,
            cells: vec![],
        });
    }
    if per_page_blocks.contains(&0) {
        return Err("PDF page without extractable text is unsupported".into());
    }
    Ok(document(hash, passages, vec![]))
}

#[allow(clippy::too_many_lines)] // Keep the narrow typed admission and locator construction auditable together.
fn extract_docx(path: &Path, hash: String) -> Result<Document, String> {
    use ooxmlsdk::schemas::w::{BodyChoice, TableCellChoice, TableChoice2, TableRowChoice};

    preflight_ooxml(path, OfficeKind::Docx)?;
    let package = WordprocessingDocument::new_from_file(path).map_err(|error| error.to_string())?;
    let main = package
        .main_document_part()
        .map_err(|error| error.to_string())?;
    let root = main
        .root_element(&package)
        .map_err(|error| error.to_string())?;
    let body = root.body.as_deref().ok_or("DOCX body is missing")?;
    let mut block_count = 0_usize;
    for choice in &body.body_choice {
        match choice {
            BodyChoice::Paragraph(_) => block_count += 1,
            BodyChoice::Table(table) => {
                let grid = table
                    .table_grid
                    .as_ref()
                    .ok_or("DOCX table grid is missing")?;
                if grid.grid_column.len() != 2
                    || !grid.out_of_place_table_column.is_empty()
                    || grid.table_grid_change.is_some()
                {
                    return Err("unsupported DOCX table grid/revision".into());
                }
                let row_count = table.table_choice2.len();
                let mut cell_count = 0_usize;
                for row in &table.table_choice2 {
                    let TableChoice2::TableRow(row) = row else {
                        return Err("unsupported DOCX table content".into());
                    };
                    if row.table_row_properties.is_some()
                        || row.table_property_exceptions.is_some()
                        || row.rsid_table_row_addition.is_some()
                        || row.rsid_table_row_deletion.is_some()
                        || row.rsid_table_row_properties.is_some()
                    {
                        return Err("unsupported DOCX table row properties/revision".into());
                    }
                    for cell in &row.table_row_choice {
                        let TableRowChoice::TableCell(cell) = cell else {
                            return Err("unsupported DOCX table row content".into());
                        };
                        if let Some(properties) = &cell.table_cell_properties
                            && (properties.grid_span.is_some()
                                || properties.horizontal_merge.is_some()
                                || properties.vertical_merge.is_some()
                                || properties.table_cell_properties_choice.is_some()
                                || properties.table_cell_properties_change.is_some())
                        {
                            return Err("unsupported DOCX merged/revised table cell".into());
                        }
                        cell_count = cell_count
                            .checked_add(1)
                            .ok_or("DOCX table cell count overflow")?;
                        block_count = block_count
                            .checked_add(cell.table_cell_choice.len())
                            .ok_or("DOCX block count overflow")?;
                    }
                }
                if row_count > MAX_OFFICE_TABLE_ROWS || cell_count > MAX_OFFICE_TABLE_CELLS {
                    return Err("DOCX table dimension limit exceeded".into());
                }
            }
            _ => return Err("unsupported DOCX body content or tracked change".into()),
        }
    }
    if block_count > MAX_OFFICE_BLOCKS {
        return Err("DOCX block limit exceeded".into());
    }
    let mut heading_path = Vec::new();
    let mut passages = Vec::new();
    let mut paragraph_number = 0_usize;
    let mut table_number = 0_usize;
    let mut retained = 0_usize;
    for choice in &body.body_choice {
        match choice {
            BodyChoice::Paragraph(paragraph) => {
                let text = word_paragraph_text(paragraph.as_ref())?;
                if text.is_empty() {
                    continue;
                }
                let style = paragraph
                    .paragraph_properties
                    .as_deref()
                    .and_then(|properties| properties.paragraph_style_id.as_ref())
                    .map(|style| style.val.as_str());
                if matches!(style, Some("Title" | "Heading1")) {
                    if style == Some("Title") {
                        heading_path.clear();
                    } else {
                        heading_path.truncate(1);
                    }
                    retain_office_text(&mut retained, &text)?;
                    heading_path.push(text);
                    continue;
                }
                paragraph_number += 1;
                let locator = format!(
                    "heading={};paragraph={paragraph_number}",
                    heading_path.join("/")
                );
                retain_office_text(&mut retained, &format!("{locator}\n{text}"))?;
                passages.push(Passage {
                    locator,
                    text,
                    cells: vec![],
                });
            }
            BodyChoice::Table(table) => {
                table_number += 1;
                if !table.table_choice1.is_empty() {
                    return Err("unsupported DOCX table revision metadata".into());
                }
                let rows = table
                    .table_choice2
                    .iter()
                    .map(|row| match row {
                        TableChoice2::TableRow(row) => row
                            .table_row_choice
                            .iter()
                            .map(|cell| match cell {
                                TableRowChoice::TableCell(cell) => {
                                    let mut paragraphs = Vec::new();
                                    for block in &cell.table_cell_choice {
                                        match block {
                                            TableCellChoice::Paragraph(paragraph) => {
                                                let text = word_paragraph_text(paragraph)?;
                                                if !text.is_empty() {
                                                    paragraphs.push(text);
                                                }
                                            }
                                            _ => {
                                                return Err(
                                                    "unsupported DOCX table cell content".into()
                                                );
                                            }
                                        }
                                    }
                                    Ok(paragraphs.join("\n"))
                                }
                                _ => Err("unsupported DOCX table row content".into()),
                            })
                            .collect::<Result<Vec<_>, String>>(),
                        _ => Err("unsupported DOCX table content".into()),
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                if rows.len() != 2 || rows[0].len() != 2 || rows[1].len() != 2 {
                    return Err("DOCX pilot tables require one two-column data row".into());
                }
                let text = format!(
                    "{}: {}\n{}: {}",
                    rows[0][0], rows[1][0], rows[0][1], rows[1][1]
                );
                let locator = format!(
                    "heading={};table={table_number};row=2;headers=A1:B1;cells=A2:B2",
                    heading_path.join("/")
                );
                retain_office_text(&mut retained, &format!("{locator}\n{text}"))?;
                passages.push(Passage {
                    locator,
                    text,
                    cells: vec![],
                });
            }
            _ => return Err("unsupported DOCX body content or tracked change".into()),
        }
    }
    if passages.is_empty() {
        return Err("DOCX has no supported text passages".into());
    }
    Ok(document(hash, passages, vec![]))
}

fn word_paragraph_text(paragraph: &ooxmlsdk::schemas::w::Paragraph) -> Result<String, String> {
    use ooxmlsdk::schemas::w::{ParagraphChoice, RunChoice};

    let mut text = String::new();
    for choice in &paragraph.paragraph_choice {
        match choice {
            ParagraphChoice::WRun(run) => {
                for run_choice in &run.run_choice {
                    match run_choice {
                        RunChoice::Text(value) => {
                            if let Some(value) = &value.0.xml_content {
                                text.push_str(value);
                            }
                        }
                        _ => return Err("unsupported DOCX run content".into()),
                    }
                }
            }
            _ => return Err("unsupported DOCX paragraph content or tracked change".into()),
        }
    }
    Ok(text)
}

fn retain_office_text(retained: &mut usize, text: &str) -> Result<(), String> {
    *retained = retained
        .checked_add(text.len())
        .ok_or("Office output size overflow")?;
    if *retained > MAX_OUTPUT_BYTES {
        return Err("Office output limit exceeded".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum OfficeKind {
    Docx,
    Pptx,
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // OPC part names are exact admission identifiers.
fn preflight_ooxml(path: &Path, kind: OfficeKind) -> Result<(), String> {
    use std::io::Read;

    let mut raw = Vec::new();
    std::fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(u64::try_from(MAX_SOURCE_BYTES).map_err(|_| "source limit overflow")? + 1)
        .read_to_end(&mut raw)
        .map_err(|error| error.to_string())?;
    if raw.len() > MAX_SOURCE_BYTES {
        return Err("source byte limit exceeded".into());
    }
    validate_raw_office_zip(&raw)?;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(raw)).map_err(|error| error.to_string())?;
    if archive.len() > MAX_OFFICE_ZIP_ENTRIES {
        return Err("OOXML ZIP entry limit exceeded".into());
    }
    let mut expanded = 0_u64;
    let mut parts = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry.name().to_owned();
        if !supported_office_component(&name, kind) {
            return Err(format!("unsupported OOXML component: {name}"));
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(MAX_OFFICE_ZIP_ENTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_OFFICE_ZIP_ENTRY_BYTES {
            return Err("OOXML ZIP entry size limit exceeded".into());
        }
        expanded = expanded
            .checked_add(bytes.len() as u64)
            .ok_or("OOXML ZIP expanded size overflow")?;
        if expanded > MAX_OFFICE_ZIP_EXPANDED_BYTES {
            return Err("OOXML ZIP expanded size limit exceeded".into());
        }
        if parts.insert(name.clone(), bytes.clone()).is_some() {
            return Err("duplicate OOXML component".into());
        }
        if name.ends_with(".rels") || name == "[Content_Types].xml" {
            validate_office_admission_xml(&name, &bytes)?;
        }
        if name.ends_with(".xml") && name != "[Content_Types].xml" {
            validate_office_xml_component(&name, &bytes, kind)?;
        }
        if matches!(kind, OfficeKind::Pptx)
            && (name.starts_with("ppt/notesSlides/") || name.starts_with("ppt/notesMasters/"))
            && name.ends_with(".xml")
        {
            reject_nonempty_notes(&bytes)?;
        }
        if let Some(expected) = fixed_office_component_hash(&name)
            && hex_digest(&bytes) != expected
        {
            return Err(format!(
                "unsupported invariant OOXML component content: {name}"
            ));
        }
    }
    validate_ooxml_package(&parts, kind)
}

fn validate_raw_office_zip(bytes: &[u8]) -> Result<(), String> {
    use std::io::Read;
    use zip::{
        result::ZipError,
        unstable::stream::{ZipStreamFileMetadata, ZipStreamReader, ZipStreamVisitor},
    };

    struct Visitor {
        local_names: Vec<Vec<u8>>,
        expected: Vec<(Vec<u8>, u64)>,
        expanded: u64,
    }

    impl ZipStreamVisitor for Visitor {
        fn visit_file<R: Read>(
            &mut self,
            file: &mut zip::read::ZipFile<'_, R>,
        ) -> zip::result::ZipResult<()> {
            if self.local_names.len() >= MAX_OFFICE_ZIP_ENTRIES {
                return Err(ZipError::InvalidArchive(
                    "OOXML raw ZIP entry limit exceeded".into(),
                ));
            }
            let name = file.name_raw().to_vec();
            let expected_size = self
                .expected
                .get(self.local_names.len())
                .filter(|(expected_name, _)| *expected_name == name)
                .map(|(_, size)| *size)
                .ok_or(ZipError::InvalidArchive(
                    "OOXML ZIP decompressed record order/name mismatch".into(),
                ))?;
            if self.local_names.contains(&name) {
                return Err(ZipError::InvalidArchive(
                    "duplicate raw OOXML ZIP entry".into(),
                ));
            }
            self.local_names.push(name);
            let read = std::io::copy(
                &mut file.take(MAX_OFFICE_ZIP_ENTRY_BYTES + 1),
                &mut std::io::sink(),
            )?;
            if read > MAX_OFFICE_ZIP_ENTRY_BYTES {
                return Err(ZipError::InvalidArchive(
                    "OOXML raw ZIP entry size limit exceeded".into(),
                ));
            }
            if read != expected_size {
                return Err(ZipError::InvalidArchive(
                    "OOXML ZIP actual/declaration size mismatch".into(),
                ));
            }
            self.expanded = self
                .expanded
                .checked_add(read)
                .ok_or(ZipError::InvalidArchive(
                    "OOXML raw ZIP size overflow".into(),
                ))?;
            if self.expanded > MAX_OFFICE_ZIP_EXPANDED_BYTES {
                return Err(ZipError::InvalidArchive(
                    "OOXML raw ZIP expanded limit exceeded".into(),
                ));
            }
            Ok(())
        }

        fn visit_additional_metadata(
            &mut self,
            _metadata: &ZipStreamFileMetadata,
        ) -> zip::result::ZipResult<()> {
            Ok(())
        }
    }

    let central_records = validate_classic_zip_records(bytes)?;
    let expected = central_records
        .iter()
        .map(|record| (record.name.clone(), u64::from(record.uncompressed)))
        .collect();
    let mut visitor = Visitor {
        local_names: Vec::new(),
        expected,
        expanded: 0,
    };
    ZipStreamReader::new(std::io::Cursor::new(bytes))
        .visit(&mut visitor)
        .map_err(|error| error.to_string())?;
    if visitor.local_names.is_empty() || visitor.local_names.len() != central_records.len() {
        return Err("OOXML ZIP local/central directory mismatch".into());
    }
    Ok(())
}

struct OfficeZipCentralRecord {
    name: Vec<u8>,
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u32,
    uncompressed: u32,
    local_offset: usize,
}

#[allow(clippy::too_many_lines)] // Kept linear so every bounded classic-ZIP field check is auditable.
fn validate_classic_zip_records(bytes: &[u8]) -> Result<Vec<OfficeZipCentralRecord>, String> {
    const EOCD_SIZE: usize = 22;
    const CENTRAL_HEADER_SIZE: usize = 46;
    let search_start = bytes.len().saturating_sub(EOCD_SIZE + u16::MAX as usize);
    let eocd = (search_start..=bytes.len().saturating_sub(EOCD_SIZE))
        .rev()
        .find(|&offset| {
            offset.checked_add(4).and_then(|end| bytes.get(offset..end)) == Some(b"PK\x05\x06")
        })
        .ok_or("OOXML ZIP EOCD missing")?;
    let read_u16 = |offset: usize| -> Result<u16, String> {
        let end = offset.checked_add(2).ok_or("OOXML ZIP offset overflow")?;
        let value = bytes.get(offset..end).ok_or("truncated OOXML ZIP EOCD")?;
        Ok(u16::from_le_bytes([value[0], value[1]]))
    };
    let read_u32 = |offset: usize| -> Result<u32, String> {
        let end = offset.checked_add(4).ok_or("OOXML ZIP offset overflow")?;
        let value = bytes
            .get(offset..end)
            .ok_or("truncated OOXML ZIP metadata")?;
        Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
    };
    let eocd_field = |delta: usize| eocd.checked_add(delta).ok_or("ZIP EOCD overflow");
    let disk = read_u16(eocd_field(4)?)?;
    let central_disk = read_u16(eocd_field(6)?)?;
    let disk_entries = read_u16(eocd_field(8)?)?;
    let total_entries = read_u16(eocd_field(10)?)?;
    let central_size = read_u32(eocd_field(12)?)?;
    let central_offset = read_u32(eocd_field(16)?)?;
    let comment_length = usize::from(read_u16(eocd_field(20)?)?);
    if disk != 0
        || central_disk != 0
        || disk_entries != total_entries
        || total_entries == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX
    {
        return Err("multi-disk/Zip64 OOXML archives are unsupported".into());
    }
    if usize::from(total_entries) > MAX_OFFICE_ZIP_ENTRIES {
        return Err("OOXML ZIP central entry limit exceeded".into());
    }
    if comment_length != 0 || eocd.checked_add(EOCD_SIZE + comment_length) != Some(bytes.len()) {
        return Err("invalid OOXML ZIP EOCD bounds".into());
    }
    let mut cursor = usize::try_from(central_offset).map_err(|_| "ZIP offset overflow")?;
    let central_end = cursor
        .checked_add(usize::try_from(central_size).map_err(|_| "ZIP size overflow")?)
        .ok_or("ZIP central bounds overflow")?;
    if central_end != eocd || central_end > bytes.len() {
        return Err("invalid OOXML ZIP central-directory bounds".into());
    }
    let mut records = Vec::with_capacity(usize::from(total_entries));
    let mut names = Vec::with_capacity(usize::from(total_entries));
    let mut local_offsets = BTreeSet::new();
    let mut expanded = 0_u64;
    for _ in 0..total_entries {
        let signature_end = cursor.checked_add(4).ok_or("ZIP central header overflow")?;
        if bytes.get(cursor..signature_end) != Some(b"PK\x01\x02") {
            return Err("invalid OOXML ZIP central header".into());
        }
        let central_field = |delta: usize| {
            cursor
                .checked_add(delta)
                .ok_or("ZIP central field overflow")
        };
        let flags = read_u16(central_field(8)?)?;
        let method = read_u16(central_field(10)?)?;
        let crc = read_u32(central_field(16)?)?;
        let compressed = read_u32(central_field(20)?)?;
        let uncompressed = read_u32(central_field(24)?)?;
        let entry_disk = read_u16(central_field(34)?)?;
        let local_offset = usize::try_from(read_u32(central_field(42)?)?)
            .map_err(|_| "ZIP local offset overflow")?;
        if flags & !0x0800 != 0 {
            return Err("encrypted/data-descriptor/unsupported ZIP flags".into());
        }
        if !matches!(method, 0 | 8) {
            return Err("unsupported OOXML ZIP compression method".into());
        }
        if entry_disk != 0 {
            return Err("multi-disk OOXML central entry is unsupported".into());
        }
        if compressed == u32::MAX
            || uncompressed == u32::MAX
            || u64::from(uncompressed) > MAX_OFFICE_ZIP_ENTRY_BYTES
        {
            return Err("Zip64/oversized OOXML central entry is unsupported".into());
        }
        expanded = expanded
            .checked_add(u64::from(uncompressed))
            .ok_or("OOXML central expanded size overflow")?;
        if expanded > MAX_OFFICE_ZIP_EXPANDED_BYTES {
            return Err("OOXML central expanded size limit exceeded".into());
        }
        let name_length = usize::from(read_u16(central_field(28)?)?);
        let extra_length = usize::from(read_u16(central_field(30)?)?);
        let comment_length = usize::from(read_u16(central_field(32)?)?);
        if extra_length != 0 || comment_length != 0 {
            return Err("OOXML ZIP entry extras/comments are unsupported".into());
        }
        let name_start = cursor
            .checked_add(CENTRAL_HEADER_SIZE)
            .ok_or("ZIP central name overflow")?;
        let name_end = name_start
            .checked_add(name_length)
            .ok_or("ZIP central name overflow")?;
        let next = name_end
            .checked_add(extra_length)
            .and_then(|value| value.checked_add(comment_length))
            .ok_or("ZIP central entry overflow")?;
        let name = bytes
            .get(name_start..name_end)
            .ok_or("truncated OOXML ZIP central name")?
            .to_vec();
        if names.contains(&name) {
            return Err("duplicate central OOXML ZIP entry".into());
        }
        if !local_offsets.insert(local_offset) {
            return Err("duplicate OOXML ZIP local-header offset".into());
        }
        names.push(name.clone());
        records.push(OfficeZipCentralRecord {
            name,
            flags,
            method,
            crc,
            compressed,
            uncompressed,
            local_offset,
        });
        cursor = next;
    }
    if cursor != central_end {
        return Err("OOXML ZIP central entry count mismatch".into());
    }
    records.sort_by_key(|record| record.local_offset);
    let mut expected_offset = 0_usize;
    for record in &records {
        if record.local_offset != expected_offset {
            return Err("OOXML ZIP local records are not contiguous".into());
        }
        let local_field = |delta: usize| {
            record
                .local_offset
                .checked_add(delta)
                .ok_or("ZIP local field overflow")
        };
        let signature_end = local_field(4)?;
        if bytes.get(record.local_offset..signature_end) != Some(b"PK\x03\x04") {
            return Err("invalid OOXML ZIP local header".into());
        }
        let local_flags = read_u16(local_field(6)?)?;
        let local_method = read_u16(local_field(8)?)?;
        let local_crc = read_u32(local_field(14)?)?;
        let local_compressed = read_u32(local_field(18)?)?;
        let local_uncompressed = read_u32(local_field(22)?)?;
        let local_name_length = usize::from(read_u16(local_field(26)?)?);
        let local_extra_length = usize::from(read_u16(local_field(28)?)?);
        if local_extra_length != 0 {
            return Err("OOXML ZIP local extra fields are unsupported".into());
        }
        let name_start = local_field(30)?;
        let name_end = name_start
            .checked_add(local_name_length)
            .ok_or("ZIP local name overflow")?;
        let local_name = bytes
            .get(name_start..name_end)
            .ok_or("truncated OOXML ZIP local name")?;
        if local_name != record.name
            || local_flags != record.flags
            || local_method != record.method
            || local_crc != record.crc
            || local_compressed != record.compressed
            || local_uncompressed != record.uncompressed
        {
            return Err("OOXML ZIP local/central record mismatch".into());
        }
        expected_offset = name_end
            .checked_add(
                usize::try_from(record.compressed).map_err(|_| "ZIP compressed size overflow")?,
            )
            .ok_or("ZIP local record overflow")?;
        if expected_offset > central_end {
            return Err("OOXML ZIP local record exceeds central directory".into());
        }
    }
    if expected_offset != usize::try_from(central_offset).map_err(|_| "ZIP offset overflow")? {
        return Err("OOXML ZIP local records do not end at central directory".into());
    }
    Ok(records)
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn supported_office_component(name: &str, kind: OfficeKind) -> bool {
    match kind {
        OfficeKind::Docx => matches!(
            name,
            "[Content_Types].xml"
                | "_rels/.rels"
                | "docProps/core.xml"
                | "docProps/app.xml"
                | "docProps/thumbnail.jpeg"
                | "word/document.xml"
                | "word/_rels/document.xml.rels"
                | "word/styles.xml"
                | "word/stylesWithEffects.xml"
                | "word/settings.xml"
                | "word/webSettings.xml"
                | "word/fontTable.xml"
                | "word/theme/theme1.xml"
                | "word/numbering.xml"
                | "customXml/item1.xml"
                | "customXml/_rels/item1.xml.rels"
                | "customXml/itemProps1.xml"
        ),
        OfficeKind::Pptx => {
            matches!(
                name,
                "[Content_Types].xml"
                    | "_rels/.rels"
                    | "docProps/core.xml"
                    | "docProps/app.xml"
                    | "ppt/presentation.xml"
                    | "ppt/theme/theme1.xml"
                    | "ppt/slideMasters/slideMaster1.xml"
                    | "ppt/slideMasters/theme/theme2.xml"
                    | "ppt/slideLayouts/slideLayout1.xml"
                    | "ppt/notesMasters/notesMaster1.xml"
                    | "ppt/notesMasters/theme/theme3.xml"
                    | "ppt/presProps.xml"
                    | "ppt/tableStyles.xml"
                    | "ppt/notesMasters/_rels/notesMaster1.xml.rels"
                    | "ppt/slideLayouts/_rels/slideLayout1.xml.rels"
                    | "ppt/slideMasters/_rels/slideMaster1.xml.rels"
                    | "ppt/_rels/presentation.xml.rels"
                    | "ppt/slides/slide1.xml"
                    | "ppt/slides/slide2.xml"
                    | "ppt/notesSlides/notesSlide1.xml"
                    | "ppt/notesSlides/notesSlide2.xml"
                    | "ppt/slides/_rels/slide1.xml.rels"
                    | "ppt/slides/_rels/slide2.xml.rels"
                    | "ppt/notesSlides/_rels/notesSlide1.xml.rels"
                    | "ppt/notesSlides/_rels/notesSlide2.xml.rels"
            )
        }
    }
}

#[allow(clippy::too_many_lines)] // Keep the small streaming metadata grammar in one audit unit.
fn validate_office_admission_xml(name: &str, bytes: &[u8]) -> Result<(), String> {
    use quick_xml::{NsReader, XmlVersion, events::Event};

    let mut reader = NsReader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut root = None;
    let mut depth = 0_usize;
    let mut element_stack = Vec::<Vec<u8>>::new();
    loop {
        buffer.clear();
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| format!("malformed OOXML admission metadata: {error}"))?;
        match event {
            Event::Start(element) => {
                validate_admission_element(
                    name,
                    element_stack.last().map(Vec::as_slice),
                    &element,
                    &namespace,
                )?;
                if depth == 0 {
                    if root.is_some() {
                        return Err("multiple OOXML admission XML roots".into());
                    }
                    validate_admission_root(name, &element, reader.decoder())?;
                    root = Some(element.local_name().as_ref().to_vec());
                }
                inspect_office_admission_attributes(
                    name,
                    &element,
                    reader.decoder(),
                    XmlVersion::Implicit1_0,
                )?;
                element_stack.push(element.name().as_ref().to_vec());
                depth += 1;
            }
            Event::Empty(element) => {
                validate_admission_element(
                    name,
                    element_stack.last().map(Vec::as_slice),
                    &element,
                    &namespace,
                )?;
                if depth == 0 {
                    if root.is_some() {
                        return Err("multiple OOXML admission XML roots".into());
                    }
                    validate_admission_root(name, &element, reader.decoder())?;
                    root = Some(element.local_name().as_ref().to_vec());
                }
                inspect_office_admission_attributes(
                    name,
                    &element,
                    reader.decoder(),
                    XmlVersion::Implicit1_0,
                )?;
            }
            Event::End(element) => {
                let opened = element_stack
                    .pop()
                    .ok_or("malformed OOXML admission element stack")?;
                if opened != element.name().as_ref() {
                    return Err("mismatched OOXML admission closing element".into());
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or("malformed OOXML admission XML depth")?;
            }
            Event::Text(text) => {
                if !text
                    .decode()
                    .map_err(|error| error.to_string())?
                    .trim()
                    .is_empty()
                {
                    return Err("unsupported OOXML admission XML text".into());
                }
            }
            Event::CData(text) => {
                if !text
                    .decode()
                    .map_err(|error| error.to_string())?
                    .trim()
                    .is_empty()
                {
                    return Err("unsupported OOXML admission XML text".into());
                }
            }
            Event::GeneralRef(reference) => {
                if !decode_xml_reference(&reference)?.trim().is_empty() {
                    return Err("unsupported OOXML admission XML reference".into());
                }
            }
            Event::DocType(_) => {
                return Err("OOXML admission document types are unsupported".into());
            }
            Event::Eof => {
                let expected = if name == "[Content_Types].xml" {
                    b"Types".as_slice()
                } else {
                    b"Relationships".as_slice()
                };
                if root.as_deref() != Some(expected) || depth != 0 || !element_stack.is_empty() {
                    return Err("invalid OOXML admission XML root".into());
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

fn validate_admission_element(
    name: &str,
    parent: Option<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    namespace: &quick_xml::name::ResolveResult<'_>,
) -> Result<(), String> {
    use quick_xml::name::ResolveResult;

    let (root, child_names, expected_namespace) = if name == "[Content_Types].xml" {
        (
            b"Types".as_slice(),
            [b"Default".as_slice(), b"Override".as_slice()],
            b"http://schemas.openxmlformats.org/package/2006/content-types".as_slice(),
        )
    } else {
        (
            b"Relationships".as_slice(),
            [b"Relationship".as_slice(), b"".as_slice()],
            b"http://schemas.openxmlformats.org/package/2006/relationships".as_slice(),
        )
    };
    let qname = element.name();
    let valid_shape = if parent.is_none() {
        qname.as_ref() == root
    } else {
        parent == Some(root) && child_names.contains(&qname.as_ref())
    };
    if !valid_shape
        || !matches!(namespace, ResolveResult::Bound(actual) if actual.as_ref() == expected_namespace)
    {
        return Err("unsupported OOXML admission element grammar/namespace".into());
    }
    Ok(())
}

fn validate_admission_root(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<(), String> {
    let (qname, namespace) = if name == "[Content_Types].xml" {
        (
            b"Types".as_slice(),
            "http://schemas.openxmlformats.org/package/2006/content-types",
        )
    } else {
        (
            b"Relationships".as_slice(),
            "http://schemas.openxmlformats.org/package/2006/relationships",
        )
    };
    if element.name().as_ref() != qname {
        return Err("invalid OOXML admission qualified root".into());
    }
    let mut actual_namespace = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        if attribute.key.as_ref() == b"xmlns" {
            actual_namespace = Some(
                attribute
                    .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
                    .map_err(|error| error.to_string())?
                    .into_owned(),
            );
        }
    }
    if actual_namespace.as_deref() != Some(namespace) {
        return Err("invalid OOXML admission root namespace".into());
    }
    Ok(())
}

fn inspect_office_admission_attributes(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
    version: quick_xml::XmlVersion,
) -> Result<(), String> {
    validate_known_namespace_bindings(element, decoder, version)?;
    let expected_default_namespace = if name == "[Content_Types].xml" {
        "http://schemas.openxmlformats.org/package/2006/content-types"
    } else {
        "http://schemas.openxmlformats.org/package/2006/relationships"
    };
    let mut local_attributes = BTreeSet::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let qname = attribute.key.as_ref();
        let key = attribute.key.local_name();
        if !local_attributes.insert(key.as_ref().to_vec()) {
            return Err("duplicate OOXML admission attribute".into());
        }
        let allowed_name = if name == "[Content_Types].xml" {
            matches!(
                qname,
                b"xmlns" | b"Extension" | b"ContentType" | b"PartName"
            )
        } else {
            matches!(
                qname,
                b"xmlns" | b"Id" | b"Type" | b"Target" | b"TargetMode"
            )
        };
        if !allowed_name {
            return Err("unsupported OOXML admission attribute qualification".into());
        }
        if attribute.key.as_ref() == b"xmlns"
            && attribute
                .decoded_and_normalized_value(version, decoder)
                .map_err(|error| error.to_string())?
                != expected_default_namespace
        {
            return Err("unsupported OOXML admission namespace rebinding".into());
        }
    }
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let key = attribute.key.local_name();
        let value = attribute
            .decoded_and_normalized_value(version, decoder)
            .map_err(|error| error.to_string())?;
        if key.as_ref() == b"TargetMode" && value.eq_ignore_ascii_case("External") {
            return Err("external OOXML relationships are unsupported".into());
        }
        if key.as_ref() == b"Type" && !supported_office_relationship(&value) {
            return Err("unsupported OOXML relationship type".into());
        }
        if key.as_ref() == b"ContentType" && !supported_office_content_type(&value) {
            return Err("unsupported OOXML content type".into());
        }
    }
    Ok(())
}

fn supported_office_relationship(value: &str) -> bool {
    matches!(
        value,
        "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
            | "http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles"
            | "http://schemas.microsoft.com/office/2007/relationships/stylesWithEffects"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/webSettings"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout"
            | "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide"
    )
}

fn supported_office_content_type(value: &str) -> bool {
    matches!(
        value,
        "image/jpeg"
            | "application/xml"
            | "application/vnd.openxmlformats-package.relationships+xml"
            | "application/vnd.openxmlformats-package.core-properties+xml"
            | "application/vnd.openxmlformats-officedocument.extended-properties+xml"
            | "application/vnd.openxmlformats-officedocument.customXmlProperties+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"
            | "application/vnd.ms-word.stylesWithEffects+xml"
            | "application/vnd.openxmlformats-officedocument.theme+xml"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.webSettings+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.slide+xml"
            | "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"
    )
}

#[allow(clippy::too_many_lines)] // The streaming envelope state stays adjacent for auditability.
fn validate_office_xml_component(name: &str, bytes: &[u8], kind: OfficeKind) -> Result<(), String> {
    use quick_xml::{NsReader, XmlVersion, events::Event};

    let mut reader = NsReader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut root = None;
    let mut depth = 0_usize;
    let mut element_stack = Vec::<Vec<u8>>::new();
    let mut child_counts = Vec::<BTreeMap<Vec<u8>, usize>>::new();
    let mut docx_body_count = 0_usize;
    loop {
        buffer.clear();
        let (namespace, event) = reader
            .read_resolved_event_into(&mut buffer)
            .map_err(|error| format!("malformed OOXML component {name}: {error}"))?;
        match event {
            Event::Start(element) => {
                validate_indexed_office_namespace(name, &element, &namespace)?;
                inspect_office_component_element(
                    name,
                    &element,
                    reader.decoder(),
                    XmlVersion::Implicit1_0,
                    kind,
                )?;
                validate_indexed_office_attributes(name, &element, reader.resolver())?;
                validate_indexed_office_edge(
                    name,
                    element_stack.last().map(Vec::as_slice),
                    element.name().as_ref(),
                )?;
                record_indexed_office_child(
                    name,
                    element_stack.last().map(Vec::as_slice),
                    element.name().as_ref(),
                    child_counts.last_mut(),
                )?;
                if name == "word/document.xml" && element.local_name().as_ref() == b"body" {
                    docx_body_count += 1;
                }
                if depth == 0 {
                    if root.is_some() {
                        return Err("multiple OOXML component roots".into());
                    }
                    validate_office_root_name_and_namespace(
                        name,
                        &element,
                        reader.decoder(),
                        kind,
                    )?;
                    root = Some(element.local_name().as_ref().to_vec());
                }
                element_stack.push(element.name().as_ref().to_vec());
                child_counts.push(BTreeMap::new());
                depth += 1;
            }
            Event::Empty(element) => {
                validate_indexed_office_namespace(name, &element, &namespace)?;
                inspect_office_component_element(
                    name,
                    &element,
                    reader.decoder(),
                    XmlVersion::Implicit1_0,
                    kind,
                )?;
                validate_indexed_office_attributes(name, &element, reader.resolver())?;
                validate_indexed_office_edge(
                    name,
                    element_stack.last().map(Vec::as_slice),
                    element.name().as_ref(),
                )?;
                record_indexed_office_child(
                    name,
                    element_stack.last().map(Vec::as_slice),
                    element.name().as_ref(),
                    child_counts.last_mut(),
                )?;
                if name == "word/document.xml" && element.local_name().as_ref() == b"body" {
                    docx_body_count += 1;
                }
                if depth == 0 {
                    if root.is_some() {
                        return Err("multiple OOXML component roots".into());
                    }
                    validate_office_root_name_and_namespace(
                        name,
                        &element,
                        reader.decoder(),
                        kind,
                    )?;
                    root = Some(element.local_name().as_ref().to_vec());
                }
            }
            Event::End(element) => {
                child_counts
                    .pop()
                    .ok_or("malformed OOXML component child stack")?;
                let opened = element_stack
                    .pop()
                    .ok_or("malformed OOXML component element stack")?;
                if opened != element.name().as_ref() {
                    return Err("mismatched OOXML component closing element".into());
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or("malformed OOXML component depth")?;
            }
            Event::Text(text) => {
                let value = text.decode().map_err(|error| error.to_string())?;
                if !value.trim().is_empty()
                    && !indexed_office_text_node(name, element_stack.last().map(Vec::as_slice))
                {
                    return Err(format!(
                        "unsupported text in unindexed OOXML component: {name}"
                    ));
                }
            }
            Event::CData(text) => {
                let value = text.decode().map_err(|error| error.to_string())?;
                if !value.trim().is_empty()
                    && !indexed_office_text_node(name, element_stack.last().map(Vec::as_slice))
                {
                    return Err(format!(
                        "unsupported text in unindexed OOXML component: {name}"
                    ));
                }
            }
            Event::GeneralRef(reference) => {
                let value = decode_xml_reference(&reference)?;
                if !value.trim().is_empty()
                    && !indexed_office_text_node(name, element_stack.last().map(Vec::as_slice))
                {
                    return Err(format!(
                        "unsupported referenced text in OOXML component: {name}"
                    ));
                }
            }
            Event::DocType(_) => return Err("OOXML document types/entities are unsupported".into()),
            Event::Eof => {
                if root.as_deref() != Some(expected_office_root(name)?)
                    || depth != 0
                    || !element_stack.is_empty()
                    || !child_counts.is_empty()
                    || (name == "word/document.xml" && docx_body_count != 1)
                {
                    return Err(format!("invalid OOXML component root: {name}"));
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

fn validate_indexed_office_namespace(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    resolved: &quick_xml::name::ResolveResult<'_>,
) -> Result<(), String> {
    use quick_xml::name::ResolveResult;

    let expected = if name == "word/document.xml" {
        Some("http://schemas.openxmlformats.org/wordprocessingml/2006/main")
    } else if name == "ppt/presentation.xml" {
        Some("http://schemas.openxmlformats.org/presentationml/2006/main")
    } else if name.starts_with("ppt/slides/slide") {
        match element.name().as_ref().split(|byte| *byte == b':').next() {
            Some(b"p") => Some("http://schemas.openxmlformats.org/presentationml/2006/main"),
            Some(b"a") => Some("http://schemas.openxmlformats.org/drawingml/2006/main"),
            Some(b"a16") => Some("http://schemas.microsoft.com/office/drawing/2014/main"),
            Some(b"p14") => Some("http://schemas.microsoft.com/office/powerpoint/2010/main"),
            _ => {
                return Err(format!(
                    "unsupported indexed OOXML element prefix: {name};element={}",
                    String::from_utf8_lossy(element.name().as_ref())
                ));
            }
        }
    } else {
        None
    };
    let Some(expected) = expected else {
        return Ok(());
    };
    match resolved {
        ResolveResult::Bound(actual) if actual.as_ref() == expected.as_bytes() => Ok(()),
        _ => Err(format!(
            "unsupported indexed OOXML element namespace: {name}"
        )),
    }
}

#[allow(clippy::too_many_lines, clippy::unnested_or_patterns)] // Exact owner/attribute pairs form a readable contract.
fn validate_indexed_office_attributes(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    resolver: &quick_xml::name::NamespaceResolver,
) -> Result<(), String> {
    use quick_xml::name::ResolveResult;

    let indexed = name == "word/document.xml"
        || name == "ppt/presentation.xml"
        || name.starts_with("ppt/slides/slide");
    if !indexed {
        return Ok(());
    }
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let qname = attribute.key.as_ref();
        if qname == b"xmlns" || qname.starts_with(b"xmlns:") {
            continue;
        }
        let (attribute_namespace, _) = resolver.resolve_attribute(attribute.key);
        let element_qname = element.name();
        let valid = if name == "word/document.xml" {
            let namespace_valid = match qname.split(|byte| *byte == b':').next() {
                Some(b"w") => {
                    matches!(attribute_namespace, ResolveResult::Bound(uri) if uri.as_ref() == b"http://schemas.openxmlformats.org/wordprocessingml/2006/main")
                }
                Some(b"mc") => {
                    matches!(attribute_namespace, ResolveResult::Bound(uri) if uri.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
                }
                Some(b"xml") => {
                    matches!(attribute_namespace, ResolveResult::Bound(uri) if uri.as_ref() == b"http://www.w3.org/XML/1998/namespace")
                }
                _ => false,
            };
            namespace_valid
                && matches!(
                    (element_qname.as_ref(), qname),
                    (b"w:document", b"mc:Ignorable")
                        | (b"w:pStyle", b"w:val")
                        | (b"w:spacing", b"w:after")
                        | (b"w:jc", b"w:val")
                        | (b"w:t", b"xml:space")
                        | (b"w:tblStyle", b"w:val")
                        | (b"w:tblW" | b"w:tcW", b"w:type" | b"w:w")
                        | (
                            b"w:tblLook",
                            b"w:firstColumn"
                                | b"w:firstRow"
                                | b"w:lastColumn"
                                | b"w:lastRow"
                                | b"w:noHBand"
                                | b"w:noVBand"
                                | b"w:val"
                        )
                        | (b"w:gridCol", b"w:w")
                        | (b"w:sectPr", b"w:rsidR" | b"w:rsidRPr" | b"w:rsidSect")
                        | (b"w:pgSz", b"w:w" | b"w:h")
                        | (
                            b"w:pgMar",
                            b"w:top"
                                | b"w:right"
                                | b"w:bottom"
                                | b"w:left"
                                | b"w:header"
                                | b"w:footer"
                                | b"w:gutter"
                        )
                        | (b"w:cols", b"w:space")
                        | (b"w:docGrid", b"w:linePitch")
                        | (b"w:vAlign", b"w:val")
                )
        } else if qname == b"r:id" {
            matches!(
                element_qname.as_ref(),
                b"p:sldMasterId" | b"p:notesMasterId" | b"p:sldId"
            ) && matches!(attribute_namespace, ResolveResult::Bound(uri) if uri.as_ref() == b"http://schemas.openxmlformats.org/officeDocument/2006/relationships")
        } else {
            matches!(attribute_namespace, ResolveResult::Unbound)
                && matches!(
                    (element_qname.as_ref(), qname),
                    (b"p:sld", b"show")
                        | (b"p:sldMasterId" | b"p:sldId", b"id")
                        | (b"p:sldSz" | b"p:notesSz", b"cx" | b"cy")
                        | (b"p:cNvPr", b"id" | b"name" | b"hidden")
                        | (b"a:spLocks", b"noGrp")
                        | (b"a:off", b"x" | b"y")
                        | (b"a:ext", b"cx" | b"cy" | b"uri")
                        | (b"a:graphicData", b"uri")
                        | (b"p:ext", b"uri")
                        | (b"a16:creationId", b"id")
                        | (b"p14:creationId", b"val")
                        | (b"a:prstGeom", b"prst")
                        | (b"a:ln" | b"a:gridCol", b"w")
                        | (b"a:defRPr" | b"a:rPr", b"b" | b"sz")
                        | (b"a:srgbClr", b"val")
                        | (b"a:latin" | b"a:ea" | b"a:cs", b"typeface")
                        | (b"a:tblPr", b"firstRow" | b"bandRow")
                        | (b"a:tr", b"h")
                        | (b"a:tc", b"rowSpan" | b"gridSpan" | b"hMerge" | b"vMerge")
                        | (b"a:tcPr", b"marL" | b"marR" | b"marT" | b"marB")
                )
        };
        if !valid {
            return Err(format!(
                "unsupported indexed OOXML attribute namespace/name: {name};attribute={}",
                String::from_utf8_lossy(qname)
            ));
        }
    }
    Ok(())
}

#[allow(clippy::unnested_or_patterns)] // Repeated parent/child collections are explicit.
fn record_indexed_office_child(
    name: &str,
    parent: Option<&[u8]>,
    child: &[u8],
    counts: Option<&mut BTreeMap<Vec<u8>, usize>>,
) -> Result<(), String> {
    let Some(counts) = counts else {
        return Ok(());
    };
    if !(name == "word/document.xml"
        || name == "ppt/presentation.xml"
        || name.starts_with("ppt/slides/slide"))
    {
        return Ok(());
    }
    let count = counts.entry(child.to_vec()).or_default();
    *count += 1;
    let repeatable = matches!(
        (parent, child),
        (Some(b"w:body"), b"w:p" | b"w:tbl")
            | (Some(b"w:p"), b"w:r")
            | (Some(b"w:tbl"), b"w:tr")
            | (Some(b"w:tr"), b"w:tc")
            | (Some(b"w:tc"), b"w:p")
            | (Some(b"w:tblGrid"), b"w:gridCol")
            | (Some(b"p:sldIdLst"), b"p:sldId")
            | (Some(b"p:spTree"), b"p:sp" | b"p:graphicFrame")
            | (Some(b"a:txBody" | b"p:txBody"), b"a:p")
            | (Some(b"a:p"), b"a:r")
            | (Some(b"a:tbl"), b"a:tr")
            | (Some(b"a:tr"), b"a:tc")
            | (Some(b"a:tblGrid"), b"a:gridCol")
    );
    if *count > 1 && !repeatable {
        return Err(format!("duplicate singleton OOXML child: {name}"));
    }
    Ok(())
}

fn indexed_office_text_node(name: &str, parent: Option<&[u8]>) -> bool {
    let local = parent.map(xml_local_name);
    (name == "word/document.xml" && parent == Some(b"w:t"))
        || (name.starts_with("ppt/slides/slide") && parent == Some(b"a:t"))
        || ((name.starts_with("ppt/notesSlides/notesSlide")
            || name.starts_with("ppt/notesMasters/notesMaster"))
            && parent.map(xml_local_name) == Some(b"t"))
        || (name == "docProps/core.xml"
            && matches!(
                local,
                Some(
                    b"created"
                        | b"creator"
                        | b"description"
                        | b"lastModifiedBy"
                        | b"modified"
                        | b"revision"
                        | b"title"
                )
            ))
        || (name == "docProps/app.xml"
            && matches!(
                local,
                Some(
                    b"AppVersion"
                        | b"Application"
                        | b"Characters"
                        | b"CharactersWithSpaces"
                        | b"DocSecurity"
                        | b"HiddenSlides"
                        | b"HyperlinksChanged"
                        | b"Lines"
                        | b"LinksUpToDate"
                        | b"Notes"
                        | b"Pages"
                        | b"Paragraphs"
                        | b"PresentationFormat"
                        | b"ScaleCrop"
                        | b"SharedDoc"
                        | b"Slides"
                        | b"Template"
                        | b"TotalTime"
                        | b"Words"
                        | b"i4"
                        | b"lpstr"
                )
            ))
}

#[allow(clippy::too_many_lines, clippy::unnested_or_patterns)] // Exact pairs read as the admitted XML grammar.
fn validate_indexed_office_edge(
    name: &str,
    parent_qname: Option<&[u8]>,
    child: &[u8],
) -> Result<(), String> {
    let parent = parent_qname;
    let allowed = if name == "word/document.xml" {
        matches!(
            (parent, child),
            (None, b"w:document")
                | (Some(b"w:document"), b"w:body")
                | (Some(b"w:body"), b"w:p" | b"w:tbl" | b"w:sectPr")
                | (Some(b"w:p"), b"w:pPr" | b"w:r")
                | (Some(b"w:pPr"), b"w:pStyle" | b"w:spacing" | b"w:jc")
                | (Some(b"w:r"), b"w:rPr" | b"w:t")
                | (Some(b"w:rPr"), b"w:b")
                | (Some(b"w:tbl"), b"w:tblPr" | b"w:tblGrid" | b"w:tr")
                | (Some(b"w:tblPr"), b"w:tblStyle" | b"w:tblW" | b"w:tblLook")
                | (Some(b"w:tblGrid"), b"w:gridCol")
                | (Some(b"w:tr"), b"w:tc")
                | (Some(b"w:tc"), b"w:tcPr" | b"w:p")
                | (Some(b"w:tcPr"), b"w:tcW" | b"w:vAlign")
                | (
                    Some(b"w:sectPr"),
                    b"w:pgSz" | b"w:pgMar" | b"w:cols" | b"w:docGrid"
                )
        )
    } else if name == "ppt/presentation.xml" {
        matches!(
            (parent, child),
            (None, b"p:presentation")
                | (
                    Some(b"p:presentation"),
                    b"p:sldMasterIdLst"
                        | b"p:notesMasterIdLst"
                        | b"p:sldIdLst"
                        | b"p:sldSz"
                        | b"p:notesSz"
                )
                | (Some(b"p:sldMasterIdLst"), b"p:sldMasterId")
                | (Some(b"p:notesMasterIdLst"), b"p:notesMasterId")
                | (Some(b"p:sldIdLst"), b"p:sldId")
        )
    } else if name.starts_with("ppt/slides/slide") {
        matches!(
            (parent, child),
            (None, b"p:sld")
                | (Some(b"p:sld"), b"p:cSld")
                | (Some(b"p:cSld"), b"p:bg" | b"p:spTree" | b"p:extLst")
                | (Some(b"p:bg"), b"p:bgPr")
                | (Some(b"p:bgPr"), b"a:solidFill")
                | (Some(b"a:solidFill"), b"a:srgbClr")
                | (
                    Some(b"p:spTree"),
                    b"p:nvGrpSpPr" | b"p:grpSpPr" | b"p:sp" | b"p:graphicFrame"
                )
                | (
                    Some(b"p:nvGrpSpPr"),
                    b"p:cNvPr" | b"p:cNvGrpSpPr" | b"p:nvPr"
                )
                | (Some(b"p:grpSpPr"), b"a:xfrm")
                | (Some(b"p:sp"), b"p:nvSpPr" | b"p:spPr" | b"p:txBody")
                | (Some(b"p:nvSpPr"), b"p:cNvPr" | b"p:cNvSpPr" | b"p:nvPr")
                | (Some(b"p:cNvPr"), b"a:extLst")
                | (Some(b"p:cNvSpPr"), b"a:spLocks")
                | (
                    Some(b"p:spPr"),
                    b"a:xfrm" | b"a:prstGeom" | b"a:noFill" | b"a:ln"
                )
                | (Some(b"a:prstGeom"), b"a:avLst")
                | (Some(b"a:ln"), b"a:noFill")
                | (
                    Some(b"p:graphicFrame"),
                    b"p:nvGraphicFramePr" | b"p:xfrm" | b"a:graphic"
                )
                | (
                    Some(b"p:nvGraphicFramePr"),
                    b"p:cNvPr" | b"p:cNvGraphicFramePr" | b"p:nvPr"
                )
                | (Some(b"a:graphic"), b"a:graphicData")
                | (Some(b"a:graphicData"), b"a:tbl")
                | (Some(b"a:tbl"), b"a:tblPr" | b"a:tblGrid" | b"a:tr")
                | (Some(b"a:tblGrid"), b"a:gridCol")
                | (Some(b"a:tr"), b"a:tc")
                | (Some(b"a:tc"), b"a:txBody" | b"a:tcPr")
                | (Some(b"a:tcPr"), b"a:solidFill")
                | (
                    Some(b"p:txBody" | b"a:txBody"),
                    b"a:bodyPr" | b"a:lstStyle" | b"a:p"
                )
                | (Some(b"a:bodyPr"), b"a:noAutofit")
                | (Some(b"a:p"), b"a:pPr" | b"a:r")
                | (Some(b"a:pPr"), b"a:defRPr")
                | (Some(b"a:r"), b"a:rPr" | b"a:t")
                | (
                    Some(b"a:defRPr" | b"a:rPr"),
                    b"a:solidFill" | b"a:latin" | b"a:ea" | b"a:cs"
                )
                | (Some(b"a:xfrm" | b"p:xfrm"), b"a:off" | b"a:ext")
                | (Some(b"a:extLst" | b"p:extLst"), b"a:ext" | b"p:ext")
                | (Some(b"a:ext"), b"a16:creationId")
                | (Some(b"p:ext"), b"p14:creationId")
        )
    } else {
        true
    };
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "unsupported OOXML parent/child structure: {name};parent={};child={}",
            parent.map_or("<root>".into(), String::from_utf8_lossy),
            String::from_utf8_lossy(child)
        ))
    }
}

fn xml_local_name(qname: &[u8]) -> &[u8] {
    qname
        .iter()
        .rposition(|byte| *byte == b':')
        .map_or(qname, |colon| &qname[colon + 1..])
}

fn decode_xml_reference(reference: &quick_xml::events::BytesRef<'_>) -> Result<String, String> {
    if let Some(value) = reference
        .resolve_char_ref()
        .map_err(|error| error.to_string())?
    {
        return Ok(value.to_string());
    }
    match reference
        .decode()
        .map_err(|error| error.to_string())?
        .as_ref()
    {
        "amp" => Ok("&".into()),
        "lt" => Ok("<".into()),
        "gt" => Ok(">".into()),
        "apos" => Ok("'".into()),
        "quot" => Ok("\"".into()),
        _ => Err("undeclared OOXML entity reference".into()),
    }
}

fn validate_office_root_name_and_namespace(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
    kind: OfficeKind,
) -> Result<(), String> {
    let expected_qname = expected_office_root_qname(name, kind)?;
    if element.name().as_ref() != expected_qname.as_bytes() {
        return Err(format!("invalid OOXML component qualified root: {name}"));
    }
    let prefix = expected_qname
        .split_once(':')
        .map_or("", |(prefix, _)| prefix);
    let namespace_key = if prefix.is_empty() {
        "xmlns".to_owned()
    } else {
        format!("xmlns:{prefix}")
    };
    let mut namespace = None;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        if attribute.key.as_ref() == namespace_key.as_bytes() {
            namespace = Some(
                attribute
                    .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
                    .map_err(|error| error.to_string())?
                    .into_owned(),
            );
        }
    }
    if namespace.as_deref() != Some(expected_office_root_namespace(name)?) {
        return Err(format!("invalid OOXML component root namespace: {name}"));
    }
    Ok(())
}

fn inspect_office_component_element(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
    version: quick_xml::XmlVersion,
    kind: OfficeKind,
) -> Result<(), String> {
    validate_known_namespace_bindings(element, decoder, version)?;
    let local = element.local_name();
    if matches!(
        local.as_ref(),
        b"hidden"
            | b"vanish"
            | b"rPrChange"
            | b"pPrChange"
            | b"sectPrChange"
            | b"tblPrChange"
            | b"trPrChange"
            | b"tcPrChange"
            | b"ins"
            | b"del"
            | b"moveFrom"
            | b"moveTo"
            | b"commentRangeStart"
            | b"commentRangeEnd"
            | b"commentReference"
    ) {
        return Err(format!("unsupported hidden/revision OOXML content: {name}"));
    }
    let mut keys = BTreeSet::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let key = attribute.key.local_name();
        if !keys.insert(attribute.key.as_ref().to_vec()) {
            return Err("duplicate OOXML component attribute".into());
        }
        let value = attribute
            .decoded_and_normalized_value(version, decoder)
            .map_err(|error| error.to_string())?;
        if matches!(kind, OfficeKind::Pptx)
            && local.as_ref() == b"cNvPr"
            && key.as_ref() == b"hidden"
            && matches!(value.as_ref(), "1" | "true")
        {
            return Err("hidden PPTX drawing content is unsupported".into());
        }
    }
    Ok(())
}

fn validate_known_namespace_bindings(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
    version: quick_xml::XmlVersion,
) -> Result<(), String> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let expected = match attribute.key.as_ref() {
            b"xmlns:w" => Some("http://schemas.openxmlformats.org/wordprocessingml/2006/main"),
            b"xmlns:p" => Some("http://schemas.openxmlformats.org/presentationml/2006/main"),
            b"xmlns:a" => Some("http://schemas.openxmlformats.org/drawingml/2006/main"),
            b"xmlns:r" => {
                Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships")
            }
            b"xmlns:cp" => {
                Some("http://schemas.openxmlformats.org/package/2006/metadata/core-properties")
            }
            b"xmlns:ap" => {
                Some("http://schemas.openxmlformats.org/officeDocument/2006/extended-properties")
            }
            b"xmlns:ds" => Some("http://schemas.openxmlformats.org/officeDocument/2006/customXml"),
            b"xmlns:b" => {
                Some("http://schemas.openxmlformats.org/officeDocument/2006/bibliography")
            }
            _ => None,
        };
        if let Some(expected) = expected {
            let actual = attribute
                .decoded_and_normalized_value(version, decoder)
                .map_err(|error| error.to_string())?;
            if actual != expected {
                return Err("unsupported OOXML namespace rebinding".into());
            }
        }
    }
    Ok(())
}

fn expected_office_root(name: &str) -> Result<&'static [u8], String> {
    let root = match name {
        "docProps/core.xml" => b"coreProperties".as_slice(),
        "docProps/app.xml" => b"Properties".as_slice(),
        "word/document.xml" => b"document".as_slice(),
        "word/styles.xml" | "word/stylesWithEffects.xml" => b"styles".as_slice(),
        "word/settings.xml" => b"settings".as_slice(),
        "word/webSettings.xml" => b"webSettings".as_slice(),
        "word/fontTable.xml" => b"fonts".as_slice(),
        "word/theme/theme1.xml"
        | "ppt/theme/theme1.xml"
        | "ppt/slideMasters/theme/theme2.xml"
        | "ppt/notesMasters/theme/theme3.xml" => b"theme".as_slice(),
        "word/numbering.xml" => b"numbering".as_slice(),
        "customXml/item1.xml" => b"Sources".as_slice(),
        "customXml/itemProps1.xml" => b"datastoreItem".as_slice(),
        "ppt/presentation.xml" => b"presentation".as_slice(),
        "ppt/slideMasters/slideMaster1.xml" => b"sldMaster".as_slice(),
        "ppt/slideLayouts/slideLayout1.xml" => b"sldLayout".as_slice(),
        "ppt/notesMasters/notesMaster1.xml" => b"notesMaster".as_slice(),
        "ppt/presProps.xml" => b"presentationPr".as_slice(),
        "ppt/tableStyles.xml" => b"tblStyleLst".as_slice(),
        name if name.starts_with("ppt/slides/slide") => b"sld".as_slice(),
        name if name.starts_with("ppt/notesSlides/notesSlide") => b"notes".as_slice(),
        _ => return Err(format!("no OOXML root contract for {name}")),
    };
    Ok(root)
}

fn expected_office_root_qname(name: &str, kind: OfficeKind) -> Result<&'static str, String> {
    let qname = match (name, kind) {
        ("docProps/core.xml", OfficeKind::Docx) => "cp:coreProperties",
        ("docProps/core.xml", OfficeKind::Pptx) => "coreProperties",
        ("docProps/app.xml", OfficeKind::Docx) => "Properties",
        ("docProps/app.xml", OfficeKind::Pptx) => "ap:Properties",
        ("word/document.xml", _) => "w:document",
        ("word/styles.xml" | "word/stylesWithEffects.xml", _) => "w:styles",
        ("word/settings.xml", _) => "w:settings",
        ("word/webSettings.xml", _) => "w:webSettings",
        ("word/fontTable.xml", _) => "w:fonts",
        (
            "word/theme/theme1.xml"
            | "ppt/theme/theme1.xml"
            | "ppt/slideMasters/theme/theme2.xml"
            | "ppt/notesMasters/theme/theme3.xml",
            _,
        ) => "a:theme",
        ("word/numbering.xml", _) => "w:numbering",
        ("customXml/item1.xml", _) => "b:Sources",
        ("customXml/itemProps1.xml", _) => "ds:datastoreItem",
        ("ppt/presentation.xml", _) => "p:presentation",
        ("ppt/slideMasters/slideMaster1.xml", _) => "p:sldMaster",
        ("ppt/slideLayouts/slideLayout1.xml", _) => "p:sldLayout",
        ("ppt/notesMasters/notesMaster1.xml", _) => "p:notesMaster",
        ("ppt/presProps.xml", _) => "p:presentationPr",
        ("ppt/tableStyles.xml", _) => "a:tblStyleLst",
        (name, _) if name.starts_with("ppt/slides/slide") => "p:sld",
        (name, _) if name.starts_with("ppt/notesSlides/notesSlide") => "p:notes",
        _ => return Err(format!("no OOXML qualified root contract for {name}")),
    };
    Ok(qname)
}

fn expected_office_root_namespace(name: &str) -> Result<&'static str, String> {
    let namespace = match name {
        "docProps/core.xml" => {
            "http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
        }
        "docProps/app.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"
        }
        name if name.starts_with("word/") => {
            if name == "word/theme/theme1.xml" {
                "http://schemas.openxmlformats.org/drawingml/2006/main"
            } else {
                "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            }
        }
        "customXml/item1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/bibliography"
        }
        "customXml/itemProps1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/customXml"
        }
        "ppt/theme/theme1.xml"
        | "ppt/slideMasters/theme/theme2.xml"
        | "ppt/notesMasters/theme/theme3.xml"
        | "ppt/tableStyles.xml" => "http://schemas.openxmlformats.org/drawingml/2006/main",
        name if name.starts_with("ppt/") => {
            "http://schemas.openxmlformats.org/presentationml/2006/main"
        }
        _ => return Err(format!("no OOXML root namespace contract for {name}")),
    };
    Ok(namespace)
}

fn fixed_office_component_hash(name: &str) -> Option<&'static str> {
    match name {
        "word/styles.xml" => {
            Some("02d71a68ddb92c055e84526d6a9e45700be3872781ababf759241694bc30e384")
        }
        "word/stylesWithEffects.xml" => {
            Some("463ae0928cf0d84775dbf8cf18d6c3029f6707c81bf590f6d6dd8757a5e93f15")
        }
        "word/settings.xml" => {
            Some("51a0d348fe85965c66e4748a03c3c0d055d78455514f03cb121c334af7d73689")
        }
        "word/webSettings.xml" => {
            Some("349d36de7434d09f86987ff671d8814964a0588c1e630c06e562cda7e75e9f95")
        }
        "word/fontTable.xml" => {
            Some("79385fb7f60247507ecaffc292e9ebd52ea0657b8634f629ba6fccc54011d6bb")
        }
        "word/theme/theme1.xml" => {
            Some("e3a8ab7db9ca7afca56f5f2820a56e8b660016c647773555b060b0a02ac76941")
        }
        "customXml/item1.xml" => {
            Some("a86086ffc5d8e83ebd6c71a55d1d2efaa31b137977f5f3a752366e1023612144")
        }
        "customXml/itemProps1.xml" => {
            Some("c542307b13ec29a8b546217bb37936ab4822e044b265d2952985ec3d6afed24e")
        }
        "word/numbering.xml" => {
            Some("70976f19cbcd896e51890859fe6ecb3467a5a7ad0c040160fedc5a1993cb09ce")
        }
        "ppt/theme/theme1.xml"
        | "ppt/slideMasters/theme/theme2.xml"
        | "ppt/notesMasters/theme/theme3.xml" => {
            Some("8b500abccb3a86061340d95e2edfe2ca62da665f2741801d8790930dba1507a0")
        }
        "ppt/slideMasters/slideMaster1.xml" => {
            Some("9d65bbe37cc6595352ecd0989ee92a033dc78421125ca33066f35731ec609252")
        }
        "ppt/slideLayouts/slideLayout1.xml" => {
            Some("2300fbb688968eebb7a1858e65f8aae0da29e9c5cefc4607fdc04101568c6b62")
        }
        "ppt/notesMasters/notesMaster1.xml" => {
            Some("67e34238b0ae378d2b06825cab5051767fe5c55010ccc2f8c4bc250056068e22")
        }
        "ppt/presProps.xml" => {
            Some("1db4fc10ae1c50c0bf989e54950b4489fbe1ce68105fa850073337ef8e043f5f")
        }
        "ppt/tableStyles.xml" => {
            Some("eefe8607728a909fd3cf65c0e18a5abf20526c77e5d206950696a268edd823c8")
        }
        "ppt/notesSlides/notesSlide1.xml" | "ppt/notesSlides/notesSlide2.xml" => {
            Some("233d7373f5475366a75fa5605a96451fcbd17626d9c7a38d8f4e4c0d8bfefd50")
        }
        _ => None,
    }
}

#[derive(Debug)]
struct OfficeRelationship {
    id: String,
    relationship_type: String,
    target: String,
}

fn parse_office_relationships(bytes: &[u8]) -> Result<Vec<OfficeRelationship>, String> {
    use quick_xml::{Reader, events::Event};

    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut records = Vec::new();
    let mut ids = BTreeSet::new();
    loop {
        buffer.clear();
        match reader
            .read_event_into(&mut buffer)
            .map_err(|error| error.to_string())?
        {
            Event::Start(element) | Event::Empty(element)
                if element.local_name().as_ref() == b"Relationship" =>
            {
                let mut id = None;
                let mut relationship_type = None;
                let mut target = None;
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|error| error.to_string())?;
                    let value = attribute
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|error| error.to_string())?
                        .into_owned();
                    match attribute.key.local_name().as_ref() {
                        b"Id" => id = Some(value),
                        b"Type" => relationship_type = Some(value),
                        b"Target" => target = Some(value),
                        b"TargetMode" => return Err("external OOXML relationship".into()),
                        _ => {}
                    }
                }
                let id = id.ok_or("OOXML relationship ID missing")?;
                if !ids.insert(id.clone()) {
                    return Err("duplicate OOXML relationship ID".into());
                }
                let relationship_type =
                    relationship_type.ok_or("OOXML relationship type missing")?;
                if !supported_office_relationship(&relationship_type) {
                    return Err("unsupported OOXML relationship type".into());
                }
                records.push(OfficeRelationship {
                    id,
                    relationship_type,
                    target: target.ok_or("OOXML relationship target missing")?,
                });
            }
            Event::Eof => return Ok(records),
            _ => {}
        }
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // OPC admission names are case-sensitive identifiers.
fn validate_ooxml_package(
    parts: &BTreeMap<String, Vec<u8>>,
    kind: OfficeKind,
) -> Result<(), String> {
    let required_count = match kind {
        OfficeKind::Docx => 17,
        OfficeKind::Pptx => 25,
    };
    if parts.len() != required_count {
        return Err("OOXML required component set mismatch".into());
    }
    let content_types = parts
        .get("[Content_Types].xml")
        .ok_or("OOXML content types missing")?;
    validate_content_type_assignments(parts, content_types)?;

    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, bytes) in parts.iter().filter(|(name, _)| name.ends_with(".rels")) {
        let source = relationship_source(name)?;
        if !source.is_empty() && !parts.contains_key(&source) {
            return Err("OOXML relationship source missing".into());
        }
        let mut relationship_targets = BTreeMap::new();
        for relationship in parse_office_relationships(bytes)? {
            let target = resolve_relationship_target(&source, &relationship.target)?;
            if relationship_targets
                .insert(target.clone(), relationship.id.clone())
                .is_some()
            {
                return Err("duplicate OOXML relationship target".into());
            }
            if !parts.contains_key(&target) {
                return Err("OOXML relationship target missing".into());
            }
            if relationship.relationship_type != expected_relationship_type(&target)? {
                return Err("OOXML relationship type/target mismatch".into());
            }
            validate_relationship_edge(&source, &target)?;
            adjacency.entry(source.clone()).or_default().push(target);
        }
        let actual_targets = relationship_targets
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let required_targets = required_relationship_targets(&source, kind)
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if actual_targets != required_targets {
            return Err(format!(
                "OOXML required relationship edge set mismatch: {source}"
            ));
        }
    }
    let mut reachable = BTreeSet::new();
    let mut pending = vec![String::new()];
    while let Some(source) = pending.pop() {
        for target in adjacency.get(&source).into_iter().flatten() {
            if reachable.insert(target.clone()) {
                pending.push(target.clone());
            }
        }
    }
    if parts.keys().any(|name| {
        name != "[Content_Types].xml" && !name.ends_with(".rels") && !reachable.contains(name)
    }) {
        return Err("orphan OOXML component".into());
    }
    Ok(())
}

fn required_relationship_targets(source: &str, kind: OfficeKind) -> &'static [&'static str] {
    match (source, kind) {
        ("", OfficeKind::Docx) => &[
            "docProps/core.xml",
            "docProps/app.xml",
            "docProps/thumbnail.jpeg",
            "word/document.xml",
        ],
        ("", OfficeKind::Pptx) => &[
            "docProps/core.xml",
            "docProps/app.xml",
            "ppt/presentation.xml",
        ],
        ("word/document.xml", _) => &[
            "word/styles.xml",
            "word/stylesWithEffects.xml",
            "word/settings.xml",
            "word/webSettings.xml",
            "word/fontTable.xml",
            "word/theme/theme1.xml",
            "customXml/item1.xml",
            "word/numbering.xml",
        ],
        ("customXml/item1.xml", _) => &["customXml/itemProps1.xml"],
        ("ppt/presentation.xml", _) => &[
            "ppt/theme/theme1.xml",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/notesMasters/notesMaster1.xml",
            "ppt/presProps.xml",
            "ppt/tableStyles.xml",
            "ppt/slides/slide1.xml",
            "ppt/slides/slide2.xml",
        ],
        ("ppt/slides/slide1.xml", _) => &[
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/notesSlides/notesSlide1.xml",
        ],
        ("ppt/slides/slide2.xml", _) => &[
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/notesSlides/notesSlide2.xml",
        ],
        ("ppt/notesSlides/notesSlide1.xml", _) => {
            &["ppt/slides/slide1.xml", "ppt/notesMasters/notesMaster1.xml"]
        }
        ("ppt/notesSlides/notesSlide2.xml", _) => {
            &["ppt/slides/slide2.xml", "ppt/notesMasters/notesMaster1.xml"]
        }
        ("ppt/slideMasters/slideMaster1.xml", _) => &[
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/slideMasters/theme/theme2.xml",
        ],
        ("ppt/slideLayouts/slideLayout1.xml", _) => &["ppt/slideMasters/slideMaster1.xml"],
        ("ppt/notesMasters/notesMaster1.xml", _) => &["ppt/notesMasters/theme/theme3.xml"],
        _ => &[],
    }
}

fn expected_relationship_type(target: &str) -> Result<&'static str, String> {
    let value = match target {
        "docProps/core.xml" => {
            "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties"
        }
        "docProps/app.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties"
        }
        "docProps/thumbnail.jpeg" => {
            "http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail"
        }
        "word/document.xml" | "ppt/presentation.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
        }
        "word/styles.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles"
        }
        "word/stylesWithEffects.xml" => {
            "http://schemas.microsoft.com/office/2007/relationships/stylesWithEffects"
        }
        "word/settings.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings"
        }
        "word/webSettings.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/webSettings"
        }
        "word/fontTable.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable"
        }
        "customXml/item1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml"
        }
        "customXml/itemProps1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps"
        }
        "word/numbering.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering"
        }
        "word/theme/theme1.xml"
        | "ppt/theme/theme1.xml"
        | "ppt/slideMasters/theme/theme2.xml"
        | "ppt/notesMasters/theme/theme3.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme"
        }
        "ppt/slideMasters/slideMaster1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster"
        }
        "ppt/slideLayouts/slideLayout1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout"
        }
        "ppt/notesMasters/notesMaster1.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesMaster"
        }
        "ppt/presProps.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/presProps"
        }
        "ppt/tableStyles.xml" => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/tableStyles"
        }
        target if target.starts_with("ppt/slides/slide") => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide"
        }
        target if target.starts_with("ppt/notesSlides/notesSlide") => {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide"
        }
        _ => return Err(format!("unexpected OOXML relationship target: {target}")),
    };
    Ok(value)
}

fn validate_relationship_edge(source: &str, target: &str) -> Result<(), String> {
    let allowed = match source {
        "" => matches!(
            target,
            "docProps/core.xml"
                | "docProps/app.xml"
                | "docProps/thumbnail.jpeg"
                | "word/document.xml"
                | "ppt/presentation.xml"
        ),
        "word/document.xml" => matches!(
            target,
            "word/styles.xml"
                | "word/stylesWithEffects.xml"
                | "word/settings.xml"
                | "word/webSettings.xml"
                | "word/fontTable.xml"
                | "word/theme/theme1.xml"
                | "customXml/item1.xml"
                | "word/numbering.xml"
        ),
        "customXml/item1.xml" => target == "customXml/itemProps1.xml",
        "ppt/presentation.xml" => matches!(
            target,
            "ppt/theme/theme1.xml"
                | "ppt/slideMasters/slideMaster1.xml"
                | "ppt/notesMasters/notesMaster1.xml"
                | "ppt/presProps.xml"
                | "ppt/tableStyles.xml"
                | "ppt/slides/slide1.xml"
                | "ppt/slides/slide2.xml"
        ),
        "ppt/slides/slide1.xml" => matches!(
            target,
            "ppt/slideLayouts/slideLayout1.xml" | "ppt/notesSlides/notesSlide1.xml"
        ),
        "ppt/slides/slide2.xml" => matches!(
            target,
            "ppt/slideLayouts/slideLayout1.xml" | "ppt/notesSlides/notesSlide2.xml"
        ),
        "ppt/notesSlides/notesSlide1.xml" => matches!(
            target,
            "ppt/slides/slide1.xml" | "ppt/notesMasters/notesMaster1.xml"
        ),
        "ppt/notesSlides/notesSlide2.xml" => matches!(
            target,
            "ppt/slides/slide2.xml" | "ppt/notesMasters/notesMaster1.xml"
        ),
        "ppt/slideMasters/slideMaster1.xml" => matches!(
            target,
            "ppt/slideLayouts/slideLayout1.xml" | "ppt/slideMasters/theme/theme2.xml"
        ),
        "ppt/slideLayouts/slideLayout1.xml" => target == "ppt/slideMasters/slideMaster1.xml",
        "ppt/notesMasters/notesMaster1.xml" => target == "ppt/notesMasters/theme/theme3.xml",
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "unsupported OOXML relationship edge: {source} -> {target}"
        ))
    }
}

fn relationship_source(name: &str) -> Result<String, String> {
    if name == "_rels/.rels" {
        return Ok(String::new());
    }
    let (directory, file) = name
        .rsplit_once("/_rels/")
        .ok_or("invalid OOXML relationship component")?;
    let file = file
        .strip_suffix(".rels")
        .ok_or("invalid OOXML relationship suffix")?;
    Ok(format!("{directory}/{file}"))
}

fn resolve_relationship_target(source: &str, target: &str) -> Result<String, String> {
    let mut components = Vec::new();
    if !target.starts_with('/')
        && let Some((directory, _)) = source.rsplit_once('/')
    {
        components.extend(directory.split('/'));
    }
    for component in target.trim_start_matches('/').split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components
                    .pop()
                    .ok_or("OOXML relationship escapes package")?;
            }
            value => components.push(value),
        }
    }
    Ok(components.join("/"))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // OPC admission names are case-sensitive identifiers.
fn validate_content_type_assignments(
    parts: &BTreeMap<String, Vec<u8>>,
    bytes: &[u8],
) -> Result<(), String> {
    use quick_xml::{Reader, events::Event};

    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut defaults = BTreeMap::new();
    let mut overrides = BTreeMap::new();
    loop {
        buffer.clear();
        match reader
            .read_event_into(&mut buffer)
            .map_err(|error| error.to_string())?
        {
            Event::Start(element) | Event::Empty(element) => {
                let local = element.local_name();
                if local.as_ref() != b"Default" && local.as_ref() != b"Override" {
                    continue;
                }
                let mut key = None;
                let mut content_type = None;
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|error| error.to_string())?;
                    let value = attribute
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|error| error.to_string())?
                        .into_owned();
                    match attribute.key.local_name().as_ref() {
                        b"Extension" | b"PartName" => {
                            key = Some(value.trim_start_matches('/').to_owned());
                        }
                        b"ContentType" => content_type = Some(value),
                        _ => {}
                    }
                }
                let key = key.ok_or("OOXML content-type key missing")?;
                let value = content_type.ok_or("OOXML content-type value missing")?;
                let map = if local.as_ref() == b"Default" {
                    &mut defaults
                } else {
                    &mut overrides
                };
                if map.insert(key, value).is_some() {
                    return Err("duplicate OOXML content-type assignment".into());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if defaults.get("rels").map(String::as_str)
        != Some("application/vnd.openxmlformats-package.relationships+xml")
    {
        return Err("OOXML relationships content type mismatch".into());
    }
    for name in parts.keys().filter(|name| name.ends_with(".rels")) {
        let effective = overrides.get(name).or_else(|| defaults.get("rels"));
        if effective.map(String::as_str)
            != Some("application/vnd.openxmlformats-package.relationships+xml")
        {
            return Err(format!(
                "OOXML relationship part content type mismatch: {name}"
            ));
        }
    }
    for name in parts
        .keys()
        .filter(|name| *name != "[Content_Types].xml" && !name.ends_with(".rels"))
    {
        let assigned = overrides
            .get(name)
            .or_else(|| {
                name.rsplit_once('.')
                    .and_then(|(_, extension)| defaults.get(extension))
            })
            .ok_or("OOXML content type missing")?;
        if assigned != expected_content_type(name)? {
            return Err(format!("OOXML content type mismatch: {name}"));
        }
    }
    if overrides.keys().any(|name| !parts.contains_key(name)) {
        return Err("orphan OOXML content-type override".into());
    }
    Ok(())
}

fn expected_content_type(name: &str) -> Result<&'static str, String> {
    let value = match name {
        "docProps/core.xml" => "application/vnd.openxmlformats-package.core-properties+xml",
        "docProps/app.xml" => {
            "application/vnd.openxmlformats-officedocument.extended-properties+xml"
        }
        "docProps/thumbnail.jpeg" => "image/jpeg",
        "word/document.xml" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
        }
        "word/fontTable.xml" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"
        }
        "word/numbering.xml" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"
        }
        "word/settings.xml" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"
        }
        "word/styles.xml" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"
        }
        "word/stylesWithEffects.xml" => "application/vnd.ms-word.stylesWithEffects+xml",
        "word/webSettings.xml" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.webSettings+xml"
        }
        "customXml/itemProps1.xml" => {
            "application/vnd.openxmlformats-officedocument.customXmlProperties+xml"
        }
        "customXml/item1.xml" => "application/xml",
        "word/theme/theme1.xml"
        | "ppt/theme/theme1.xml"
        | "ppt/slideMasters/theme/theme2.xml"
        | "ppt/notesMasters/theme/theme3.xml" => {
            "application/vnd.openxmlformats-officedocument.theme+xml"
        }
        "ppt/presentation.xml" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
        }
        "ppt/slideMasters/slideMaster1.xml" => {
            "application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"
        }
        "ppt/slideLayouts/slideLayout1.xml" => {
            "application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"
        }
        "ppt/notesMasters/notesMaster1.xml" => {
            "application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"
        }
        "ppt/presProps.xml" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"
        }
        "ppt/tableStyles.xml" => {
            "application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"
        }
        name if name.starts_with("ppt/slides/slide") => {
            "application/vnd.openxmlformats-officedocument.presentationml.slide+xml"
        }
        name if name.starts_with("ppt/notesSlides/notesSlide") => {
            "application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"
        }
        _ => return Err(format!("unexpected OOXML content component: {name}")),
    };
    Ok(value)
}

fn reject_nonempty_notes(bytes: &[u8]) -> Result<(), String> {
    use quick_xml::{Reader, events::Event};

    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut in_text = false;
    loop {
        buffer.clear();
        match reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("malformed PPTX notes XML: {error}"))?
        {
            Event::Start(element) if element.local_name().as_ref() == b"t" => in_text = true,
            Event::Start(element) | Event::Empty(element)
                if matches!(
                    element.local_name().as_ref(),
                    b"fld" | b"hlinkClick" | b"hlinkHover" | b"audio" | b"video" | b"oleObj"
                ) =>
            {
                return Err("PPTX notes references are unsupported".into());
            }
            Event::End(element) if element.local_name().as_ref() == b"t" => in_text = false,
            Event::Text(text)
                if in_text
                    && !text
                        .decode()
                        .map_err(|error| error.to_string())?
                        .trim()
                        .is_empty() =>
            {
                return Err("PPTX notes text is unsupported".into());
            }
            Event::CData(text)
                if in_text
                    && !text
                        .decode()
                        .map_err(|error| error.to_string())?
                        .trim()
                        .is_empty() =>
            {
                return Err("PPTX notes text is unsupported".into());
            }
            Event::GeneralRef(reference) => {
                let value = decode_xml_reference(&reference)?;
                if in_text && !value.trim().is_empty() {
                    return Err("PPTX notes text is unsupported".into());
                }
            }
            Event::DocType(_) => return Err("PPTX notes document types are unsupported".into()),
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_lines)] // Keep typed slide/shape/table evidence rules adjacent and explicit.
fn extract_pptx(path: &Path, hash: String) -> Result<Document, String> {
    use ooxmlsdk::schemas::{a::GraphicDataChoice, p::ShapeTreeChoice};

    preflight_ooxml(path, OfficeKind::Pptx)?;
    let package = PresentationDocument::new_from_file(path).map_err(|error| error.to_string())?;
    let presentation = package
        .presentation_part()
        .map_err(|error| error.to_string())?;
    let presentation_root = presentation
        .root_element(&package)
        .map_err(|error| error.to_string())?;
    let declared_slides = &presentation_root
        .slide_id_list
        .as_ref()
        .ok_or("PPTX slide ID list is missing")?
        .slide_id;
    if declared_slides.len() < 2 || declared_slides.len() > MAX_PDF_PAGES {
        return Err("PPTX slide limit exceeded".into());
    }
    let mut relationship_ids = BTreeSet::new();
    let mut numeric_slide_ids = BTreeSet::new();
    let mut block_count = 0_usize;
    let mut paragraph_count = 0_usize;
    for slide_id in declared_slides {
        if !(256..2_147_483_648).contains(&slide_id.id) {
            return Err("PPTX numeric slide ID is out of range".into());
        }
        if !numeric_slide_ids.insert(slide_id.id) {
            return Err("duplicate PPTX numeric slide ID".into());
        }
        let relationship_id = slide_id.relationship_id.as_str();
        if !relationship_ids.insert(relationship_id.to_owned()) {
            return Err("duplicate PPTX slide relationship ID".into());
        }
        let Some(ooxmlsdk::parts::PartRef::SlidePart(slide_part)) =
            presentation.get_part_by_id(&package, relationship_id)
        else {
            return Err("PPTX slide relationship is missing or mismatched".into());
        };
        let slide = slide_part
            .root_element(&package)
            .map_err(|error| error.to_string())?;
        block_count = block_count
            .checked_add(slide.common_slide_data.shape_tree.shape_tree_choice.len())
            .ok_or("PPTX block count overflow")?;
        for choice in &slide.common_slide_data.shape_tree.shape_tree_choice {
            match choice {
                ShapeTreeChoice::Shape(shape) => {
                    paragraph_count = paragraph_count
                        .checked_add(
                            shape
                                .text_body
                                .as_ref()
                                .map_or(0, |body| body.paragraph.len()),
                        )
                        .ok_or("PPTX paragraph count overflow")?;
                }
                ShapeTreeChoice::GraphicFrame(frame) => {
                    let [GraphicDataChoice::Table(table)] =
                        frame.graphic.graphic_data.graphic_data_choice.as_slice()
                    else {
                        continue;
                    };
                    if table.table_grid.grid_column.len() != 2
                        || table.table_row.len() > MAX_OFFICE_TABLE_ROWS
                        || table
                            .table_row
                            .iter()
                            .map(|row| row.table_cell.len())
                            .sum::<usize>()
                            > MAX_OFFICE_TABLE_CELLS
                    {
                        return Err("PPTX table dimension limit exceeded".into());
                    }
                    for cell in table.table_row.iter().flat_map(|row| &row.table_cell) {
                        if cell.row_span.is_some_and(|span| span != 1)
                            || cell.grid_span.is_some_and(|span| span != 1)
                            || cell
                                .horizontal_merge
                                .is_some_and(ooxmlsdk::simple_type::BooleanValue::as_bool)
                            || cell
                                .vertical_merge
                                .is_some_and(ooxmlsdk::simple_type::BooleanValue::as_bool)
                        {
                            return Err("unsupported PPTX merged/spanning table cell".into());
                        }
                        paragraph_count = paragraph_count
                            .checked_add(
                                cell.text_body
                                    .as_ref()
                                    .map_or(0, |body| body.paragraph.len()),
                            )
                            .ok_or("PPTX paragraph count overflow")?;
                    }
                }
                _ => {}
            }
        }
    }
    if block_count > MAX_OFFICE_BLOCKS || paragraph_count > MAX_OFFICE_BLOCKS {
        return Err("PPTX block limit exceeded".into());
    }
    let mut passages = Vec::new();
    let mut retained = 0_usize;
    for (slide_index, slide_id) in declared_slides.iter().enumerate() {
        let Some(ooxmlsdk::parts::PartRef::SlidePart(slide_part)) =
            presentation.get_part_by_id(&package, slide_id.relationship_id.as_str())
        else {
            return Err("PPTX slide relationship is missing or mismatched".into());
        };
        let slide = slide_part
            .root_element(&package)
            .map_err(|error| error.to_string())?;
        if slide.show.is_some_and(|value| !value.as_bool()) {
            return Err("hidden PPTX slides are unsupported".into());
        }
        let mut table_number = 0_usize;
        let first_passage = passages.len();
        for (tree_index, shape) in slide
            .common_slide_data
            .shape_tree
            .shape_tree_choice
            .iter()
            .enumerate()
        {
            match shape {
                ShapeTreeChoice::Shape(shape) => {
                    let drawing = &shape
                        .non_visual_shape_properties
                        .non_visual_drawing_properties;
                    if drawing
                        .hidden
                        .is_some_and(ooxmlsdk::simple_type::BooleanValue::as_bool)
                    {
                        return Err("hidden PPTX shapes are unsupported".into());
                    }
                    let Some(text_body) = shape.text_body.as_deref() else {
                        continue;
                    };
                    let text = drawing_text(&text_body.paragraph)?;
                    if text.is_empty() {
                        continue;
                    }
                    let locator = format!(
                        "slide={};shape-tree={};shape-id={}",
                        slide_index + 1,
                        tree_index + 1,
                        drawing.id
                    );
                    retain_office_text(&mut retained, &format!("{locator}\n{text}"))?;
                    passages.push(Passage {
                        locator,
                        text,
                        cells: vec![],
                    });
                }
                ShapeTreeChoice::GraphicFrame(frame) => {
                    let drawing = &frame
                        .non_visual_graphic_frame_properties
                        .non_visual_drawing_properties;
                    if drawing
                        .hidden
                        .is_some_and(ooxmlsdk::simple_type::BooleanValue::as_bool)
                    {
                        return Err("hidden PPTX graphic frames are unsupported".into());
                    }
                    let choices = &frame.graphic.graphic_data.graphic_data_choice;
                    let [GraphicDataChoice::Table(table)] = choices.as_slice() else {
                        return Err("unsupported PPTX graphic frame".into());
                    };
                    table_number += 1;
                    if table.table_row.len() != 2
                        || table.table_row.iter().any(|row| row.table_cell.len() != 2)
                    {
                        return Err("PPTX pilot tables require one two-column data row".into());
                    }
                    let rows = table
                        .table_row
                        .iter()
                        .map(|row| {
                            row.table_cell
                                .iter()
                                .map(|cell| {
                                    cell.text_body
                                        .as_deref()
                                        .ok_or_else(|| "PPTX table cell text is missing".to_owned())
                                        .and_then(|body| drawing_text(&body.paragraph))
                                })
                                .collect::<Result<Vec<_>, String>>()
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    let text = format!(
                        "{}: {}\n{}: {}",
                        rows[0][0], rows[1][0], rows[0][1], rows[1][1]
                    );
                    let locator = format!(
                        "slide={};shape-tree={};graphic-id={};table={table_number};headers=A1:B1;cells=A2:B2",
                        slide_index + 1,
                        tree_index + 1,
                        drawing.id
                    );
                    retain_office_text(&mut retained, &format!("{locator}\n{text}"))?;
                    passages.push(Passage {
                        locator,
                        text,
                        cells: vec![],
                    });
                }
                _ => return Err("unsupported PPTX visual or embedded content".into()),
            }
        }
        if passages.len() == first_passage {
            return Err("PPTX slide has no supported text evidence".into());
        }
    }
    if passages.is_empty() {
        return Err("PPTX has no supported text passages".into());
    }
    Ok(document(hash, passages, vec![]))
}

fn drawing_text(paragraphs: &[ooxmlsdk::schemas::a::Paragraph]) -> Result<String, String> {
    use ooxmlsdk::schemas::a::ParagraphChoice;

    let mut lines = Vec::new();
    for paragraph in paragraphs {
        let mut text = String::new();
        for choice in &paragraph.paragraph_choice {
            match choice {
                ParagraphChoice::Run(run) => text.push_str(&run.text),
                ParagraphChoice::Break(_) => text.push('\n'),
                _ => return Err("unsupported PPTX text content".into()),
            }
        }
        if !text.is_empty() {
            lines.push(text);
        }
    }
    Ok(lines.join("\n"))
}

fn extract_csv(bytes: &[u8], hash: &str) -> Result<Document, String> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("source byte limit exceeded".into());
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(false)
        .from_reader(bytes);
    let headers = reader.headers().map_err(|error| error.to_string())?.clone();
    if headers.len() > MAX_COLUMNS {
        return Err("CSV column limit exceeded".into());
    }
    let mut distinct_headers = BTreeSet::new();
    if headers.is_empty()
        || headers
            .iter()
            .any(|header| header.trim().is_empty() || !distinct_headers.insert(header.trim()))
    {
        return Err("CSV requires non-empty unique headers".into());
    }
    let mut passages = Vec::new();
    let mut output_bytes = 0;
    for (row, record) in reader.records().enumerate() {
        if row >= MAX_ROWS || (row + 1) * headers.len() > MAX_CELLS {
            return Err("CSV row/cell limit exceeded".into());
        }
        let record = record.map_err(|error| error.to_string())?;
        let mut lines = Vec::with_capacity(headers.len());
        let mut evidence = Vec::with_capacity(headers.len());
        for (column, (header, value)) in headers.iter().zip(record.iter()).enumerate() {
            lines.push(format!("{header}: {value}"));
            evidence.push(CellEvidence::Literal {
                locator: format!(
                    "record={};column={};header={}",
                    row + 1,
                    column_name(column),
                    serde_json::to_string(header).map_err(|error| error.to_string())?
                ),
                value_type: "string".into(),
                value: value.into(),
            });
        }
        let text = lines.join("\n");
        output_bytes += text.len();
        if output_bytes > MAX_OUTPUT_BYTES {
            return Err("CSV output limit exceeded".into());
        }
        passages.push(Passage {
            locator: format!(
                "record={};cells=A{}:{}{}",
                row + 1,
                row + 2,
                column_name(headers.len() - 1),
                row + 2
            ),
            text,
            cells: evidence,
        });
    }
    if passages.is_empty() {
        return Err("header-only CSV is unsupported".into());
    }
    Ok(document(hash.to_owned(), passages, vec![]))
}

fn extract_xlsx(path: &Path, hash: String) -> Result<Document, String> {
    preflight_xlsx(path)?;
    let mut workbook: Xlsx<_> =
        open_workbook(path).map_err(|error: calamine::XlsxError| error.to_string())?;
    if workbook
        .sheets_metadata()
        .iter()
        .any(|sheet| sheet.visible != SheetVisible::Visible)
    {
        return Err("hidden XLSX sheets are unsupported".into());
    }
    let mut passages = Vec::new();
    let mut total_cells = 0;
    let mut retained_bytes: usize = 0;
    for sheet in workbook.sheet_names() {
        let mut cells = BTreeMap::new();
        let mut reader = workbook
            .worksheet_cells_reader(&sheet)
            .map_err(|error| error.to_string())?;
        while let Some(cell) = reader
            .next_cell_with_formula_metadata()
            .map_err(|error| error.to_string())?
        {
            total_cells += 1;
            if total_cells > MAX_CELLS
                || cell.pos.0 as usize >= MAX_ROWS
                || cell.pos.1 as usize >= MAX_COLUMNS
            {
                return Err("XLSX row/column/cell limit exceeded".into());
            }
            let formula = cell.formula.map(formula_expression).transpose()?;
            let cached = typed_value(cell.value)?;
            let (value_type, value) = match (formula.as_ref(), cached) {
                (Some(_), None) => return Err("cacheless XLSX formulas are unsupported".into()),
                (None, None) => continue,
                (_, Some(value)) => value,
            };
            retained_bytes = retained_bytes
                .checked_add(value.len() + formula.as_ref().map_or(0, String::len))
                .ok_or("XLSX retained output size overflow")?;
            if retained_bytes > MAX_OUTPUT_BYTES {
                return Err("XLSX retained output limit exceeded".into());
            }
            cells.insert(
                cell.pos,
                SheetCell {
                    value_type: value_type.into(),
                    value,
                    formula,
                },
            );
        }
        passages.extend(sheet_passages(&sheet, &cells, &mut retained_bytes)?);
    }
    Ok(document(hash, passages, vec![]))
}

fn preflight_xlsx(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    preflight_zip(file)
}

fn preflight_zip(reader: impl std::io::Read + std::io::Seek) -> Result<(), String> {
    use std::io::Read;

    let mut archive = zip::ZipArchive::new(reader).map_err(|error| error.to_string())?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err("XLSX ZIP entry limit exceeded".into());
    }
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry.name().to_owned();
        if !supported_xlsx_component(&name) {
            return Err(format!("unsupported XLSX component: {name}"));
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(MAX_ZIP_ENTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_ZIP_ENTRY_BYTES {
            return Err("XLSX ZIP entry size limit exceeded".into());
        }
        expanded = expanded
            .checked_add(bytes.len() as u64)
            .ok_or("XLSX ZIP expanded size overflow")?;
        if expanded > MAX_ZIP_EXPANDED_BYTES {
            return Err("XLSX ZIP expanded size limit exceeded".into());
        }
        validate_xlsx_control_part(&name, &bytes)?;
    }
    Ok(())
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // ZIP member names are an exact admission list, not user-facing file extensions.
fn supported_xlsx_component(name: &str) -> bool {
    matches!(
        name,
        "[Content_Types].xml"
            | "_rels/.rels"
            | "xl/workbook.xml"
            | "xl/_rels/workbook.xml.rels"
            | "xl/styles.xml"
            | "xl/sharedStrings.xml"
    ) || (name.starts_with("xl/theme/theme") && name.ends_with(".xml"))
        || (name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
}

fn validate_xlsx_control_part(name: &str, bytes: &[u8]) -> Result<(), String> {
    use quick_xml::{Reader, events::Event};

    let expected_root = match name {
        "[Content_Types].xml" => b"Types".as_slice(),
        "_rels/.rels" | "xl/_rels/workbook.xml.rels" => b"Relationships".as_slice(),
        "xl/workbook.xml" => b"workbook".as_slice(),
        "xl/styles.xml" => b"styleSheet".as_slice(),
        "xl/sharedStrings.xml" => b"sst".as_slice(),
        name if name.starts_with("xl/theme/theme") => b"theme".as_slice(),
        name if name.starts_with("xl/worksheets/sheet") => b"worksheet".as_slice(),
        _ => return Err("unsupported XLSX XML control part".into()),
    };
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    let mut state = XmlControlState::default();
    loop {
        buffer.clear();
        match reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("malformed XLSX XML: {error}"))?
        {
            Event::Start(element) => {
                inspect_xlsx_xml_element(name, &element, reader.decoder(), &mut state)?;
                state.depth += 1;
            }
            Event::Empty(element) => {
                inspect_xlsx_xml_element(name, &element, reader.decoder(), &mut state)?;
            }
            Event::End(_) => {
                state.depth = state
                    .depth
                    .checked_sub(1)
                    .ok_or("malformed XLSX XML depth")?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if state.root.as_deref() != Some(expected_root) {
        return Err("unexpected XLSX XML root element".into());
    }
    if state.depth != 0 {
        return Err("unclosed XLSX XML root element".into());
    }
    if name == "xl/sharedStrings.xml"
        && ((state.shared_count > 0 && state.unique_count.is_none())
            || state
                .unique_count
                .is_some_and(|count| count != state.shared_count))
    {
        return Err("invalid XLSX sharedStrings uniqueCount".into());
    }
    Ok(())
}

#[derive(Default)]
struct XmlControlState {
    root: Option<Vec<u8>>,
    depth: usize,
    shared_count: usize,
    unique_count: Option<usize>,
}

fn inspect_xlsx_xml_element(
    name: &str,
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
    state: &mut XmlControlState,
) -> Result<(), String> {
    use quick_xml::XmlVersion;

    let element_name = element.local_name();
    let element_name = element_name.as_ref();
    let is_root = state.depth == 0;
    if is_root {
        if state.root.is_some() {
            return Err("multiple XLSX XML root elements".into());
        }
        state.root = Some(element_name.to_vec());
    }
    if name == "xl/sharedStrings.xml" && element_name == b"sst" && !is_root {
        return Err("nested XLSX sharedStrings root".into());
    }

    let mut relationship_type_seen = false;
    let mut target_mode_seen = false;
    let mut unique_count_seen = false;
    for attribute in element.attributes() {
        let attribute =
            attribute.map_err(|error| format!("malformed XLSX XML attribute: {error}"))?;
        let key = attribute.key.local_name();
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)
            .map_err(|error| format!("malformed XLSX XML attribute value: {error}"))?;
        match (element_name, key.as_ref()) {
            (b"sheet", b"state")
                if value.eq_ignore_ascii_case("hidden")
                    || value.eq_ignore_ascii_case("veryHidden") =>
            {
                return Err("hidden XLSX sheets are unsupported".into());
            }
            (b"Relationship", b"TargetMode") => {
                if target_mode_seen {
                    return Err("duplicate XLSX relationship TargetMode".into());
                }
                target_mode_seen = true;
                if value.eq_ignore_ascii_case("external") {
                    return Err("unsupported XLSX relationship".into());
                }
            }
            (b"Relationship", b"Type") => {
                if relationship_type_seen {
                    return Err("duplicate XLSX relationship Type".into());
                }
                relationship_type_seen = true;
                if unsupported_relationship_type(&value) {
                    return Err("unsupported XLSX relationship".into());
                }
            }
            (b"Override" | b"Default", b"ContentType") if unsupported_content_type(&value) => {
                return Err("unsupported XLSX content type".into());
            }
            (_, b"uniqueCount") if name == "xl/sharedStrings.xml" => {
                if element_name != b"sst" || !is_root {
                    return Err("XLSX sharedStrings uniqueCount must be on the root".into());
                }
                if unique_count_seen || state.unique_count.is_some() {
                    return Err("duplicate XLSX sharedStrings uniqueCount".into());
                }
                unique_count_seen = true;
                let count = value
                    .parse::<usize>()
                    .map_err(|_| "invalid XLSX sharedStrings uniqueCount")?;
                if count > MAX_SHARED_STRINGS {
                    return Err("excessive XLSX sharedStrings uniqueCount".into());
                }
                state.unique_count = Some(count);
            }
            _ => {}
        }
    }
    if name == "xl/sharedStrings.xml" && element_name == b"si" {
        state.shared_count += 1;
        if state.shared_count > MAX_SHARED_STRINGS {
            return Err("excessive XLSX shared strings".into());
        }
    }
    Ok(())
}

fn unsupported_relationship_type(value: &str) -> bool {
    matches!(
        value.rsplit('/').next(),
        Some(
            "drawing"
                | "comments"
                | "externalLink"
                | "vbaProject"
                | "oleObject"
                | "package"
                | "image"
                | "threadedComment"
                | "person"
        )
    )
}

fn unsupported_content_type(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "application/vnd.ms-office.vbaproject"
            | "application/vnd.ms-excel.sheet.macroenabled.main+xml"
            | "application/vnd.openxmlformats-officedocument.drawing+xml"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml"
            | "application/vnd.ms-excel.comments+xml"
    )
}

fn sheet_passages(
    sheet: &str,
    cells: &BTreeMap<(u32, u32), SheetCell>,
    retained_bytes: &mut usize,
) -> Result<Vec<Passage>, String> {
    if cells.is_empty() {
        return Ok(vec![]);
    }
    let header_row = cells.keys().map(|(row, _)| *row).min().expect("cells");
    let columns: Vec<_> = cells
        .keys()
        .filter_map(|(row, column)| (*row == header_row).then_some(*column))
        .collect();
    if columns.is_empty()
        || columns.windows(2).any(|pair| pair[1] != pair[0] + 1)
        || columns.iter().any(|column| {
            cells
                .get(&(header_row, *column))
                .is_some_and(|cell| cell.formula.is_some())
        })
    {
        return Err("XLSX requires contiguous literal headers".into());
    }
    let data_rows: BTreeSet<_> = cells
        .keys()
        .filter_map(|(row, _)| (*row > header_row).then_some(*row))
        .collect();
    if data_rows.is_empty() {
        return Err("XLSX sheet requires a data row".into());
    }
    let mut passages = Vec::with_capacity(data_rows.len());
    for row in data_rows {
        if columns
            .iter()
            .any(|column| !cells.contains_key(&(row, *column)))
            || cells
                .keys()
                .any(|(cell_row, column)| *cell_row == row && !columns.contains(column))
        {
            return Err("sparse or extra XLSX row cells are unsupported".into());
        }
        let mut lines = Vec::with_capacity(columns.len() + 1);
        let mut evidence = Vec::with_capacity(columns.len());
        for column in &columns {
            let header = &cells[&(header_row, *column)].value;
            let cell = &cells[&(row, *column)];
            let locator = format!("{sheet}!{}{}", column_name(*column as usize), row + 1);
            if let Some(expression) = &cell.formula {
                lines.push(format!("{header} formula: {expression}"));
                lines.push(format!(
                    "{header} cached {}: {}",
                    cell.value_type, cell.value
                ));
                evidence.push(CellEvidence::Formula {
                    locator,
                    expression: expression.clone(),
                    cached_type: cell.value_type.clone(),
                    cached_value: cell.value.clone(),
                });
            } else {
                lines.push(format!("{header}: {}", cell.value));
                evidence.push(CellEvidence::Literal {
                    locator,
                    value_type: cell.value_type.clone(),
                    value: cell.value.clone(),
                });
            }
        }
        let text = lines.join("\n");
        *retained_bytes = retained_bytes
            .checked_add(text.len())
            .ok_or("XLSX rendered output size overflow")?;
        if *retained_bytes > MAX_OUTPUT_BYTES {
            return Err("XLSX rendered output limit exceeded".into());
        }
        passages.push(Passage {
            locator: format!(
                "sheet={sheet};headers={}{}:{}{};cells={}{}:{}{}",
                column_name(columns[0] as usize),
                header_row + 1,
                column_name(*columns.last().expect("columns") as usize),
                header_row + 1,
                column_name(columns[0] as usize),
                row + 1,
                column_name(*columns.last().expect("columns") as usize),
                row + 1,
            ),
            text,
            cells: evidence,
        });
    }
    Ok(passages)
}

fn formula_expression(metadata: XlsxFormulaMetadata) -> Result<String, String> {
    let formula = match metadata {
        XlsxFormulaMetadata::Normal { formula } | XlsxFormulaMetadata::Shared { formula, .. } => {
            formula
        }
        XlsxFormulaMetadata::SharedDerived { .. } => {
            return Err("derived shared XLSX formula expression unavailable".into());
        }
        _ => return Err("unsupported XLSX formula metadata".into()),
    };
    Ok(format!("={formula}"))
}

fn typed_value(value: DataRef<'_>) -> Result<Option<(&'static str, String)>, String> {
    match value {
        DataRef::Empty => Ok(None),
        DataRef::String(value) => Ok(Some(("string", value))),
        DataRef::SharedString(value) => Ok(Some(("string", value.to_owned()))),
        DataRef::Int(value) => Ok(Some(("integer", value.to_string()))),
        DataRef::Float(value) => Ok(Some(("float", value.to_string()))),
        DataRef::Bool(value) => Ok(Some(("boolean", value.to_string()))),
        DataRef::DateTimeIso(value) => Ok(Some(("datetime", value))),
        DataRef::DurationIso(value) => Ok(Some(("duration", value))),
        DataRef::DateTime(_) | DataRef::Error(_) => Err("unsupported XLSX cell type".into()),
    }
}

fn document(hash: String, passages: Vec<Passage>, unsupported: Vec<String>) -> Document {
    Document {
        source_revision: format!("sha256:{hash}"),
        source_sha256: hash,
        passages,
        unsupported,
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(output, "{byte:02x}").expect("write digest");
    }
    output
}

fn column_name(mut column: usize) -> String {
    let mut name = String::new();
    loop {
        name.insert(
            0,
            char::from(b'A' + u8::try_from(column % 26).expect("column remainder")),
        );
        if column < 26 {
            return name;
        }
        column = column / 26 - 1;
    }
}

async fn setup() -> (PgPool, PgPool, String, String) {
    let migrator = PgPoolOptions::new()
        .max_connections(2)
        .connect(MIGRATOR_URL)
        .await
        .expect("run `just setup` first");
    let runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(RUNTIME_URL)
        .await
        .expect("restricted runtime connection");
    let mut transaction = migrator.begin().await.expect("begin fixture reset");
    sqlx::raw_sql(include_str!("fixtures/reset.sql"))
        .execute(&mut *transaction)
        .await
        .expect("reset fixture");
    let reader = random_bearer();
    let writer = random_bearer();
    for (id, bearer, class, operations) in [
        (
            "c0000000000000000000000000000001",
            &reader,
            "agent_reader",
            vec!["list", "search", "read"],
        ),
        (
            "c0000000000000000000000000000003",
            &writer,
            "trusted_writer",
            vec!["create", "correct", "forget"],
        ),
    ] {
        sqlx::query("INSERT INTO credentials
            (tenant_id, id, principal_id, app_id, token_digest, credential_class, allowed_operations, issued_at, expires_at)
            VALUES ($1, $2, '10000000000000000000000000000001', 'a0000000000000000000000000000001', $3, $4, $5, clock_timestamp(), clock_timestamp() + interval '24 hours')")
            .bind(TENANT).bind(id).bind(Sha256::digest(bearer.as_bytes()).as_slice())
            .bind(class).bind(operations).execute(&mut *transaction).await.expect("insert synthetic credential");
    }
    transaction.commit().await.expect("commit fixture reset");
    (migrator, runtime, reader, writer)
}

fn random_bearer() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness");
    let mut bearer = String::with_capacity(64);
    for byte in bytes {
        write!(bearer, "{byte:02x}").expect("write bearer");
    }
    bearer
}

async fn post_json(
    app: &axum::Router,
    bearer: &str,
    key: &str,
    body: Value,
) -> (StatusCode, Value) {
    json_response(
        app,
        Request::builder()
            .method("POST")
            .uri("/v1/memories")
            .header("authorization", format!("Bearer {bearer}"))
            .header("idempotency-key", key)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("create request"),
    )
    .await
}

async fn search(app: &axum::Router, bearer: &str, query: &str) -> (StatusCode, Value) {
    json_response(
        app,
        Request::builder()
            .method("POST")
            .uri("/v1/search")
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({"query": query}).to_string()))
            .expect("search request"),
    )
    .await
}

async fn get_json(app: &axum::Router, bearer: &str, uri: &str) -> (StatusCode, Value) {
    json_response(
        app,
        Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .expect("read request"),
    )
    .await
}

async fn json_response(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.expect("router response");
    let status = response.status();
    let body = to_bytes(response.into_body(), MAX_OUTPUT_BYTES)
        .await
        .expect("bounded JSON response");
    (
        status,
        serde_json::from_slice(&body).expect("JSON response"),
    )
}
