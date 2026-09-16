use super::retrieval_benchmark::tool_version;
use super::*;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

const CONTEXT_BUDGET: usize = 4096;
const FILES_PER_SCALE: usize = 100;
const LARGE_FILE_INDEXES: [usize; 5] = [0, 25, 45, 65, 80];
const LARGE_FORMATS: [(&str, usize); 5] = [
    ("pdf", 0),
    ("docx", 25),
    ("xlsx", 45),
    ("csv", 65),
    ("pptx", 80),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum Scale {
    Thousand,
    TenThousand,
}

impl Scale {
    const fn name(self) -> &'static str {
        match self {
            Self::Thousand => "1k",
            Self::TenThousand => "10k",
        }
    }

    const fn passages_per_file(self) -> usize {
        match self {
            Self::Thousand => 10,
            Self::TenThousand => 100,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[allow(clippy::struct_field_names)] // Domain names must match the Source Passage contract.
struct Passage {
    key: String,
    file_id: String,
    format: &'static str,
    source_sha256: String,
    source_revision: String,
    source_revision_id: String,
    extraction_set_id: String,
    passage_id: String,
    structural_parent: String,
    passage_order: usize,
    continuation_direction: &'static str,
    locator: Value,
    text: String,
    item_id: String,
    revision_id: String,
}

#[derive(Clone)]
struct Query {
    id: &'static str,
    text: &'static str,
    category: &'static str,
    answer: Option<&'static str>,
    required: Vec<ExpectedEvidence>,
}

#[derive(Clone, Debug, Serialize)]
struct ExpectedEvidence {
    key: String,
    format: &'static str,
    item_id: String,
    revision_id: String,
    source_sha256: String,
    source_revision: String,
    source_revision_id: String,
    extraction_set_id: String,
    passage_id: String,
    locator: Value,
    text: String,
}

#[derive(Serialize)]
struct VariantScore {
    required: usize,
    supplied: usize,
    matched: usize,
    full_support: bool,
    recall: f64,
    evidence_precision: f64,
    citation_valid: usize,
    citation_total: usize,
    citation_validity: f64,
    context_bytes: usize,
}

fn digest(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .fold(String::new(), |mut output, byte| {
            write!(output, "{byte:02x}").expect("hex output");
            output
        })
}

fn format_plan() -> Vec<&'static str> {
    let mut formats = Vec::with_capacity(FILES_PER_SCALE);
    for (format, count) in [
        ("pdf", 25),
        ("docx", 20),
        ("xlsx", 20),
        ("csv", 15),
        ("pptx", 15),
        ("txt_md", 5),
    ] {
        formats.extend(std::iter::repeat_n(format, count));
    }
    formats
}

fn locator(format: &str, passage: usize) -> String {
    match format {
        "pdf" => match passage % 4 {
            0 => format!(
                "page={};block=1;type=title;bbox=247.5,696.0,117.0,18.0",
                passage / 4 + 1
            ),
            1 => format!(
                "page={};block=2;type=paragraph;bbox=78.0,646.0,190.0,10.0",
                passage / 4 + 1
            ),
            2 => format!(
                "page={};block=3;type=key_value;bbox=78.0,616.0,230.0,10.0",
                passage / 4 + 1
            ),
            _ => format!(
                "page={};block=4;type=table;bbox=108.0,530.0,396.0,56.0",
                passage / 4 + 1
            ),
        },
        "docx" if passage % 5 == 4 => format!(
            "heading=Operations/Region {};table={};row=2;headers=A1:B1;cells=A2:B2",
            passage / 10 + 1,
            passage / 5 + 1
        ),
        "docx" => format!(
            "heading=Operations/Region {};paragraph={}",
            passage / 10 + 1,
            passage % 10 + 1
        ),
        "xlsx" => format!(
            "sheet=Rates_{};headers=A1:E1;cells=A{}:E{}",
            passage / 40 + 1,
            passage % 40 + 2,
            passage % 40 + 2
        ),
        "csv" => format!(
            "record={};cells=A{}:D{}",
            passage + 1,
            passage + 2,
            passage + 2
        ),
        "pptx" if passage % 4 == 3 => format!(
            "slide={};shape-tree=4;graphic-id=6;table=1;headers=A1:B1;cells=A2:B2",
            passage / 4 + 1
        ),
        "pptx" => format!(
            "slide={};shape-tree={};shape-id={}",
            passage / 4 + 1,
            passage % 4 + 1,
            passage % 4 + 2
        ),
        "txt_md" => format!("line={}-{}", passage * 3 + 1, passage * 3 + 3),
        _ => unreachable!("fixed format plan"),
    }
}

fn structured_locator(format: &str, passage: usize) -> Value {
    serde_json::json!({"format":format,"logical":locator(format, passage)})
}

fn structural_parent(format: &str, file: usize, passage: usize) -> String {
    if format == "docx" && file == 25 && matches!(passage, 4 | 5) {
        "docx-atlas-split".to_owned()
    } else {
        format!("{format}-passage-{passage:04}")
    }
}

fn continuation_direction(format: &str, file: usize, passage: usize) -> &'static str {
    if format == "docx" && file == 25 && passage == 4 {
        "to_next"
    } else if format == "docx" && file == 25 && passage == 5 {
        "from_previous"
    } else {
        "none"
    }
}

fn authored_fact(format: &str, file: usize, passage: usize, last: usize) -> &'static str {
    match (format, file, passage) {
        ("pdf", 0, 0) => "Early evidence: Teal Finch control owner is Ravi.",
        ("docx", 25, 3) => {
            "Amber Lantern approval owner is Zoë Chen. Northstar renewal approval requires finance review."
        }
        ("docx", 25, 4) => "Atlas review packet section one requires legal approval.",
        ("docx", 25, 5) => "Adjacent section two requires security approval.",
        ("xlsx", 45, 4) => {
            "Silver Heron rate formula expression =C6+0.005; cached type float; cached value 0.08. Northstar renewal approval rate is cached evidence, not recalculated truth."
        }
        ("csv", 65, 5) => {
            "Quoted account: \"Acme, Ltd.\"; multiline note: Calls\nrenew in October; identifier: 00127."
        }
        ("pptx", 80, 6) => "Cobalt Harbor launch checkpoint is Thursday.",
        ("pdf", 2, 7) => {
            "Jade River exception requires two reviewers. Northstar renewal approval exception is current."
        }
        ("txt_md", 95, 2) => "Unicode travel marker: naïve café 東京 — rail code FALCON.",
        ("pdf", 0, p) if p == last => "Late-file evidence: Obsidian Kestrel retention is 37 days.",
        ("docx", 25, p) if p == last => "Late-file evidence: Saffron Lynx escalation is Friday.",
        ("xlsx", 45, p) if p == last => "Late-file evidence: Indigo Wren saved rate is 0.14.",
        ("csv", 65, p) if p == last => "Late-file evidence: Copper Auk account code is 9081.",
        ("pptx", 80, p) if p == last => "Late-file evidence: Umber Seal checkpoint is Monday.",
        (_, f, 1) if (8..12).contains(&f) => "Current repeated fact: shared cadence is quarterly.",
        (_, f, 1) if (12..16).contains(&f) => {
            "Archived near-match: shared cadence was quarterly; current cadence is monthly."
        }
        ("docx", 26, 3) => {
            "Superseded near-match: Amber Lantern draft owner was Nia, not the approval owner."
        }
        ("pptx", 81, 6) => {
            "Relevant-looking distractor: Northstar renewal approval training is postponed."
        }
        _ => "Synthetic distractor: routine planning notes have no benchmark answer.",
    }
}

fn generate_file(scale: Scale, file: usize) -> Vec<Passage> {
    let plan = format_plan();
    let format = plan[file];
    let count = scale.passages_per_file() + usize::from(LARGE_FILE_INDEXES.contains(&file)) * 8;
    let file_id = format!("office-{}-{format}-{file:03}", scale.name());
    let structures: Vec<_> = (0..count)
        .map(|index| {
            let locator = locator(format, index);
            let fact = authored_fact(format, file, index, count - 1);
            let text = match format {
                "pdf" => format!("Quarterly Operations Briefing\n{fact}"),
                "docx" => format!("Operations handbook heading\n{fact}"),
                "xlsx" => format!("Region: SG\nRecord: {index:05}\n{fact}"),
                "csv" => format!("Account: synthetic-{file:03}\nOwner: Test User\n{fact}"),
                "pptx" => format!("Slide title: Delivery Review\n{fact}"),
                "txt_md" => format!("## Synthetic section\n{fact}"),
                _ => unreachable!("fixed format plan"),
            };
            (locator, text)
        })
        .collect();
    let canonical = serde_json::to_vec(&(scale.name(), &file_id, format, &structures))
        .expect("canonical synthetic file");
    let source_sha256 = digest(&canonical);
    let source_revision = format!("sha256:{source_sha256}");
    let source_revision_id = source_sha256.clone();
    let extraction_set_id = format!("set-{}-{format}-{file:03}", scale.name());
    structures
        .into_iter()
        .enumerate()
        .map(|(index, (locator_text, body))| {
            let key = format!("{file_id}:passage:{index:04}");
            let text = format!(
                "source_sha256={source_sha256}\nsource_revision={source_revision}\nfile_id={file_id}\nformat={format}\nlocator={locator_text}\n{body}"
            );
            Passage {
                key,
                file_id: file_id.clone(),
                format,
                source_sha256: source_sha256.clone(),
                source_revision: source_revision.clone(),
                source_revision_id: source_revision_id.clone(),
                extraction_set_id: extraction_set_id.clone(),
                passage_id: format!("passage-{index:04}"),
                structural_parent: structural_parent(format, file, index),
                passage_order: index + 1,
                continuation_direction: continuation_direction(format, file, index),
                locator: structured_locator(format, index),
                text,
                item_id: file_id.clone(),
                revision_id: format!("{file_id}-revision"),
            }
        })
        .collect()
}

fn generate(scale: Scale) -> Vec<Passage> {
    let mut passages = Vec::with_capacity(scale.passages_per_file() * FILES_PER_SCALE + 40);
    for file in (0..FILES_PER_SCALE).map(|position| position * 37 % FILES_PER_SCALE) {
        passages.extend(generate_file(scale, file));
    }
    passages
}

fn key(scale: Scale, format: &str, file: usize, passage: usize) -> String {
    format!(
        "office-{}-{format}-{file:03}:passage:{passage:04}",
        scale.name()
    )
}

fn expected(
    scale: Scale,
    format: &'static str,
    file: usize,
    passage: usize,
    source_sha256: &str,
    locator: &str,
    body: &str,
) -> ExpectedEvidence {
    let file_id = format!("office-{}-{format}-{file:03}", scale.name());
    let source_revision = format!("sha256:{source_sha256}");
    ExpectedEvidence {
        key: key(scale, format, file, passage),
        format,
        item_id: file_id.clone(),
        revision_id: format!("{file_id}-revision"),
        source_sha256: source_sha256.to_owned(),
        source_revision: source_revision.clone(),
        source_revision_id: source_sha256.to_owned(),
        extraction_set_id: format!("set-{}-{format}-{file:03}", scale.name()),
        passage_id: format!("passage-{passage:04}"),
        locator: serde_json::json!({"format":format,"logical":locator}),
        text: format!(
            "source_sha256={source_sha256}\nsource_revision={source_revision}\nfile_id={file_id}\nformat={format}\nlocator={locator}\n{body}"
        ),
    }
}

fn frozen_source_sha(scale: Scale, file: usize) -> &'static str {
    match (scale, file) {
        (Scale::Thousand, 0) => "63bcb01b4682dcb87bc7007cea9579afaf86a144a063bb901638404cb4219033",
        (Scale::Thousand, 2) => "c62467fa4e81aae81528053a267ec8e4034c10248ca8ba13c57f5f6453dfdbf3",
        (Scale::Thousand, 8) => "15dc614f3246737dc193edb5ed315a61b3e998bd440ed80faf8ae7ff7b1a6f9e",
        (Scale::Thousand, 9) => "590e814614e64b8322c9ac48a88443a7ff4698b6745380fba18d29dd25dd3103",
        (Scale::Thousand, 10) => "66927c3cf59c4ae4daed99ccaade1b1fe02081e96feefa7e9d3552fdaeb1a23e",
        (Scale::Thousand, 11) => "e39bff5f4f2fe26d511f035d4452c58cd7145104122ccf34dee25a0223006981",
        (Scale::Thousand, 25) => "bb730ed9ebc9358552a35590635d5d8fb9daa2d89cd8c2ddd6cb749a2adf7423",
        (Scale::Thousand, 45) => "a6bbcfe7436d586af25ed4cb43fa810fddc699b61a2f0a2793f6dc8ff19e565e",
        (Scale::Thousand, 65) => "194bc2321c6f8a32c3f04fea7d2f87a985ec9179a982d5ab703ef204deaf5016",
        (Scale::Thousand, 80) => "3ec763776a261d65557803d95852f71259abe2b883a4443d5cc8f7a69d79c842",
        (Scale::Thousand, 95) => "9f51f00e6ae8e9da2695aec13b0f06af6a7ab7191f49abf23c0ab9cb3e9994ce",
        (Scale::TenThousand, 0) => {
            "6cd48ca10e45a23f11e9f16b83fe1d07ad6f8e3cc11f177abcd006e639efa1c6"
        }
        (Scale::TenThousand, 2) => {
            "e0732021e24b36b07a2a5338a7925eb74abeb01246eac61bfb3d328552bd30e9"
        }
        (Scale::TenThousand, 8) => {
            "6a1f097bf384a85513633601c321f7bd20704363f0dd8e2271f160a9c3d39fe6"
        }
        (Scale::TenThousand, 9) => {
            "4813890085a0ca24c3a1d5972bface089ca8c1ef6c4d8a59c693fa483c952fe1"
        }
        (Scale::TenThousand, 10) => {
            "b05063c9372a43501316d75d90737dfb449c35ce0059cd3be0b13c415f762ae4"
        }
        (Scale::TenThousand, 11) => {
            "66f02debee1d1927af20c4b37e15cb52dda4a35f6427999f5b6cfaa6333b1f37"
        }
        (Scale::TenThousand, 25) => {
            "b8a0bfd22e0d94fd495f0906117fa30fb7d0bca98403b3a6a2eb2b6f9d8e941e"
        }
        (Scale::TenThousand, 45) => {
            "ae741b728b0e6dd42193760422597e547a9ce778150f9595b2ecff4709553e7c"
        }
        (Scale::TenThousand, 65) => {
            "5584c8d0e908f407acfc7c93c81bf33317151717d9b5ae685172389c6ba869fe"
        }
        (Scale::TenThousand, 80) => {
            "4d255be86886a1bf220558a1827b893e6c45a9dabc0d0f6e21147b4ffd80e6c7"
        }
        (Scale::TenThousand, 95) => {
            "e25852de2f75d9552d3202198e81d88cb0946d952268fd58b5bb15c84668ec33"
        }
        _ => panic!("missing frozen source hash"),
    }
}

fn gold(scale: Scale, format: &'static str, file: usize, passage: usize) -> ExpectedEvidence {
    let (locator, body) = match (format, file, passage) {
        ("pdf", 0, 0) => (
            "page=1;block=1;type=title;bbox=247.5,696.0,117.0,18.0",
            "Quarterly Operations Briefing\nEarly evidence: Teal Finch control owner is Ravi.",
        ),
        ("docx", 25, 3) => (
            "heading=Operations/Region 1;paragraph=4",
            "Operations handbook heading\nAmber Lantern approval owner is Zoë Chen. Northstar renewal approval requires finance review.",
        ),
        ("docx", 25, 4) => (
            "heading=Operations/Region 1;table=1;row=2;headers=A1:B1;cells=A2:B2",
            "Operations handbook heading\nAtlas review packet section one requires legal approval.",
        ),
        ("docx", 25, 5) => (
            "heading=Operations/Region 1;paragraph=6",
            "Operations handbook heading\nAdjacent section two requires security approval.",
        ),
        ("xlsx", 45, 4) => (
            "sheet=Rates_1;headers=A1:E1;cells=A6:E6",
            "Region: SG\nRecord: 00004\nSilver Heron rate formula expression =C6+0.005; cached type float; cached value 0.08. Northstar renewal approval rate is cached evidence, not recalculated truth.",
        ),
        ("csv", 65, 5) => (
            "record=6;cells=A7:D7",
            "Account: synthetic-065\nOwner: Test User\nQuoted account: \"Acme, Ltd.\"; multiline note: Calls\nrenew in October; identifier: 00127.",
        ),
        ("pptx", 80, 6) => (
            "slide=2;shape-tree=3;shape-id=4",
            "Slide title: Delivery Review\nCobalt Harbor launch checkpoint is Thursday.",
        ),
        ("pdf", 2, 7) => (
            "page=2;block=4;type=table;bbox=108.0,530.0,396.0,56.0",
            "Quarterly Operations Briefing\nJade River exception requires two reviewers. Northstar renewal approval exception is current.",
        ),
        ("txt_md", 95, 2) => (
            "line=7-9",
            "## Synthetic section\nUnicode travel marker: naïve café 東京 — rail code FALCON.",
        ),
        ("pdf", 0, p) if p == scale.passages_per_file() + 7 => (
            if scale == Scale::Thousand {
                "page=5;block=2;type=paragraph;bbox=78.0,646.0,190.0,10.0"
            } else {
                "page=27;block=4;type=table;bbox=108.0,530.0,396.0,56.0"
            },
            "Quarterly Operations Briefing\nLate-file evidence: Obsidian Kestrel retention is 37 days.",
        ),
        _ => panic!("missing frozen gold evidence"),
    };
    expected(
        scale,
        format,
        file,
        passage,
        frozen_source_sha(scale, file),
        locator,
        body,
    )
}

fn repeated_gold(scale: Scale) -> Vec<ExpectedEvidence> {
    (8..12)
        .map(|file| {
            expected(
                scale,
                "pdf",
                file,
                1,
                frozen_source_sha(scale, file),
                "page=1;block=2;type=paragraph;bbox=78.0,646.0,190.0,10.0",
                "Quarterly Operations Briefing\nCurrent repeated fact: shared cadence is quarterly.",
            )
        })
        .collect()
}

fn large_gold(format: &'static str) -> ExpectedEvidence {
    match format {
        "pdf" => gold(Scale::TenThousand, "pdf", 0, 107),
        "docx" => expected(
            Scale::TenThousand,
            "docx",
            25,
            107,
            "b8a0bfd22e0d94fd495f0906117fa30fb7d0bca98403b3a6a2eb2b6f9d8e941e",
            "heading=Operations/Region 11;paragraph=8",
            "Operations handbook heading\nLate-file evidence: Saffron Lynx escalation is Friday.",
        ),
        "xlsx" => expected(
            Scale::TenThousand,
            "xlsx",
            45,
            107,
            "ae741b728b0e6dd42193760422597e547a9ce778150f9595b2ecff4709553e7c",
            "sheet=Rates_3;headers=A1:E1;cells=A29:E29",
            "Region: SG\nRecord: 00107\nLate-file evidence: Indigo Wren saved rate is 0.14.",
        ),
        "csv" => expected(
            Scale::TenThousand,
            "csv",
            65,
            107,
            "5584c8d0e908f407acfc7c93c81bf33317151717d9b5ae685172389c6ba869fe",
            "record=108;cells=A109:D109",
            "Account: synthetic-065\nOwner: Test User\nLate-file evidence: Copper Auk account code is 9081.",
        ),
        "pptx" => expected(
            Scale::TenThousand,
            "pptx",
            80,
            107,
            "4d255be86886a1bf220558a1827b893e6c45a9dabc0d0f6e21147b4ffd80e6c7",
            "slide=27;shape-tree=4;graphic-id=6;table=1;headers=A1:B1;cells=A2:B2",
            "Slide title: Delivery Review\nLate-file evidence: Umber Seal checkpoint is Monday.",
        ),
        _ => unreachable!("large format"),
    }
}

#[allow(clippy::too_many_lines)] // Frozen held-out cases stay explicit instead of being derived from the corpus.
fn queries(scale: Scale) -> Vec<Query> {
    let late = scale.passages_per_file() + 7;
    vec![
        Query {
            id: "early_pdf",
            text: "teal finch",
            category: "early_document",
            answer: Some("Ravi owns the Teal Finch control."),
            required: vec![gold(scale, "pdf", 0, 0)],
        },
        Query {
            id: "single_docx",
            text: "amber lantern",
            category: "single_passage",
            answer: Some("Zoë Chen owns Amber Lantern approval."),
            required: vec![gold(scale, "docx", 25, 3)],
        },
        Query {
            id: "single_xlsx_formula",
            text: "silver heron",
            category: "single_passage",
            answer: Some(
                "The saved Silver Heron formula is =C6+0.005 with a float cache of 0.08; it is not a recalculated truth claim.",
            ),
            required: vec![gold(scale, "xlsx", 45, 4)],
        },
        Query {
            id: "single_csv_multiline",
            text: "Acme October",
            category: "single_passage",
            answer: Some("Acme, Ltd. has a multiline October renewal note and identifier 00127."),
            required: vec![gold(scale, "csv", 65, 5)],
        },
        Query {
            id: "single_pptx",
            text: "cobalt harbor",
            category: "single_passage",
            answer: Some("The Cobalt Harbor checkpoint is Thursday."),
            required: vec![gold(scale, "pptx", 80, 6)],
        },
        Query {
            id: "single_pdf",
            text: "jade river",
            category: "single_passage",
            answer: Some("Jade River requires two reviewers."),
            required: vec![gold(scale, "pdf", 2, 7)],
        },
        Query {
            id: "single_unicode_text",
            text: "FALCON",
            category: "single_passage",
            answer: Some("The Unicode travel marker uses rail code FALCON."),
            required: vec![gold(scale, "txt_md", 95, 2)],
        },
        Query {
            id: "multi_passage",
            text: "atlas review packet",
            category: "multi_passage",
            answer: Some("The Atlas packet requires legal and security approval."),
            required: vec![gold(scale, "docx", 25, 4), gold(scale, "docx", 25, 5)],
        },
        Query {
            id: "cross_format",
            text: "northstar renewal approval",
            category: "cross_file_cross_format",
            answer: Some(
                "Northstar renewal approval requires finance review, has a saved 0.08 rate cache, and has a current two-reviewer exception.",
            ),
            required: vec![
                gold(scale, "docx", 25, 3),
                gold(scale, "xlsx", 45, 4),
                gold(scale, "pdf", 2, 7),
            ],
        },
        Query {
            id: "late_pdf",
            text: "obsidian kestrel",
            category: "late_document",
            answer: Some("Obsidian Kestrel retention is 37 days."),
            required: vec![gold(scale, "pdf", 0, late)],
        },
        Query {
            id: "repeated_fact",
            text: "shared cadence",
            category: "repeated_fact_precision",
            answer: Some("Four current sources say the shared cadence is quarterly."),
            required: repeated_gold(scale),
        },
        Query {
            id: "no_answer",
            text: "nonexistent narwhal titanium",
            category: "no_answer_exclusion",
            answer: None,
            required: vec![],
        },
        Query {
            id: "visible_paraphrase_miss",
            text: "who manages the yellow light approval",
            category: "paraphrase",
            answer: Some("Zoë Chen owns Amber Lantern approval."),
            required: vec![gold(scale, "docx", 25, 3)],
        },
    ]
}

fn evidence_matches(expected: &ExpectedEvidence, actual: &Passage) -> bool {
    expected.key == actual.key
        && expected.format == actual.format
        && expected.item_id == actual.item_id
        && expected.revision_id == actual.revision_id
        && expected.source_sha256 == actual.source_sha256
        && expected.source_revision == actual.source_revision
        && expected.source_revision_id == actual.source_revision_id
        && expected.extraction_set_id == actual.extraction_set_id
        && expected.passage_id == actual.passage_id
        && expected.locator == actual.locator
        && expected.text == actual.text
}

fn citation_matches_manifest(actual: &Passage, manifest: &[Passage]) -> bool {
    manifest.iter().any(|expected| {
        expected.key == actual.key
            && expected.item_id == actual.item_id
            && expected.revision_id == actual.revision_id
            && expected.source_sha256 == actual.source_sha256
            && expected.source_revision == actual.source_revision
            && expected.source_revision_id == actual.source_revision_id
            && expected.extraction_set_id == actual.extraction_set_id
            && expected.passage_id == actual.passage_id
            && expected.structural_parent == actual.structural_parent
            && expected.passage_order == actual.passage_order
            && expected.continuation_direction == actual.continuation_direction
            && expected.locator == actual.locator
            && expected.text == actual.text
    })
}

fn score(
    required: &[ExpectedEvidence],
    supplied: &[&Passage],
    context_bytes: usize,
    citation_manifest: &[Passage],
) -> VariantScore {
    let matched = required
        .iter()
        .filter(|expected| {
            supplied
                .iter()
                .any(|actual| evidence_matches(expected, actual))
        })
        .count();
    let citation_valid = supplied
        .iter()
        .filter(|actual| citation_matches_manifest(actual, citation_manifest))
        .count();
    VariantScore {
        required: required.len(),
        supplied: supplied.len(),
        matched,
        full_support: matched == required.len(),
        recall: ratio(matched, required.len()),
        evidence_precision: ratio(matched, supplied.len()),
        citation_valid,
        citation_total: supplied.len(),
        citation_validity: ratio(citation_valid, supplied.len()),
        context_bytes,
    }
}

fn append_clipped(context: &mut String, text: &str, budget: usize) -> bool {
    if context.len() == budget {
        return false;
    }
    if !context.is_empty() {
        let separator = "\n\n";
        let room = budget - context.len();
        context.push_str(&separator[..room.min(separator.len())]);
    }
    let room = budget - context.len();
    let mut end = room.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    context.push_str(&text[..end]);
    end == text.len()
}

fn append_whole(context: &mut String, text: &str, budget: usize) -> bool {
    let separator = usize::from(!context.is_empty()) * 2;
    if context.len() + separator + text.len() > budget {
        return false;
    }
    if separator != 0 {
        context.push_str("\n\n");
    }
    context.push_str(text);
    true
}

fn passage_number(passage: &Passage) -> usize {
    passage.passage_order - 1
}

fn same_extraction(left: &Passage, right: &Passage) -> bool {
    left.item_id == right.item_id
        && left.revision_id == right.revision_id
        && left.source_revision_id == right.source_revision_id
        && left.extraction_set_id == right.extraction_set_id
}

fn neighbors<'a>(hit: &Passage, manifest: &'a [Passage]) -> Vec<&'a Passage> {
    let number = passage_number(hit);
    let adjacent = [number.checked_sub(1), number.checked_add(1)];
    adjacent
        .into_iter()
        .flatten()
        .filter_map(|number| {
            manifest.iter().find(|candidate| {
                candidate.file_id == hit.file_id
                    && passage_number(candidate) == number
                    && same_extraction(hit, candidate)
                    && candidate.source_sha256 == hit.source_sha256
            })
        })
        .collect()
}

fn continues_to_previous(passage: &Passage) -> bool {
    matches!(passage.continuation_direction, "from_previous" | "both")
}

fn continues_to_next(passage: &Passage) -> bool {
    matches!(passage.continuation_direction, "to_next" | "both")
}

fn continuation_neighbors<'a>(hit: &Passage, manifest: &'a [Passage]) -> Vec<&'a Passage> {
    neighbors(hit, manifest)
        .into_iter()
        .filter(|candidate| {
            hit.structural_parent == candidate.structural_parent
                && if candidate.passage_order < hit.passage_order {
                    continues_to_previous(hit) && continues_to_next(candidate)
                } else {
                    continues_to_next(hit) && continues_to_previous(candidate)
                }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RetrievalPolicy {
    DirectOnly,
    UnconditionalAdjacent,
    ContinuationAware,
    SmallOverlap,
}

struct PolicyResult<'a> {
    supplied: Vec<&'a Passage>,
    context_bytes: usize,
    expansion_count: usize,
    useful_expansions: usize,
    budget_rejected: usize,
    duplicated_overlap_bytes: usize,
    overlap_fragments: Vec<OverlapFragment<'a>>,
    complete_evidence_in_overlap: usize,
}

struct OverlapFragment<'a> {
    source: &'a Passage,
    text: &'a str,
}

fn source_span_identity(passage: &Passage) -> (String, String, String, String, String) {
    (
        passage.item_id.clone(),
        passage.revision_id.clone(),
        passage.source_revision_id.clone(),
        passage.extraction_set_id.clone(),
        passage.locator.to_string(),
    )
}

#[allow(clippy::too_many_lines)] // Policy packing stays together so all variants share one budget path.
fn pack_policy<'a>(
    policy: RetrievalPolicy,
    direct_hits: &[&'a Passage],
    manifest: &'a [Passage],
    required: &[ExpectedEvidence],
) -> PolicyResult<'a> {
    let mut supplied = Vec::new();
    let mut included = BTreeSet::new();
    let mut per_source = BTreeMap::new();
    let mut context_bytes = 0;
    let mut budget_rejected = 0;
    for hit in direct_hits {
        let source = (
            hit.item_id.as_str(),
            hit.revision_id.as_str(),
            hit.source_revision_id.as_str(),
            hit.extraction_set_id.as_str(),
        );
        let bytes = hit.text.len() + usize::from(!supplied.is_empty()) * 2;
        if context_bytes + bytes <= CONTEXT_BUDGET
            && per_source.get(&source).copied().unwrap_or(0) < 4
            && included.insert(source_span_identity(hit))
        {
            context_bytes += bytes;
            *per_source.entry(source).or_insert(0) += 1;
            supplied.push(*hit);
        } else {
            budget_rejected += 1;
        }
    }
    let direct_count = supplied.len();
    let candidates: Vec<_> = match policy {
        RetrievalPolicy::DirectOnly | RetrievalPolicy::SmallOverlap => Vec::new(),
        RetrievalPolicy::UnconditionalAdjacent => direct_hits
            .iter()
            .flat_map(|hit| neighbors(hit, manifest))
            .collect(),
        RetrievalPolicy::ContinuationAware => direct_hits
            .iter()
            .flat_map(|hit| continuation_neighbors(hit, manifest))
            .collect(),
    };
    for candidate in candidates {
        let source = (
            candidate.item_id.as_str(),
            candidate.revision_id.as_str(),
            candidate.source_revision_id.as_str(),
            candidate.extraction_set_id.as_str(),
        );
        if included.contains(&source_span_identity(candidate)) {
            continue;
        }
        let bytes = candidate.text.len() + usize::from(!supplied.is_empty()) * 2;
        if per_source.get(&source).copied().unwrap_or(0) == 4
            || context_bytes + bytes > CONTEXT_BUDGET
        {
            budget_rejected += 1;
            continue;
        }
        included.insert(source_span_identity(candidate));
        context_bytes += bytes;
        *per_source.entry(source).or_insert(0) += 1;
        supplied.push(candidate);
    }
    let mut duplicated_overlap_bytes = 0;
    let mut overlap_fragments = Vec::new();
    let mut complete_evidence_in_overlap = 0;
    if policy == RetrievalPolicy::SmallOverlap {
        for neighbor in direct_hits
            .iter()
            .flat_map(|hit| continuation_neighbors(hit, manifest))
        {
            if included.contains(&source_span_identity(neighbor)) {
                continue;
            }
            let mut overlap = neighbor.text.len().min(64);
            while !neighbor.text.is_char_boundary(overlap) {
                overlap -= 1;
            }
            if context_bytes + overlap <= CONTEXT_BUDGET {
                context_bytes += overlap;
                duplicated_overlap_bytes += overlap;
                overlap_fragments.push(OverlapFragment {
                    source: neighbor,
                    text: &neighbor.text[..overlap],
                });
                complete_evidence_in_overlap += usize::from(
                    overlap == neighbor.text.len()
                        && required
                            .iter()
                            .any(|expected| evidence_matches(expected, neighbor)),
                );
            } else {
                budget_rejected += 1;
            }
        }
    }
    let useful_expansions = supplied[direct_count..]
        .iter()
        .filter(|passage| {
            required
                .iter()
                .any(|expected| evidence_matches(expected, passage))
        })
        .count();
    PolicyResult {
        expansion_count: supplied.len() - direct_count,
        useful_expansions,
        supplied,
        context_bytes,
        budget_rejected,
        duplicated_overlap_bytes,
        overlap_fragments,
        complete_evidence_in_overlap,
    }
}

fn fixed_context(passages: &[Passage], budget: usize) -> (String, Vec<&Passage>) {
    let mut context = String::with_capacity(budget);
    let mut supplied = Vec::new();
    for passage in passages {
        if append_clipped(&mut context, &passage.text, budget) {
            supplied.push(passage);
        } else {
            break;
        }
    }
    (context, supplied)
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return f64::from(numerator == 0);
    }
    f64::from(u32::try_from(numerator).expect("bounded benchmark numerator"))
        / f64::from(u32::try_from(denominator).expect("bounded benchmark denominator"))
}

fn percentile(samples: &[f64], percentile: usize) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    ordered[(ordered.len() * percentile).div_ceil(100) - 1]
}

fn aggregate(cases: &[Value], variant: &str) -> Value {
    let answerable: Vec<_> = cases
        .iter()
        .filter(|case| case[variant]["required"].as_u64().expect("required") > 0)
        .collect();
    let sum = |field: &str| {
        cases
            .iter()
            .map(|case| case[variant][field].as_u64().expect("score count"))
            .sum::<u64>()
    };
    let required = sum("required");
    let supplied = sum("supplied");
    let matched = sum("matched");
    let citation_total = sum("citation_total");
    let citation_valid = sum("citation_valid");
    let full = answerable
        .iter()
        .filter(|case| case[variant]["full_support"] == true)
        .count();
    serde_json::json!({
        "cases":cases.len(),
        "answerable_cases":answerable.len(),
        "required_evidence":required,
        "supplied_evidence":supplied,
        "matched_evidence":matched,
        "fully_supported_answerable_cases":full,
        "full_support_rate":ratio(full,answerable.len()),
        "recall":ratio(usize::try_from(matched).expect("bounded matched"),usize::try_from(required).expect("bounded required")),
        "evidence_precision":ratio(usize::try_from(matched).expect("bounded matched"),usize::try_from(supplied).expect("bounded supplied")),
        "citation_valid":citation_valid,
        "citation_total":citation_total,
        "citation_validity":ratio(usize::try_from(citation_valid).expect("bounded citations"),usize::try_from(citation_total).expect("bounded citations")),
        "context_bytes":sum("context_bytes"),
    })
}

fn aggregate_production(cases: &[Value]) -> Value {
    let mut result = aggregate(cases, "production_automatic");
    let sum = |field: &str| {
        cases
            .iter()
            .map(|case| {
                case["production_automatic"][field]
                    .as_u64()
                    .expect("production score count")
            })
            .sum::<u64>()
    };
    let expansions = sum("expansion_count");
    let useful = sum("useful_expansions");
    let truncated_queries = cases
        .iter()
        .filter(|case| case["production_automatic"]["truncated"] == true)
        .count();
    let object = result.as_object_mut().expect("aggregate score object");
    object.insert("expansion_count".to_owned(), expansions.into());
    object.insert("useful_expansions".to_owned(), useful.into());
    object.insert(
        "useful_expansion_yield".to_owned(),
        ratio(
            usize::try_from(useful).expect("bounded useful expansions"),
            usize::try_from(expansions).expect("bounded expansions"),
        )
        .into(),
    );
    object.insert("truncated_queries".to_owned(), truncated_queries.into());
    result
}

fn citation_value_matches_manifest(actual: &Value, manifest: &[Passage]) -> bool {
    manifest.iter().any(|expected| {
        actual["key"].as_str() == Some(expected.key.as_str())
            && actual["format"].as_str() == Some(expected.format)
            && actual["item_id"].as_str() == Some(expected.item_id.as_str())
            && actual["revision_id"].as_str() == Some(expected.revision_id.as_str())
            && actual["source_sha256"].as_str() == Some(expected.source_sha256.as_str())
            && actual["source_revision"].as_str() == Some(expected.source_revision.as_str())
            && actual["source_revision_id"].as_str() == Some(expected.source_revision_id.as_str())
            && actual["extraction_set_id"].as_str() == Some(expected.extraction_set_id.as_str())
            && actual["passage_id"].as_str() == Some(expected.passage_id.as_str())
            && actual["structural_parent"].as_str() == Some(expected.structural_parent.as_str())
            && actual["passage_order"].as_u64()
                == Some(u64::try_from(expected.passage_order).expect("bounded passage order"))
            && actual["continuation_direction"].as_str() == Some(expected.continuation_direction)
            && actual.get("locator") == Some(&expected.locator)
            && actual["text"].as_str() == Some(expected.text.as_str())
    })
}

fn by_format(cases: &[Value], evidence_field: &str, citation_manifest: &[Passage]) -> Value {
    let mut result = serde_json::Map::new();
    for format in ["pdf", "docx", "xlsx", "csv", "pptx", "txt_md"] {
        let mut required = 0usize;
        let mut supplied = 0usize;
        let mut matched = 0usize;
        let mut citation_valid = 0usize;
        for case in cases {
            let returned: Vec<_> = case[evidence_field]
                .as_array()
                .expect("supplied evidence")
                .iter()
                .filter(|passage| passage["format"] == format)
                .collect();
            supplied += returned.len();
            citation_valid += returned
                .iter()
                .filter(|actual| citation_value_matches_manifest(actual, citation_manifest))
                .count();
            for passage in case["required_evidence"]
                .as_array()
                .expect("required evidence")
                .iter()
                .filter(|passage| passage["format"] == format)
            {
                required += 1;
                matched += usize::from(returned.iter().any(|actual| {
                    [
                        "key",
                        "format",
                        "item_id",
                        "revision_id",
                        "source_sha256",
                        "source_revision",
                        "source_revision_id",
                        "extraction_set_id",
                        "passage_id",
                        "locator",
                        "text",
                    ]
                    .iter()
                    .all(|field| actual[field] == passage[field])
                }));
            }
        }
        result.insert(
            format.to_owned(),
            serde_json::json!({
                "required_evidence":required,
                "supplied_evidence":supplied,
                "matched_evidence":matched,
                "recall":ratio(matched,required),
                "evidence_precision":ratio(matched,supplied),
                "citation_valid":citation_valid,
                "citation_total":supplied,
                "citation_validity":ratio(citation_valid,supplied),
            }),
        );
    }
    Value::Object(result)
}

fn assert_format_citation_reconciliation(pooled: &Value, formats: &Value) {
    for field in ["citation_valid", "citation_total"] {
        let format_sum = formats
            .as_object()
            .expect("format scores")
            .values()
            .map(|score| score[field].as_u64().expect("format citation count"))
            .sum::<u64>();
        assert_eq!(
            format_sum,
            pooled[field].as_u64().expect("pooled citation count")
        );
    }
}

fn assert_corpus_contract(scale: Scale, corpus: &[Passage]) {
    assert_eq!(
        corpus.len(),
        scale.passages_per_file() * FILES_PER_SCALE + 40
    );
    let files: BTreeSet<_> = corpus.iter().map(|passage| &passage.file_id).collect();
    let sources: BTreeSet<_> = corpus
        .iter()
        .map(|passage| &passage.source_sha256)
        .collect();
    assert_eq!(files.len(), FILES_PER_SCALE);
    assert_eq!(sources.len(), FILES_PER_SCALE);
    assert_eq!(
        corpus
            .iter()
            .map(|passage| (
                &passage.source_sha256,
                &passage.source_revision,
                passage.locator.to_string()
            ))
            .collect::<BTreeSet<_>>()
            .len(),
        corpus.len()
    );
    for (format, expected) in [
        ("pdf", 25),
        ("docx", 20),
        ("xlsx", 20),
        ("csv", 15),
        ("pptx", 15),
        ("txt_md", 5),
    ] {
        assert_eq!(
            corpus
                .iter()
                .filter(|passage| passage.format == format)
                .map(|passage| &passage.file_id)
                .collect::<BTreeSet<_>>()
                .len(),
            expected
        );
    }
    for passage in corpus {
        assert_eq!(
            passage.source_revision,
            format!("sha256:{}", passage.source_sha256)
        );
        assert_eq!(passage.source_revision_id, passage.source_sha256);
        assert!((1..=32 * 1024).contains(&passage.text.len()));
    }
    let drifts: Vec<_> = queries(scale)
        .into_iter()
        .flat_map(|query| query.required)
        .filter_map(|expected| {
            let actual = corpus
                .iter()
                .find(|passage| passage.key == expected.key)
                .expect("frozen oracle passage exists");
            (!evidence_matches(&expected, actual)).then(|| {
                format!(
                    "{} expected source {} actual {}",
                    expected.key, expected.source_sha256, actual.source_sha256
                )
            })
        })
        .collect();
    assert!(
        drifts.is_empty(),
        "frozen oracle drift:\n{}",
        drifts.join("\n")
    );
}

fn expected_keyword_counts(case_id: &str) -> (usize, usize) {
    match case_id {
        "early_pdf"
        | "single_xlsx_formula"
        | "single_csv_multiline"
        | "single_pptx"
        | "single_pdf"
        | "single_unicode_text"
        | "late_pdf"
        | "multi_passage" => (1, 1),
        "single_docx" => (1, 2),
        "cross_format" => (3, 4),
        "repeated_fact" => (4, 8),
        "no_answer" | "visible_paraphrase_miss" => (0, 0),
        _ => panic!("unknown frozen case"),
    }
}

fn keyword_counts_match(case_id: &str, matched: usize, supplied: usize) -> bool {
    expected_keyword_counts(case_id) == (matched, supplied)
}

#[allow(clippy::too_many_lines)] // Frozen benchmark gates stay together and explicit.
fn assert_case_gates(cases: &[Value], fixed_context: &str) {
    assert_eq!(cases.len(), 13);
    assert!(
        fixed_context.len() <= CONTEXT_BUDGET
            && fixed_context.is_char_boundary(fixed_context.len())
    );
    for case in cases {
        for policy in [
            "direct_only",
            "legacy_unconditional_adjacent",
            "continuation_aware",
            "small_overlap",
        ] {
            assert!(case[policy]["context_bytes"].as_u64().expect("bytes") <= 4096);
            assert_eq!(
                case[policy]["citation_valid"],
                case[policy]["citation_total"]
            );
            assert!(
                case[policy]["supplied_evidence"]
                    .as_array()
                    .expect("policy evidence")
                    .starts_with(
                        case["direct_hit_evidence"]
                            .as_array()
                            .expect("direct evidence")
                    )
            );
        }
        assert_eq!(
            case["production_automatic_evidence"], case["continuation_aware_evidence"],
            "native ordered evidence must match the frozen continuation selection"
        );
        assert_eq!(
            case["production_automatic"]["citation_valid"],
            case["production_automatic"]["citation_total"]
        );
        assert_eq!(
            case["production_automatic"]["context_bytes"],
            case["production_automatic_evidence"]
                .as_array()
                .expect("native evidence")
                .iter()
                .map(|passage| passage["text"].as_str().expect("passage text").len())
                .sum::<usize>()
        );
        let case_id = case["id"].as_str().expect("case ID");
        let matched = usize::try_from(case["direct_only"]["matched"].as_u64().expect("matched"))
            .expect("bounded matched");
        let supplied = usize::try_from(case["direct_only"]["supplied"].as_u64().expect("supplied"))
            .expect("bounded supplied");
        assert!(keyword_counts_match(case_id, matched, supplied));
    }
    let early = cases
        .iter()
        .find(|case| case["id"] == "early_pdf")
        .expect("early case");
    let late = cases
        .iter()
        .find(|case| case["id"] == "late_pdf")
        .expect("late case");
    assert_eq!(early["fixed_order"]["matched"], 1);
    assert_eq!(late["fixed_order"]["matched"], 0);
    let sum = |variant: &str, field: &str| {
        cases
            .iter()
            .map(|case| case[variant][field].as_u64().expect("score count"))
            .sum::<u64>()
    };
    assert_eq!(sum("direct_only", "required"), 18);
    assert_eq!(sum("direct_only", "matched"), 16);
    assert_eq!(sum("direct_only", "supplied"), 22);
    assert_eq!(sum("legacy_unconditional_adjacent", "matched"), 17);
    let unconditional_supplied = sum("legacy_unconditional_adjacent", "supplied");
    assert_eq!(unconditional_supplied, 49);
    assert_eq!(sum("continuation_aware", "matched"), 17);
    let continuation_supplied = sum("continuation_aware", "supplied");
    assert_eq!(continuation_supplied, 23);
    assert_eq!(sum("small_overlap", "matched"), 16);
    assert_eq!(sum("small_overlap", "supplied"), 22);
    assert_eq!(sum("production_automatic", "matched"), 17);
    assert_eq!(sum("production_automatic", "supplied"), 23);
    assert_eq!(sum("production_automatic", "citation_valid"), 23);
    assert_eq!(sum("production_automatic", "citation_total"), 23);
    assert_eq!(sum("production_automatic", "expansion_count"), 1);
    assert_eq!(sum("production_automatic", "useful_expansions"), 1);
    assert_eq!(
        cases
            .iter()
            .filter(|case| case["production_automatic"]["truncated"] == true)
            .count(),
        0
    );
    assert_eq!(sum("small_overlap", "duplicated_overlap_bytes"), 64);
    assert_eq!(sum("small_overlap", "overlap_fragment_count"), 1);
    assert_eq!(sum("small_overlap", "complete_evidence_in_overlap"), 0);
    let overlap_fragments = cases
        .iter()
        .flat_map(|case| {
            case["small_overlap"]["overlap_fragments"]
                .as_array()
                .expect("materialized overlap fragments")
        })
        .collect::<Vec<_>>();
    assert_eq!(overlap_fragments.len(), 1);
    assert!(overlap_fragments.iter().all(|fragment| {
        fragment["citation_valid"] == true
            && fragment["bytes"].as_u64().expect("fragment bytes")
                == u64::try_from(fragment["text"].as_str().expect("fragment text").len())
                    .expect("bounded fragment")
    }));
    assert_eq!(sum("legacy_unconditional_adjacent", "useful_expansions"), 1);
    assert_eq!(sum("continuation_aware", "useful_expansions"), 1);
    assert_eq!(sum("legacy_unconditional_adjacent", "expansion_count"), 27);
    assert_eq!(sum("continuation_aware", "expansion_count"), 1);
    assert_eq!(sum("fixed_order", "required"), 18);
    assert_eq!(sum("fixed_order", "matched"), 1);
    assert_eq!(sum("empty", "required"), 18);
    assert_eq!(sum("empty", "matched"), 0);
    assert_eq!(
        cases
            .iter()
            .filter(|case| case["direct_only"]["required"] != 0)
            .count(),
        12
    );
    assert_eq!(
        cases
            .iter()
            .filter(|case| case["direct_only"]["required"] != 0
                && case["direct_only"]["full_support"] == true)
            .count(),
        10
    );
    assert_eq!(
        cases
            .iter()
            .filter(|case| {
                case["continuation_aware"]["required"] != 0
                    && case["continuation_aware"]["full_support"] == true
            })
            .count(),
        11
    );
    assert_eq!(
        cases
            .iter()
            .filter(|case| {
                case["production_automatic"]["required"] != 0
                    && case["production_automatic"]["full_support"] == true
            })
            .count(),
        11
    );

    let adjacent = cases
        .iter()
        .find(|case| case["id"] == "multi_passage")
        .expect("adjacent-evidence case");
    assert_eq!(adjacent["direct_only"]["matched"], 1);
    assert_eq!(adjacent["continuation_aware"]["matched"], 2);
    assert_eq!(adjacent["continuation_aware"]["full_support"], true);
    assert_eq!(
        adjacent["direct_hit_evidence"]
            .as_array()
            .expect("hits")
            .len(),
        1
    );
    assert_eq!(adjacent["continuation_aware"]["useful_expansions"], 1);
    assert!(
        ratio(
            17,
            usize::try_from(continuation_supplied).expect("bounded supplied evidence"),
        ) > ratio(
            17,
            usize::try_from(unconditional_supplied).expect("bounded supplied evidence"),
        )
    );
}

fn sampled_rss_bytes() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

fn fixture_locator_shape(format: &str, value: &str) -> bool {
    match format {
        "pdf" => {
            value.starts_with("page=")
                && value.contains(";block=")
                && value.contains(";type=")
                && value.contains(";bbox=")
        }
        "docx" => {
            value.starts_with("heading=")
                && (value.contains(";paragraph=")
                    || (value.contains(";table=")
                        && value.contains(";row=")
                        && value.contains(";headers=")
                        && value.contains(";cells=")))
        }
        "xlsx" => {
            value.starts_with("sheet=") && value.contains(";headers=") && value.contains(";cells=")
        }
        "csv" => value.starts_with("record=") && value.contains(";cells="),
        "pptx" => {
            value.starts_with("slide=")
                && value.contains(";shape-tree=")
                && (value.contains(";shape-id=")
                    || (value.contains(";graphic-id=")
                        && value.contains(";table=")
                        && value.contains(";headers=")
                        && value.contains(";cells=")))
        }
        "txt_md" => value.starts_with("line=") && value.contains('-'),
        _ => false,
    }
}

fn frozen_boundary_locator(format: &str, index: usize) -> &'static str {
    let locators = match format {
        "pdf" => [
            "page=1;block=1;type=title;bbox=247.5,696.0,117.0,18.0",
            "page=2;block=3;type=key_value;bbox=78.0,616.0,230.0,10.0",
            "page=2;block=4;type=table;bbox=108.0,530.0,396.0,56.0",
            "page=3;block=4;type=table;bbox=108.0,530.0,396.0,56.0",
        ],
        "docx" => [
            "heading=Operations/Region 1;paragraph=1",
            "heading=Operations/Region 1;paragraph=3",
            "heading=Operations/Region 1;paragraph=4",
            "heading=Operations/Region 1;table=2;row=2;headers=A1:B1;cells=A2:B2",
        ],
        "xlsx" => [
            "sheet=Rates_1;headers=A1:E1;cells=A2:E2",
            "sheet=Rates_1;headers=A1:E1;cells=A6:E6",
            "sheet=Rates_1;headers=A1:E1;cells=A7:E7",
            "sheet=Rates_1;headers=A1:E1;cells=A11:E11",
        ],
        "csv" => [
            "record=1;cells=A2:D2",
            "record=5;cells=A6:D6",
            "record=6;cells=A7:D7",
            "record=10;cells=A11:D11",
        ],
        "pptx" => [
            "slide=1;shape-tree=1;shape-id=2",
            "slide=2;shape-tree=2;shape-id=3",
            "slide=2;shape-tree=3;shape-id=4",
            "slide=3;shape-tree=4;graphic-id=7;table=1;headers=A1:B1;cells=A2:B2",
        ],
        "txt_md" => ["line=1-3", "line=13-15", "line=16-18", "line=28-30"],
        _ => unreachable!("frozen policy format"),
    };
    locators[index]
}

fn frozen_boundary_passages(format: &'static str) -> Vec<Passage> {
    let file_id = format!("policy-boundary-{format}");
    (0..4)
        .map(|index| Passage {
            key: format!("{file_id}:passage:{index:04}"),
            file_id: file_id.clone(),
            format,
            source_sha256: "1111111111111111111111111111111111111111111111111111111111111111"
                .to_owned(),
            source_revision:
                "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .to_owned(),
            source_revision_id: format!("policy-source-{format}"),
            extraction_set_id: format!("policy-set-{format}"),
            passage_id: format!("policy-passage-{index}"),
            structural_parent: if matches!(index, 1 | 2) {
                format!("policy-{format}-split")
            } else {
                format!("policy-{format}-boundary-{index}")
            },
            passage_order: index + 1,
            continuation_direction: match index {
                1 => "to_next",
                2 => "from_previous",
                _ => "none",
            },
            locator: serde_json::json!({
                "format":format,
                "logical":frozen_boundary_locator(format,index)
            }),
            text: match index {
                1 => format!("Frozen {format} logical section begins."),
                2 => format!("Frozen {format} logical section completes."),
                _ => format!("Frozen {format} boundary {index}."),
            },
            item_id: file_id.clone(),
            revision_id: format!("policy-revision-{format}"),
        })
        .collect()
}

#[test]
fn deterministic_office_corpus_sanity() {
    for (scale, expected) in [(Scale::Thousand, 1_040), (Scale::TenThousand, 10_040)] {
        let corpus = generate(scale);
        assert_eq!(corpus.len(), expected);
        assert_corpus_contract(scale, &corpus);
        let counts = corpus.iter().fold(BTreeMap::new(), |mut counts, passage| {
            counts
                .entry(passage.format)
                .or_insert_with(BTreeSet::new)
                .insert(passage.file_id.as_str());
            counts
        });
        let files = corpus
            .iter()
            .map(|passage| &passage.file_id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(files.len(), FILES_PER_SCALE);
        assert_eq!(
            corpus
                .iter()
                .map(|passage| (
                    &passage.source_sha256,
                    &passage.source_revision,
                    passage.locator.to_string()
                ))
                .collect::<BTreeSet<_>>()
                .len(),
            corpus.len(),
            "every passage citation identity must be unique"
        );
        assert_eq!(format_plan().len(), FILES_PER_SCALE);
        assert!(corpus.iter().all(|passage| passage.text.len() < 2_048));
        assert!(
            corpus
                .iter()
                .any(|passage| passage.text.contains("cached type float"))
        );
        assert!(
            corpus
                .iter()
                .any(|passage| passage.text.contains("Calls\nrenew"))
        );
        assert_eq!(counts["pdf"].len(), 25);
        assert_eq!(counts["docx"].len(), 20);
        assert_eq!(counts["xlsx"].len(), 20);
        assert_eq!(counts["csv"].len(), 15);
        assert_eq!(counts["pptx"].len(), 15);
        assert_eq!(counts["txt_md"].len(), 5);
        for query in queries(scale) {
            assert!(
                query
                    .required
                    .iter()
                    .all(|required| corpus.iter().any(|passage| passage.key == required.key))
            );
        }
        assert_eq!(
            digest(&serde_json::to_vec(&corpus).expect("corpus JSON")),
            digest(&serde_json::to_vec(&generate(scale)).expect("repeat corpus JSON"))
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One deterministic seam covers the four policy boundary matrix.
fn four_policy_directory_is_governed_reciprocal_and_whole_passage_budgeted() {
    let corpus = generate(Scale::Thousand);
    let hit = corpus
        .iter()
        .find(|passage| passage.key == key(Scale::Thousand, "docx", 25, 4))
        .expect("Atlas keyword hit");
    assert_eq!(
        neighbors(hit, &corpus)
            .iter()
            .map(|passage| passage_number(passage))
            .collect::<Vec<_>>(),
        [3, 5]
    );

    let mut wrong_revision = corpus.clone();
    wrong_revision
        .iter_mut()
        .find(|passage| passage.key == key(Scale::Thousand, "docx", 25, 5))
        .expect("next passage")
        .source_revision_id = "stale".to_owned();
    assert_eq!(neighbors(hit, &wrong_revision).len(), 1);

    let mut wrong_file = corpus.clone();
    wrong_file
        .iter_mut()
        .find(|passage| passage.key == key(Scale::Thousand, "docx", 25, 5))
        .expect("next passage")
        .item_id = "another-file".to_owned();
    assert_eq!(neighbors(hit, &wrong_file).len(), 1);
    let mut wrong_set = corpus.clone();
    wrong_set
        .iter_mut()
        .find(|passage| passage.key == key(Scale::Thousand, "docx", 25, 5))
        .expect("next passage")
        .extraction_set_id = "another-set".to_owned();
    assert_eq!(neighbors(hit, &wrong_set).len(), 1);
    let mut wrong_bytes = corpus.clone();
    wrong_bytes
        .iter_mut()
        .find(|passage| passage.key == key(Scale::Thousand, "docx", 25, 5))
        .expect("next passage")
        .source_sha256 = "0".repeat(64);
    assert_eq!(neighbors(hit, &wrong_bytes).len(), 1);

    assert_eq!(
        continuation_neighbors(hit, &corpus)
            .iter()
            .map(|passage| passage_number(passage))
            .collect::<Vec<_>>(),
        [5]
    );
    assert_eq!(
        corpus
            .iter()
            .filter(|passage| passage.continuation_direction != "none")
            .map(|passage| passage.key.as_str())
            .collect::<Vec<_>>(),
        [
            key(Scale::Thousand, "docx", 25, 4),
            key(Scale::Thousand, "docx", 25, 5)
        ]
    );
    for (format, locators) in [
        (
            "pdf",
            [
                "page=1;block=1;type=title;bbox=247.5,696.0,117.0,18.0",
                "page=2;block=3;type=key_value;bbox=78.0,616.0,230.0,10.0",
                "page=2;block=4;type=table;bbox=108.0,530.0,396.0,56.0",
                "page=3;block=4;type=table;bbox=108.0,530.0,396.0,56.0",
            ],
        ),
        (
            "docx",
            [
                "heading=Operations/Region 1;paragraph=1",
                "heading=Operations/Region 1;paragraph=3",
                "heading=Operations/Region 1;paragraph=4",
                "heading=Operations/Region 1;table=2;row=2;headers=A1:B1;cells=A2:B2",
            ],
        ),
        (
            "xlsx",
            [
                "sheet=Rates_1;headers=A1:E1;cells=A2:E2",
                "sheet=Rates_1;headers=A1:E1;cells=A6:E6",
                "sheet=Rates_1;headers=A1:E1;cells=A7:E7",
                "sheet=Rates_1;headers=A1:E1;cells=A11:E11",
            ],
        ),
        (
            "csv",
            [
                "record=1;cells=A2:D2",
                "record=5;cells=A6:D6",
                "record=6;cells=A7:D7",
                "record=10;cells=A11:D11",
            ],
        ),
        (
            "pptx",
            [
                "slide=1;shape-tree=1;shape-id=2",
                "slide=2;shape-tree=2;shape-id=3",
                "slide=2;shape-tree=3;shape-id=4",
                "slide=3;shape-tree=4;graphic-id=7;table=1;headers=A1:B1;cells=A2:B2",
            ],
        ),
        (
            "txt_md",
            ["line=1-3", "line=13-15", "line=16-18", "line=28-30"],
        ),
    ] {
        let passages = frozen_boundary_passages(format);
        for (passage, expected_locator) in passages.iter().zip(locators) {
            assert_eq!(
                passage.locator,
                serde_json::json!({"format":format,"logical":expected_locator})
            );
        }
        assert!(continuation_neighbors(&passages[0], &passages).is_empty());
        assert!(continuation_neighbors(&passages[3], &passages).is_empty());
        let mut malformed_first = passages.clone();
        malformed_first[0].continuation_direction = "from_previous";
        assert!(continuation_neighbors(&malformed_first[0], &malformed_first).is_empty());
        let mut malformed_last = passages.clone();
        malformed_last[3].continuation_direction = "to_next";
        assert!(continuation_neighbors(&malformed_last[3], &malformed_last).is_empty());
        assert_eq!(
            continuation_neighbors(&passages[1], &passages)[0].passage_order,
            3
        );
        assert_eq!(
            continuation_neighbors(&passages[2], &passages)[0].passage_order,
            2
        );
        assert!(!continues_to_previous(&passages[1]));
        assert!(!continues_to_next(&passages[2]));
        let mut crossed = passages.clone();
        crossed[2].structural_parent.push_str("-other");
        assert!(continuation_neighbors(&crossed[1], &crossed).is_empty());
        assert!(continuation_neighbors(&crossed[2], &crossed).is_empty());
        let mut one_sided = passages;
        one_sided[2].continuation_direction = "none";
        assert!(continuation_neighbors(&one_sided[1], &one_sided).is_empty());
        assert!(continuation_neighbors(&one_sided[2], &one_sided).is_empty());
    }
    let mut broken_reciprocal = corpus.clone();
    broken_reciprocal
        .iter_mut()
        .find(|passage| passage.key == key(Scale::Thousand, "docx", 25, 5))
        .expect("next passage")
        .continuation_direction = "none";
    assert!(continuation_neighbors(hit, &broken_reciprocal).is_empty());
    let duplicate_direct = pack_policy(
        RetrievalPolicy::ContinuationAware,
        &[hit, hit],
        &corpus,
        &[],
    );
    assert_eq!(duplicate_direct.supplied.len(), 2);
    assert_eq!(
        duplicate_direct
            .supplied
            .iter()
            .map(|passage| passage.key.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        2
    );
    let mut duplicate_alias = hit.clone();
    duplicate_alias.key.push_str("-alias");
    let alias_deduplicated = pack_policy(
        RetrievalPolicy::DirectOnly,
        &[hit, &duplicate_alias],
        &corpus,
        &[],
    );
    assert_eq!(alias_deduplicated.supplied.len(), 1);
    let mut oversized = hit.clone();
    oversized.key.push_str("-oversized");
    oversized.passage_id.push_str("-oversized");
    oversized.text = "x".repeat(CONTEXT_BUDGET + 1);
    let mut fitting = hit.clone();
    fitting.key.push_str("-fitting");
    fitting.passage_id.push_str("-fitting");
    fitting.text = "fits".to_owned();
    let skip_and_continue = pack_policy(
        RetrievalPolicy::DirectOnly,
        &[&oversized, &fitting],
        &[],
        &[],
    );
    assert_eq!(skip_and_continue.supplied[0].text, "fits");
    assert_eq!(skip_and_continue.budget_rejected, 1);
    let five_same_source = (0..5)
        .map(|index| {
            let mut passage = fitting.clone();
            passage.key = format!("source-cap-{index}");
            passage.passage_id = format!("source-cap-{index}");
            passage.locator = serde_json::json!({
                "format":"docx",
                "logical":format!("source-cap={index}")
            });
            passage
        })
        .collect::<Vec<_>>();
    let five_same_source = five_same_source.iter().collect::<Vec<_>>();
    let source_capped = pack_policy(RetrievalPolicy::DirectOnly, &five_same_source, &[], &[]);
    assert_eq!(source_capped.supplied.len(), 4);

    let mut context = String::new();
    assert!(append_whole(&mut context, "direct", 10));
    assert!(!append_whole(&mut context, "neighbor", 10));
    assert_eq!(context, "direct");
}

#[test]
fn standalone_large_oracle_rejects_identity_and_evidence_mutations() {
    for &(format, file) in &LARGE_FORMATS {
        let actual = generate_file(Scale::TenThousand, file)
            .pop()
            .expect("large-file late passage");
        let oracle = large_gold(format);
        assert!(evidence_matches(&oracle, &actual));
        let citation_manifest = vec![actual.clone()];
        assert_eq!(
            score(&[oracle], &[&actual], actual.text.len(), &citation_manifest).matched,
            1
        );
    }

    for (format, file, original, replacement) in [
        ("docx", 25, "Friday", "Thursday"),
        ("xlsx", 45, "0.14", "0.15"),
    ] {
        let mut changed = generate_file(Scale::TenThousand, file)
            .pop()
            .expect("large-file late passage");
        changed.text = changed.text.replace(original, replacement);
        assert_eq!(
            score(&[large_gold(format)], &[&changed], changed.text.len(), &[]).matched,
            0
        );
    }
    let mut wrong_locator = generate_file(Scale::TenThousand, 65)
        .pop()
        .expect("CSV late passage");
    wrong_locator.locator["wrong"] = Value::Bool(true);
    assert_eq!(
        score(
            &[large_gold("csv")],
            &[&wrong_locator],
            wrong_locator.text.len(),
            &[]
        )
        .matched,
        0
    );
    let mut wrong_source = generate_file(Scale::TenThousand, 80)
        .pop()
        .expect("PPTX late passage");
    wrong_source.source_sha256.replace_range(..1, "0");
    wrong_source.source_revision.replace_range(..1, "0");
    wrong_source.source_revision_id.replace_range(..1, "0");
    assert_eq!(
        score(
            &[large_gold("pptx")],
            &[&wrong_source],
            wrong_source.text.len(),
            &[]
        )
        .matched,
        0
    );
}

#[test]
fn frozen_oracle_rejects_evidence_mutations_and_context_clipping_is_real() {
    let expected = gold(Scale::Thousand, "docx", 25, 3);
    let actual = generate(Scale::Thousand)
        .into_iter()
        .find(|passage| passage.key == expected.key)
        .expect("gold passage");
    assert!(evidence_matches(&expected, &actual));
    let citation_manifest = vec![actual.clone()];
    let valid = score(
        std::slice::from_ref(&expected),
        &[&actual],
        actual.text.len(),
        &citation_manifest,
    );
    assert_eq!(valid.citation_valid, 1);
    let mut wrong_text = actual.clone();
    wrong_text.text.push_str(" corrupted");
    assert!(!evidence_matches(&expected, &wrong_text));
    assert_eq!(
        score(
            std::slice::from_ref(&expected),
            &[&wrong_text],
            wrong_text.text.len(),
            &citation_manifest
        )
        .matched,
        0
    );
    assert!(!keyword_counts_match("early_pdf", 1, 2));
    assert_eq!(
        queries(Scale::Thousand)
            .into_iter()
            .find(|query| query.id == "repeated_fact")
            .expect("repeated case")
            .answer,
        Some("Four current sources say the shared cadence is quarterly.")
    );
    let mut wrong_locator = actual.clone();
    wrong_locator.locator["wrong"] = Value::Bool(true);
    assert!(!evidence_matches(&expected, &wrong_locator));
    let mut wrong_source = actual.clone();
    wrong_source.source_sha256.replace_range(..1, "0");
    assert!(!evidence_matches(&expected, &wrong_source));
    assert_eq!(
        score(
            std::slice::from_ref(&expected),
            &[&wrong_source],
            wrong_source.text.len(),
            &citation_manifest
        )
        .matched,
        0
    );
    let mut wrong_revision = actual.clone();
    wrong_revision.revision_id.push_str("-wrong");
    let mut wrong_source_revision = actual.clone();
    wrong_source_revision.source_revision_id.push_str("-wrong");
    let mut wrong_source_revision_name = actual.clone();
    wrong_source_revision_name
        .source_revision
        .push_str("-wrong");
    let mut wrong_set = actual.clone();
    wrong_set.extraction_set_id.push_str("-wrong");
    let mut wrong_passage = actual.clone();
    wrong_passage.passage_id.push_str("-wrong");
    for changed in [
        &wrong_locator,
        &wrong_source,
        &wrong_revision,
        &wrong_source_revision,
        &wrong_source_revision_name,
        &wrong_set,
        &wrong_passage,
    ] {
        assert!(!citation_matches_manifest(changed, &citation_manifest));
        let changed_score = score(
            std::slice::from_ref(&expected),
            &[changed],
            changed.text.len(),
            &citation_manifest,
        );
        assert_eq!(changed_score.citation_valid, 0);
    }
    let corpus = generate(Scale::Thousand);
    let (context, supplied) = fixed_context(&corpus, 4096);
    assert_eq!(context.len(), 4096);
    assert!(context.is_char_boundary(context.len()));
    assert!(
        supplied
            .iter()
            .all(|passage| context.contains(&passage.text))
    );
}

#[test]
fn per_format_citation_summary_rejects_corrupt_locator() {
    let actual = generate_file(Scale::Thousand, 25)
        .into_iter()
        .next()
        .expect("DOCX passage");
    let citation_manifest = vec![actual.clone()];
    let format_case = |passage: &Passage| {
        serde_json::json!({
            "required_evidence":[],
            "direct_hit_evidence":[passage],
        })
    };
    let valid = by_format(
        &[format_case(&actual)],
        "direct_hit_evidence",
        &citation_manifest,
    );
    assert_eq!(valid["docx"]["citation_valid"], 1);
    assert_eq!(valid["docx"]["citation_total"], 1);
    let mut wrong_locator = actual;
    wrong_locator.locator["wrong"] = Value::Bool(true);
    let corrupt = by_format(
        &[format_case(&wrong_locator)],
        "direct_hit_evidence",
        &citation_manifest,
    );
    assert_eq!(corrupt["docx"]["citation_valid"], 0);
    assert_eq!(corrupt["docx"]["citation_total"], 1);
}

#[test]
fn benchmark_locator_templates_match_committed_fixture_grammars() {
    assert_eq!(
        locator("pdf", 0),
        "page=1;block=1;type=title;bbox=247.5,696.0,117.0,18.0"
    );
    assert_eq!(
        locator("docx", 4),
        "heading=Operations/Region 1;table=1;row=2;headers=A1:B1;cells=A2:B2"
    );
    assert_eq!(
        locator("xlsx", 0),
        "sheet=Rates_1;headers=A1:E1;cells=A2:E2"
    );
    assert_eq!(locator("csv", 0), "record=1;cells=A2:D2");
    assert_eq!(
        locator("pptx", 3),
        "slide=1;shape-tree=4;graphic-id=6;table=1;headers=A1:B1;cells=A2:B2"
    );
    assert_eq!(locator("txt_md", 0), "line=1-3");
    // Frozen examples are copied from Group 1/2 real-format fixture assertions.
    for (format, fixture) in [
        (
            "pdf",
            "page=1;block=1;type=title;bbox=247.5,696.0,117.0,18.0",
        ),
        (
            "docx",
            "heading=Client renewal handbook/Escalation policy;table=1;row=2;headers=A1:B1;cells=A2:B2",
        ),
        ("xlsx", "sheet=Rates;headers=A1:D1;cells=A2:D2"),
        ("csv", "record=1;cells=A2:C2"),
        (
            "pptx",
            "slide=2;shape-tree=3;graphic-id=6;table=1;headers=A1:B1;cells=A2:B2",
        ),
        ("txt_md", "line=1-3"),
    ] {
        assert!(fixture_locator_shape(format, fixture));
        assert!(
            generate(Scale::Thousand)
                .iter()
                .filter(|passage| passage.format == format)
                .all(|passage| fixture_locator_shape(
                    format,
                    passage.locator["logical"]
                        .as_str()
                        .expect("logical locator")
                ))
        );
        assert!(!fixture_locator_shape(format, "flattened=unknown"));
    }
}

fn policy_json(
    required: &[ExpectedEvidence],
    result: &PolicyResult<'_>,
    citation_manifest: &[Passage],
) -> Value {
    let score = score(
        required,
        &result.supplied,
        result.context_bytes,
        citation_manifest,
    );
    let overlap_fragments = result
        .overlap_fragments
        .iter()
        .map(|fragment| {
            serde_json::json!({
                "item_id":fragment.source.item_id,
                "revision_id":fragment.source.revision_id,
                "source_sha256":fragment.source.source_sha256,
                "source_revision":fragment.source.source_revision,
                "source_revision_id":fragment.source.source_revision_id,
                "extraction_set_id":fragment.source.extraction_set_id,
                "passage_id":fragment.source.passage_id,
                "locator":fragment.source.locator,
                "text":fragment.text,
                "bytes":fragment.text.len(),
                "citation_valid":citation_matches_manifest(fragment.source,citation_manifest),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "required":score.required,
        "supplied":score.supplied,
        "matched":score.matched,
        "full_support":score.full_support,
        "recall":score.recall,
        "evidence_precision":score.evidence_precision,
        "citation_valid":score.citation_valid,
        "citation_total":score.citation_total,
        "citation_validity":score.citation_validity,
        "context_bytes":score.context_bytes,
        "expansion_count":result.expansion_count,
        "useful_expansions":result.useful_expansions,
        "useful_neighbor_yield":ratio(result.useful_expansions,result.expansion_count),
        "budget_rejected":result.budget_rejected,
        "duplicated_overlap_bytes":result.duplicated_overlap_bytes,
        "overlap_fragment_count":result.overlap_fragments.len(),
        "overlap_fragments":overlap_fragments,
        "complete_evidence_in_overlap":result.complete_evidence_in_overlap,
        "supplied_evidence":result.supplied,
    })
}

fn decode_sha256(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).expect("hex source SHA"))
        .collect()
}

async fn activate_office_file(pool: &PgPool, passages: &[&Passage]) {
    activate_office_file_in_collection(
        pool,
        ALPHA_TENANT,
        "30000000000000000000000000000001",
        passages,
    )
    .await;
}

async fn activate_office_file_in_collection(
    pool: &PgPool,
    tenant_id: &str,
    collection_id: &str,
    passages: &[&Passage],
) {
    let first = passages.first().expect("nonempty logical file");
    assert!(passages.iter().enumerate().all(|(index, passage)| {
        same_extraction(first, passage)
            && first.source_sha256 == passage.source_sha256
            && passage.passage_order == index + 1
    }));
    sqlx::query("INSERT INTO items (tenant_id,id,collection_id) VALUES ($1,$2,$3)")
        .bind(tenant_id)
        .bind(&first.item_id)
        .bind(collection_id)
        .execute(pool)
        .await
        .expect("seed office document item");
    sqlx::query("INSERT INTO revisions (tenant_id,item_id,id,content) VALUES ($1,$2,$3,$4)")
        .bind(tenant_id)
        .bind(&first.item_id)
        .bind(&first.revision_id)
        .bind(format!("Synthetic {} office document", first.format))
        .execute(pool)
        .await
        .expect("seed office document revision");
    sqlx::query("UPDATE items SET active_revision_id=$3 WHERE tenant_id=$1 AND id=$2")
        .bind(tenant_id)
        .bind(&first.item_id)
        .bind(&first.revision_id)
        .execute(pool)
        .await
        .expect("activate office document revision");
    activate_office_extraction(pool, tenant_id, passages).await;
}

async fn activate_office_extraction(pool: &PgPool, tenant_id: &str, passages: &[&Passage]) {
    let mut connection = pool
        .acquire()
        .await
        .expect("acquire office extraction connection");
    activate_office_extraction_on_connection(&mut connection, tenant_id, passages).await;
}

async fn activate_office_extraction_on_connection(
    connection: &mut sqlx::PgConnection,
    tenant_id: &str,
    passages: &[&Passage],
) {
    let first = passages.first().expect("nonempty logical file");
    assert!(passages.iter().enumerate().all(|(index, passage)| {
        same_extraction(first, passage)
            && first.source_sha256 == passage.source_sha256
            && passage.passage_order == index + 1
    }));
    let passage_ids = passages
        .iter()
        .map(|passage| passage.passage_id.clone())
        .collect::<Vec<_>>();
    let parents = passages
        .iter()
        .map(|passage| passage.structural_parent.clone())
        .collect::<Vec<_>>();
    let directions = passages
        .iter()
        .map(|passage| passage.continuation_direction.to_owned())
        .collect::<Vec<_>>();
    let locators = passages
        .iter()
        .map(|passage| passage.locator.to_string())
        .collect::<Vec<_>>();
    let contents = passages
        .iter()
        .map(|passage| passage.text.clone())
        .collect::<Vec<_>>();
    let activated = sqlx::query_scalar::<_, String>(
        "SELECT activate_document_extraction(
           $1,$2,$3,$4,$5,$6,'synthetic-office','1','policy-v1',$7,$8,$9,$10,$11)",
    )
    .bind(tenant_id)
    .bind(&first.item_id)
    .bind(&first.revision_id)
    .bind(&first.source_revision_id)
    .bind(decode_sha256(&first.source_sha256))
    .bind(&first.extraction_set_id)
    .bind(passage_ids)
    .bind(parents)
    .bind(directions)
    .bind(locators)
    .bind(contents)
    .fetch_one(connection)
    .await
    .expect("activate office extraction set");
    assert_eq!(activated, first.extraction_set_id);
}

struct ValidatedSearch<'a> {
    returned: Vec<&'a Passage>,
    direct: Vec<&'a Passage>,
    expansions: Vec<&'a Passage>,
    context_bytes: usize,
    truncated: bool,
}

fn validated_search<'a>(response: &Value, manifest: &'a [Passage]) -> ValidatedSearch<'a> {
    validated_search_with_semantic(response, manifest, false)
}

fn validated_search_with_semantic<'a>(
    response: &Value,
    manifest: &'a [Passage],
    allow_semantic: bool,
) -> ValidatedSearch<'a> {
    let hits = response["items"].as_array().expect("search items");
    let mut saw_expansion = false;
    let validated = hits
        .iter()
        .map(|hit| {
            let reason = hit["reason"].as_str().expect("search reason");
            assert!(
                matches!(reason, "lexical" | "adjacent_continuation")
                    || allow_semantic && reason == "semantic"
            );
            if reason == "adjacent_continuation" {
                saw_expansion = true;
            } else {
                assert!(!saw_expansion, "direct hits must precede expansions");
            }
            let citation = hit["citation"].as_object().expect("passage citation");
            let passage = manifest
                .iter()
                .find(|passage| {
                    hit["item_id"] == passage.item_id
                        && hit["revision_id"] == passage.revision_id
                        && citation["source_revision_id"] == passage.source_revision_id
                        && citation["extraction_set_id"] == passage.extraction_set_id
                        && citation["passage_id"] == passage.passage_id
                })
                .expect("exact governed manifest identity");
            assert_eq!(hit["excerpt"], passage.text);
            assert_eq!(citation["locator"], passage.locator);
            (reason, passage)
        })
        .collect::<Vec<_>>();
    let context_bytes = usize::try_from(
        response["context_bytes"]
            .as_u64()
            .expect("search context bytes"),
    )
    .expect("bounded search context bytes");
    assert_eq!(
        context_bytes,
        validated
            .iter()
            .map(|(_, passage)| passage.text.len())
            .sum::<usize>()
    );
    let direct = validated
        .iter()
        .filter_map(|(reason, passage)| {
            (*reason == "lexical" || *reason == "semantic").then_some(*passage)
        })
        .collect::<Vec<_>>();
    let expected_expansions = validated
        .iter()
        .filter_map(|(reason, passage)| (*reason == "lexical").then_some(*passage))
        .flat_map(|passage| continuation_neighbors(passage, manifest))
        .map(source_span_identity)
        .collect::<BTreeSet<_>>();
    for (_, expansion) in validated
        .iter()
        .filter(|(reason, _)| *reason == "adjacent_continuation")
    {
        assert!(expected_expansions.contains(&source_span_identity(expansion)));
    }
    ValidatedSearch {
        returned: validated.iter().map(|(_, passage)| *passage).collect(),
        direct,
        expansions: validated
            .iter()
            .filter_map(|(reason, passage)| {
                (*reason == "adjacent_continuation").then_some(*passage)
            })
            .collect(),
        context_bytes,
        truncated: response["truncated"].as_bool().expect("search truncated"),
    }
}

fn production_automatic_json(
    required: &[ExpectedEvidence],
    search: &ValidatedSearch<'_>,
    citation_manifest: &[Passage],
) -> Value {
    let result = score(
        required,
        &search.returned,
        search.context_bytes,
        citation_manifest,
    );
    let useful_expansions = search
        .expansions
        .iter()
        .filter(|passage| {
            required
                .iter()
                .any(|expected| evidence_matches(expected, passage))
        })
        .count();
    serde_json::json!({
        "required":result.required,
        "supplied":result.supplied,
        "matched":result.matched,
        "full_support":result.full_support,
        "recall":result.recall,
        "evidence_precision":result.evidence_precision,
        "citation_valid":result.citation_valid,
        "citation_total":result.citation_total,
        "citation_validity":result.citation_validity,
        "context_bytes":result.context_bytes,
        "expansion_count":search.expansions.len(),
        "useful_expansions":useful_expansions,
        "useful_expansion_yield":ratio(useful_expansions,search.expansions.len()),
        "truncated":search.truncated,
    })
}

#[allow(clippy::too_many_lines)] // The sequential benchmark keeps timings and their public operations together.
async fn run_scale(scale: Scale) -> Value {
    std::fs::create_dir_all("target").expect("benchmark output directory");
    let report_path = format!("target/office-retrieval-benchmark-{}.json", scale.name());
    std::fs::write(&report_path, br#"{"status":"running_or_interrupted"}"#)
        .expect("invalidate prior benchmark report");
    let rss_before = sampled_rss_bytes();
    let generation_start = Instant::now();
    let corpus = generate(scale);
    let generation_ms = generation_start.elapsed().as_secs_f64() * 1_000.0;
    assert_corpus_contract(scale, &corpus);
    let deterministic_manifest = serde_json::json!({
        "scale":scale.name(),
        "format_file_distribution":{"pdf":25,"docx":20,"xlsx":20,"csv":15,"pptx":15,"txt_markdown":5},
        "passages":corpus,
    });
    let manifest_bytes =
        serde_json::to_vec_pretty(&deterministic_manifest).expect("corpus manifest");
    let corpus_hash = digest(&manifest_bytes);
    std::fs::create_dir_all("target").expect("benchmark output directory");
    let manifest_path = format!("target/office-corpus-manifest-{}.json", scale.name());
    std::fs::write(&manifest_path, &manifest_bytes).expect("write corpus manifest");
    let query_set = queries(scale);
    let query_hash = digest(
        &serde_json::to_vec(
            &query_set
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
                .collect::<Vec<_>>(),
        )
        .expect("query manifest"),
    );
    let (migrator, runtime, worker, reader, bob, _writer) = setup().await;
    let mut ingest_times = Vec::with_capacity(FILES_PER_SCALE);
    let files = corpus.iter().fold(BTreeMap::new(), |mut files, passage| {
        files
            .entry(passage.file_id.as_str())
            .or_insert_with(Vec::new)
            .push(passage);
        files
    });
    for passages in files.values() {
        let start = Instant::now();
        activate_office_file(&migrator, passages).await;
        ingest_times.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    runtime.close().await;

    let fresh_runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(RUNTIME_URL)
        .await
        .expect("fresh benchmark reader pool");
    let app = router(fresh_runtime.clone());
    let (fixed_context_text, fixed) = fixed_context(&corpus, CONTEXT_BUDGET);
    let mut search_times = Vec::new();
    let mut cases = Vec::new();
    for query in &query_set {
        let start = Instant::now();
        let (status, response) = search(
            &app,
            Some(&reader),
            serde_json::json!({"query":query.text,"max_context_bytes":CONTEXT_BUDGET})
                .to_string()
                .as_bytes(),
        )
        .await;
        let case_search_ms = start.elapsed().as_secs_f64() * 1_000.0;
        search_times.push(case_search_ms);
        assert_eq!(status, StatusCode::OK, "search {}", query.id);
        let production = validated_search(&response, &corpus);
        let direct_hits = production.direct.clone();
        let direct = pack_policy(
            RetrievalPolicy::DirectOnly,
            &direct_hits,
            &corpus,
            &query.required,
        );
        let unconditional = pack_policy(
            RetrievalPolicy::UnconditionalAdjacent,
            &direct_hits,
            &corpus,
            &query.required,
        );
        let continuation = pack_policy(
            RetrievalPolicy::ContinuationAware,
            &direct_hits,
            &corpus,
            &query.required,
        );
        let overlap = pack_policy(
            RetrievalPolicy::SmallOverlap,
            &direct_hits,
            &corpus,
            &query.required,
        );
        assert_eq!(
            production
                .returned
                .iter()
                .map(|passage| (source_span_identity(passage), passage.passage_id.as_str()))
                .collect::<Vec<_>>(),
            continuation
                .supplied
                .iter()
                .map(|passage| (source_span_identity(passage), passage.passage_id.as_str()))
                .collect::<Vec<_>>(),
            "native automatic expansion must match the frozen continuation selection for {}",
            query.id
        );
        cases.push(serde_json::json!({
            "id":query.id,"query":query.text,"category":query.category,"expected_answer":query.answer,
            "required_evidence":query.required,
            "empty":score(&query.required,&[],0,&corpus),
            "fixed_order":score(&query.required,&fixed,fixed_context_text.len(),&corpus),
            "direct_only":policy_json(&query.required,&direct,&corpus),
            "legacy_unconditional_adjacent":policy_json(&query.required,&unconditional,&corpus),
            "continuation_aware":policy_json(&query.required,&continuation,&corpus),
            "small_overlap":policy_json(&query.required,&overlap,&corpus),
            "production_automatic":production_automatic_json(&query.required,&production,&corpus),
            "direct_hit_evidence":direct_hits,
            "continuation_aware_evidence":continuation.supplied,
            "production_automatic_evidence":production.returned,
            "production_automatic_expansion_evidence":production.expansions,
            "latency_ms":{"end_to_end_public_search_with_automatic_expansion":case_search_ms},
            "search_truncated":response["truncated"]
        }));
    }
    let (bob_status, bob_response) = search_json(&app, Some(&bob), "amber lantern").await;
    assert_eq!(bob_status, StatusCode::OK);
    assert!(
        bob_response["items"]
            .as_array()
            .expect("Bob search items")
            .is_empty(),
        "another private reader must not see Alice's corpus"
    );
    assert_case_gates(&cases, &fixed_context_text);
    let atlas_hit = corpus
        .iter()
        .find(|passage| passage.key == key(scale, "docx", 25, 4))
        .expect("Atlas direct hit");
    let atlas_neighbor = corpus
        .iter()
        .find(|passage| passage.key == key(scale, "docx", 25, 5))
        .expect("Atlas adjacent evidence");
    let mut wrong_revision_manifest = vec![atlas_neighbor.clone()];
    "stale".clone_into(&mut wrong_revision_manifest[0].source_revision_id);
    let cross_revision_candidates = neighbors(atlas_hit, &wrong_revision_manifest).len();
    let mut wrong_file_manifest = vec![atlas_neighbor.clone()];
    "another-file".clone_into(&mut wrong_file_manifest[0].item_id);
    let cross_file_candidates = neighbors(atlas_hit, &wrong_file_manifest).len();
    assert_eq!(cross_revision_candidates, 0);
    assert_eq!(cross_file_candidates, 0);
    let rss_after = sampled_rss_bytes();
    let rss_max_sample = rss_before.into_iter().chain(rss_after).max();
    let direct_only = aggregate(&cases, "direct_only");
    let continuation_aware = aggregate(&cases, "continuation_aware");
    let production_automatic = aggregate_production(&cases);
    let direct_only_by_format = by_format(&cases, "direct_hit_evidence", &corpus);
    let continuation_aware_by_format = by_format(&cases, "continuation_aware_evidence", &corpus);
    let production_automatic_by_format =
        by_format(&cases, "production_automatic_evidence", &corpus);
    assert_format_citation_reconciliation(&direct_only, &direct_only_by_format);
    assert_format_citation_reconciliation(&continuation_aware, &continuation_aware_by_format);
    assert_format_citation_reconciliation(&production_automatic, &production_automatic_by_format);
    let quality = serde_json::json!({
        "empty":aggregate(&cases,"empty"),
        "fixed_order":aggregate(&cases,"fixed_order"),
        "direct_only":direct_only,
        "legacy_unconditional_adjacent":aggregate(&cases,"legacy_unconditional_adjacent"),
        "continuation_aware":continuation_aware,
        "small_overlap":aggregate(&cases,"small_overlap"),
        "production_automatic":production_automatic,
        "direct_only_by_format":direct_only_by_format,
        "continuation_aware_by_format":continuation_aware_by_format,
        "production_automatic_by_format":production_automatic_by_format,
        "no_answer_exclusions":{"cases":cases.iter().filter(|case|case["category"]=="no_answer_exclusion").count(),"empty_direct_context":cases.iter().filter(|case|case["category"]=="no_answer_exclusion" && case["direct_only"]["supplied"]==0).count()},
    });
    let report = serde_json::json!({
        "status":"passed",
        "scale":scale.name(),
        "manifest":{"files":FILES_PER_SCALE,"passages":corpus.len(),"corpus_sha256":corpus_hash,"queries_sha256":query_hash,"harness_sha256":digest(include_bytes!("office_retrieval.rs")),"git_head":tool_version("git", &["rev-parse","HEAD"]),"rustc":tool_version("rustc", &["--version"]),"profile":if cfg!(debug_assertions){"debug"}else{"release"},"synthetic_only":true,"hosted_ai":false,"context_budget_bytes":CONTEXT_BUDGET},
        "format_file_distribution":{"pdf":25,"docx":20,"xlsx":20,"csv":15,"pptx":15,"txt_markdown":5},
        "deterministic_corpus_manifest":{"path":manifest_path,"sha256":corpus_hash},
        "activated_corpus":corpus,
        "generation":{"method":"runtime parser-validated structural templates; does not reparse 100 distinct office binaries","parsing_ms":null,"parsing_note":"No binary parsing occurs in scale generation; committed real-format fixtures separately prove extraction fidelity.","chunking_ms":generation_ms,"normalized_bytes":corpus.iter().map(|passage|passage.text.len()).sum::<usize>()},
        "quality":quality,
        "fixed_order_context":fixed_context_text,
        "private_reader_isolation":{"alice_positive":cases[0]["direct_only"]["matched"]==1,"bob_same_query_items":bob_response["items"].as_array().expect("Bob items").len()},
        "policy_selection":{"selected":"continuation_aware","rationale":"reciprocal structural continuation recovers the useful split with fewer irrelevant adjacent passages than unconditional expansion","comparison_is_test_only":true,"production_automatic_is_separately_scored":true,"current_public_search_behavior":"automatic validated reciprocal adjacent_continuation after lexical results; no public knob or N+1 read"},
        "declared_continuation_edges":[{"item_id":atlas_hit.item_id,"from_passage_id":atlas_hit.passage_id,"to_passage_id":atlas_neighbor.passage_id,"reason":"The independently frozen Atlas answer is deliberately split between consecutive authored sections."}],
        "neighbor_controls":{"same_item_revision_source_and_extraction_set_only":true,"cross_file_candidates_included":cross_file_candidates,"cross_revision_candidates_included":cross_revision_candidates},
        "latency_ms":{"activation":{"samples":ingest_times.len(),"sample_note":"one trusted activation per logical file; not a distribution of repeated identical activations","p50":percentile(&ingest_times,50),"p95":percentile(&ingest_times,95),"raw":ingest_times},"first_search_fresh_pool":search_times.first(),"end_to_end_public_search_with_automatic_expansion":{"samples":search_times.len(),"scope":"HTTP/router + database search + Rust packing","p50":percentile(&search_times,50),"p95":percentile(&search_times,95),"raw":search_times}},
        "rss":{"sampled_process_rss_bytes":rss_max_sample,"note":"Maximum of before/after benchmark-process RSS samples; measurement only, not a true peak. The real-fixture parser child separately has fail-closed sampled RSS supervision, not a native hard ceiling."},
        "failures":[],
        "cases":cases,
        "limits":"Four-policy comparison is test-only over validated lexical public hits plus the frozen manifest. The separately scored production_automatic section records the actual ordered public response, whose context_bytes count content bytes without test-policy separators. Current public search automatically returns validated reciprocal adjacent_continuation after direct hits in the same bounded operation, without a public knob or N+1 read. No answer/judge model or answer-quality claim. Formula caches remain evidence, never recalculated truth."
    });
    std::fs::write(
        report_path,
        serde_json::to_vec_pretty(&report).expect("report JSON"),
    )
    .expect("write report");
    fresh_runtime.close().await;
    worker.close().await;
    migrator.close().await;
    report
}

#[allow(clippy::too_many_lines)] // Timings remain adjacent to the public operation they measure.
async fn run_large_files() -> Value {
    std::fs::create_dir_all("target").expect("benchmark output directory");
    let report_path = "target/office-large-files-benchmark.json";
    std::fs::write(report_path, br#"{"status":"running_or_interrupted"}"#)
        .expect("invalidate prior large-file report");
    let rss_before = sampled_rss_bytes();
    let mut generation_ms = BTreeMap::new();
    let mut corpus = Vec::new();
    for &(format, file) in &LARGE_FORMATS {
        let start = Instant::now();
        let passages = generate_file(Scale::TenThousand, file);
        generation_ms.insert(format, start.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(passages.len(), 108);
        assert!(passages.iter().all(|passage| {
            passage.format == format
                && fixture_locator_shape(
                    format,
                    passage.locator["logical"]
                        .as_str()
                        .expect("logical locator"),
                )
                && passage.source_revision == format!("sha256:{}", passage.source_sha256)
                && passage.source_revision_id == passage.source_sha256
        }));
        corpus.extend(passages);
    }
    let deterministic_manifest = serde_json::json!({
        "mode":"standalone_large_logical_files",
        "scale_shape":"10k enlarged-file shape",
        "files":LARGE_FORMATS.iter().map(|(format,file)|serde_json::json!({"format":format,"file_index":file})).collect::<Vec<_>>(),
        "passages":corpus,
    });
    let manifest_bytes =
        serde_json::to_vec_pretty(&deterministic_manifest).expect("large-file manifest");
    let manifest_sha256 = digest(&manifest_bytes);
    let manifest_path = "target/office-large-files-manifest.json";
    std::fs::write(manifest_path, &manifest_bytes).expect("write large-file manifest");

    let (migrator, runtime, worker, reader, _bob, _writer) = setup().await;
    let mut ingest_ms: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for &(format, _) in &LARGE_FORMATS {
        let passages = corpus
            .iter()
            .filter(|passage| passage.format == format)
            .collect::<Vec<_>>();
        let start = Instant::now();
        activate_office_file(&migrator, &passages).await;
        ingest_ms
            .entry(format)
            .or_default()
            .push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    runtime.close().await;
    let fresh_runtime = PgPoolOptions::new()
        .max_connections(3)
        .connect(RUNTIME_URL)
        .await
        .expect("fresh large-file reader pool");
    let app = router(fresh_runtime.clone());
    let query_for = |format| match format {
        "pdf" => "obsidian kestrel",
        "docx" => "saffron lynx",
        "xlsx" => "indigo wren",
        "csv" => "copper auk",
        "pptx" => "umber seal",
        _ => unreachable!("large format"),
    };
    let mut formats = serde_json::Map::new();
    let mut all_search_ms = Vec::new();
    for &(format, _) in &LARGE_FORMATS {
        let mut first_query_before_timed_samples_ms = None;
        let mut search_ms = Vec::new();
        let mut passage = None;
        let mut production_context_bytes = None;
        for sample in 0..=10 {
            let start = Instant::now();
            let (status, search_response) = search(
                &app,
                Some(&reader),
                serde_json::json!({"query":query_for(format),"max_context_bytes":CONTEXT_BUDGET})
                    .to_string()
                    .as_bytes(),
            )
            .await;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1_000.0;
            assert_eq!(status, StatusCode::OK);
            let production = validated_search(&search_response, &corpus);
            assert_eq!(production.returned.len(), 1, "one late marker for {format}");
            assert_eq!(
                production.direct.len(),
                1,
                "one direct late marker for {format}"
            );
            assert!(production.expansions.is_empty());
            assert!(!production.truncated);
            assert_eq!(production.returned[0].format, format);
            assert_eq!(passage_number(production.returned[0]), 107);
            passage = Some(production.returned[0]);
            production_context_bytes = Some(production.context_bytes);
            if sample == 0 {
                first_query_before_timed_samples_ms = Some(elapsed_ms);
            } else {
                search_ms.push(elapsed_ms);
                all_search_ms.push(elapsed_ms);
            }
        }
        let passage = passage.expect("validated late passage");
        assert_eq!(passage.format, format);
        assert_eq!(passage_number(passage), 107);
        let production_context_bytes = production_context_bytes.expect("validated context bytes");
        let file_passages: Vec<_> = corpus
            .iter()
            .filter(|candidate| candidate.format == format)
            .collect();
        let normalized_bytes = file_passages
            .iter()
            .map(|candidate| candidate.text.len())
            .sum::<usize>();
        assert!(
            production_context_bytes < normalized_bytes,
            "no whole large file in context"
        );
        let oracle = large_gold(format);
        let late_score = score(
            std::slice::from_ref(&oracle),
            &[passage],
            production_context_bytes,
            &corpus,
        );
        assert!(late_score.full_support);
        assert_eq!(late_score.citation_valid, 1);
        assert_eq!(late_score.citation_total, 1);
        let times = &ingest_ms[format];
        formats.insert(format.to_owned(), serde_json::json!({
            "passage_count":file_passages.len(),
            "normalized_bytes":normalized_bytes,
            "chunk_generation_ms":generation_ms[format],
            "trusted_activation_ms":{"samples":times.len(),"p50":percentile(times,50),"p95":percentile(times,95),"raw":times},
            "end_to_end_public_search_ms":{"scope":"HTTP/router + database search + Rust packing","first_query_before_timed_samples":first_query_before_timed_samples_ms,"pool_note":"One shared reader pool is created before the format loop; only the first overall query begins on that newly created pool, and later formats reuse it.","samples":search_ms.len(),"p50":percentile(&search_ms,50),"p95":percentile(&search_ms,95),"raw":search_ms},
            "context_bytes":production_context_bytes,
            "late_evidence":{"required_evidence":oracle,"score":late_score,"passage_number":107,"source_sha256":passage.source_sha256,"source_revision":passage.source_revision,"locator":passage.locator},
        }));
    }
    let rss_after = sampled_rss_bytes();
    let report = serde_json::json!({
        "status":"passed",
        "mode":"standalone_large_logical_files",
        "manifest":{"files":LARGE_FORMATS.len(),"passages":corpus.len(),"harness_sha256":digest(include_bytes!("office_retrieval.rs")),"git_head":tool_version("git", &["rev-parse","HEAD"]),"rustc":tool_version("rustc", &["--version"]),"profile":if cfg!(debug_assertions){"debug"}else{"release"},"synthetic_only":true,"hosted_ai":false,"context_budget_bytes":CONTEXT_BUDGET,"binary_parsing_ms":null,"binary_parsing_note":"This mode reuses parser-validated normalized shapes; it measures chunk generation and public retrieval, not distinct binary parsing."},
        "deterministic_manifest":{"path":manifest_path,"sha256":manifest_sha256,"stable":true},
        "activation_receipts":{"stable":true,"passages":corpus},
        "formats":formats,
        "aggregate_end_to_end_public_search_ms":{"samples":all_search_ms.len(),"p50":percentile(&all_search_ms,50),"p95":percentile(&all_search_ms,95),"raw":all_search_ms,"warmup_note":"Each format has one validated first_query_before_timed_samples observation. A single shared reader pool is created before the format loop, so only PDF's first query begins on the newly created pool."},
        "rss":{"sampled_process_rss_bytes":rss_before.into_iter().chain(rss_after).max(),"note":"Maximum of before/after benchmark-process RSS samples; measurement only, not a true peak. The real-fixture parser child separately has fail-closed sampled RSS supervision, not a native hard ceiling."},
        "failures":[],
        "limits":"Standalone synthetic logical retrieval/chunk measurement only; trusted test activation plus current public search with automatic validated reciprocal adjacent_continuation, no public knob or N+1 read. No production upload, ingestion, parser, or binary parsing performance claim. Native kernel memory-ceiling enforcement remains unresolved."
    });
    std::fs::write(
        report_path,
        serde_json::to_vec_pretty(&report).expect("large-file report JSON"),
    )
    .expect("write large-file report");
    fresh_runtime.close().await;
    worker.close().await;
    migrator.close().await;
    report
}

include!("n1_retrieval.rs");
include!("n2_lexical.rs");

#[tokio::test]
#[ignore = "resets the synthetic database and activates 5 documents / 540 passages; run just benchmark-office-large"]
async fn office_large_files() {
    let report = run_large_files().await;
    println!(
        "office large-file benchmark: {} files, {} passages",
        report["manifest"]["files"], report["manifest"]["passages"]
    );
}

#[tokio::test]
#[ignore = "resets the synthetic database and activates 100 documents / 1,040 or 10,040 passages; run just benchmark-office"]
async fn office_corpus() {
    let requested = std::env::var("OFFICE_BENCH_SCALE").unwrap_or_else(|_| "both".to_owned());
    let scales: &[Scale] = match requested.as_str() {
        "1k" => &[Scale::Thousand],
        "10k" => &[Scale::TenThousand],
        "both" => &[Scale::Thousand, Scale::TenThousand],
        _ => panic!("OFFICE_BENCH_SCALE must be 1k, 10k, or both"),
    };
    for &scale in scales {
        let report = run_scale(scale).await;
        println!(
            "office benchmark {}: {} files, {} passages",
            scale.name(),
            report["manifest"]["files"],
            report["manifest"]["passages"]
        );
    }
}
