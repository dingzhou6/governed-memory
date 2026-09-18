//! Local, versioned evidence counting. No model or network calls.
use super::{RequestFailure, SearchCitation, SearchItem};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Write,
    sync::{Arc, OnceLock},
};
use tokio::sync::Semaphore;

pub(super) const TOKENIZER: &str = "o200k_base:tiktoken-rs-0.12.0";
const MAX_SERIALIZED_BYTES: usize = 512 * 1024;
const MAX_COUNTS: usize = 83;
static TOKENIZER_CORE: OnceLock<Result<tiktoken_rs::CoreBPE, ()>> = OnceLock::new();
static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Allowance {
    tokenizer: String,
    max_tokens: u16,
}

pub(super) fn present<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Allowance>, D::Error> {
    Allowance::deserialize(d).map(Some)
}

impl Allowance {
    pub(super) fn validate(&self) -> Result<(), RequestFailure> {
        if self.tokenizer != TOKENIZER {
            return Err(RequestFailure::UnsupportedContextTokenizer);
        }
        if self.max_tokens == 0 {
            return Err(RequestFailure::InvalidContextBudget);
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub(super) struct Context {
    format: &'static str,
    tokenizer: &'static str,
    max_tokens: u16,
    token_count: usize,
    text: String,
}

#[derive(Serialize)]
struct Evidence<'a> {
    item_id: &'a str,
    revision_id: &'a str,
    text: &'a str,
    citation: &'a Option<SearchCitation>,
}
#[derive(Serialize)]
struct Envelope<'a> {
    evidence: Vec<Evidence<'a>>,
}

struct BoundedWriter(Vec<u8>);
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_SERIALIZED_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("context serialization bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn render(items: &[SearchItem], candidate: Option<&SearchItem>) -> Result<String, RequestFailure> {
    if items.len() + usize::from(candidate.is_some()) > 12 {
        return Err(RequestFailure::ContextBudgetUnavailable);
    }
    let evidence = items
        .iter()
        .chain(candidate)
        .map(|item| Evidence {
            item_id: &item.item_id,
            revision_id: &item.revision_id,
            text: &item.excerpt,
            citation: &item.citation,
        })
        .collect();
    let mut writer = BoundedWriter(Vec::new());
    serde_json::to_writer(&mut writer, &Envelope { evidence })
        .map_err(|_| RequestFailure::ContextBudgetUnavailable)?;
    String::from_utf8(writer.0).map_err(|_| RequestFailure::ContextBudgetUnavailable)
}

pub(super) struct Work {
    core: &'static tiktoken_rs::CoreBPE,
    context: Context,
    counts: usize,
    bytes: usize,
}
impl Work {
    fn new(allowance: &Allowance) -> Result<Self, RequestFailure> {
        allowance.validate()?;
        let core = TOKENIZER_CORE
            .get_or_init(|| tiktoken_rs::o200k_base().map_err(|_| ()))
            .as_ref()
            .map_err(|()| RequestFailure::ContextBudgetUnavailable)?;
        let mut work = Self {
            core,
            context: Context {
                format: "native-evidence-json-v1",
                tokenizer: TOKENIZER,
                max_tokens: allowance.max_tokens,
                token_count: 0,
                text: String::new(),
            },
            counts: 0,
            bytes: 0,
        };
        let text = render(&[], None)?;
        let count = work.count(&text)?;
        if count > usize::from(allowance.max_tokens) {
            return Err(RequestFailure::InvalidContextBudget);
        }
        work.context.text = text;
        work.context.token_count = count;
        Ok(work)
    }
    fn count(&mut self, text: &str) -> Result<usize, RequestFailure> {
        if self.counts >= MAX_COUNTS
            || text.len() > MAX_SERIALIZED_BYTES
            || self
                .bytes
                .checked_add(text.len())
                .is_none_or(|n| n > MAX_COUNTS * MAX_SERIALIZED_BYTES)
        {
            return Err(RequestFailure::ContextBudgetUnavailable);
        }
        self.counts += 1;
        self.bytes += text.len();
        self.core
            .encode(text, &HashSet::new())
            .map(|(tokens, _)| tokens.len())
            .map_err(|_| RequestFailure::ContextBudgetUnavailable)
    }
    pub(super) fn admit(
        &mut self,
        items: &[SearchItem],
        candidate: &SearchItem,
    ) -> Result<bool, RequestFailure> {
        let text = render(items, Some(candidate))?;
        let count = self.count(&text)?;
        let accepted = count <= usize::from(self.context.max_tokens);
        #[cfg(feature = "benchmark-trace")]
        super::benchmark_trace::record(
            serde_json::json!({"stage":"token_budget", "token_count":count,
            "max_tokens":self.context.max_tokens,"serialized_bytes":text.len(),"accepted":accepted}),
        );
        if accepted {
            self.context.text = text;
            self.context.token_count = count;
        }
        Ok(accepted)
    }
}

pub(super) async fn pack(
    rows: Vec<sqlx::postgres::PgRow>,
    bytes: u16,
    status: &'static str,
    allowance: Allowance,
) -> Result<(Vec<SearchItem>, usize, bool, Context), RequestFailure> {
    if rows.len() > 82 {
        return Err(RequestFailure::ContextBudgetUnavailable);
    }
    let permit = WORKERS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| RequestFailure::ContextBudgetUnavailable)?;
    #[cfg(feature = "benchmark-trace")]
    let observed = super::benchmark_trace::enabled();
    let joined = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let run = || {
            let mut work = Work::new(&allowance)?;
            let (items, used, truncated) =
                super::pack_search_rows(&rows, bytes, status, Some(&mut work))?;
            Ok((items, used, truncated, work.context))
        };
        #[cfg(feature = "benchmark-trace")]
        {
            if observed {
                super::benchmark_trace::capture_sync(run)
            } else {
                (run(), Vec::new())
            }
        }
        #[cfg(not(feature = "benchmark-trace"))]
        {
            run()
        }
    })
    .await
    .map_err(|_| RequestFailure::ContextBudgetUnavailable)?;
    #[cfg(feature = "benchmark-trace")]
    {
        let (result, events) = joined;
        for event in events {
            super::benchmark_trace::record(event);
        }
        result
    }
    #[cfg(not(feature = "benchmark-trace"))]
    {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(text: &str) -> SearchItem {
        SearchItem {
            item_id: "item".into(),
            revision_id: "revision".into(),
            excerpt: text.into(),
            recorded_at: "ignored".into(),
            valid_from: None,
            valid_until: None,
            validity_status: "current".into(),
            reason: "lexical",
            semantic_status: "not_requested",
            citation: None,
        }
    }
    #[test]
    fn exact_envelope_counts_and_whole_candidate_rejection() {
        let allowance = Allowance {
            tokenizer: TOKENIZER.into(),
            max_tokens: 128,
        };
        let mut work = Work::new(&allowance).unwrap();
        assert_eq!(work.context.text, r#"{"evidence":[]}"#);
        let large = item(&"中文 English ".repeat(100));
        assert!(!work.admit(&[], &large).unwrap());
        assert_eq!(work.context.text, r#"{"evidence":[]}"#);
        let small = item("中文 English <|endoftext|>");
        assert!(work.admit(&[], &small).unwrap());
        assert_eq!(
            work.context.text,
            r#"{"evidence":[{"item_id":"item","revision_id":"revision","text":"中文 English <|endoftext|>","citation":null}]}"#
        );
        assert_eq!(
            work.context.token_count,
            work.core.encode_ordinary(&work.context.text).len()
        );
        assert_eq!(work.counts, 3);
        let standalone = work.core.encode_ordinary(&small.excerpt).len();
        assert!(
            work.context.token_count > standalone,
            "wrappers are counted"
        );
        work.counts = MAX_COUNTS;
        assert!(matches!(
            work.admit(&[], &small),
            Err(RequestFailure::ContextBudgetUnavailable)
        ));
    }
    #[test]
    fn limits_and_bounded_serialization_fail_closed() {
        for max_tokens in [0, 1] {
            assert!(matches!(
                Work::new(&Allowance {
                    tokenizer: TOKENIZER.into(),
                    max_tokens
                }),
                Err(RequestFailure::InvalidContextBudget)
            ));
        }
        assert!(matches!(
            Work::new(&Allowance {
                tokenizer: "unsupported".into(),
                max_tokens: 128
            }),
            Err(RequestFailure::UnsupportedContextTokenizer)
        ));
        assert!(
            Work::new(&Allowance {
                tokenizer: TOKENIZER.into(),
                max_tokens: u16::MAX
            })
            .is_ok()
        );
        let giant = item(&"\u{0001}".repeat(MAX_SERIALIZED_BYTES / 6 + 1));
        assert!(matches!(
            render(&[], Some(&giant)),
            Err(RequestFailure::ContextBudgetUnavailable)
        ));
        let mut writer = BoundedWriter(Vec::new());
        assert!(writer.write_all(&vec![b'x'; MAX_SERIALIZED_BYTES]).is_ok());
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.0.len(), MAX_SERIALIZED_BYTES);
    }
    #[tokio::test]
    async fn token_failures_are_sanitized_and_never_degrade_to_lexical_retry() {
        for (error, status, code) in [
            (
                RequestFailure::ContextBudgetUnavailable,
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "context_budget_unavailable",
            ),
            (
                RequestFailure::InvalidContextBudget,
                axum::http::StatusCode::BAD_REQUEST,
                "invalid_context_budget",
            ),
            (
                RequestFailure::UnsupportedContextTokenizer,
                axum::http::StatusCode::BAD_REQUEST,
                "unsupported_context_tokenizer",
            ),
        ] {
            assert!(!error.permits_semantic_fallback(false));
            let response = error.response("test".into());
            assert_eq!(response.status(), status);
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"request_id":"test","status":code,"code":code})
            );
        }
        assert!(RequestFailure::storage(sqlx::Error::PoolClosed).permits_semantic_fallback(false));
        assert!(!RequestFailure::storage(sqlx::Error::PoolClosed).permits_semantic_fallback(true));
    }
    #[tokio::test]
    async fn blocking_worker_bound_is_non_waiting() {
        let semaphore = WORKERS.get_or_init(|| Arc::new(Semaphore::new(2))).clone();
        let first = semaphore.clone().try_acquire_owned().unwrap();
        let second = semaphore.try_acquire_owned().unwrap();
        let result = pack(
            Vec::new(),
            8192,
            "not_requested",
            Allowance {
                tokenizer: TOKENIZER.into(),
                max_tokens: 128,
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(RequestFailure::ContextBudgetUnavailable)
        ));
        drop((first, second));
        let result = pack(
            Vec::new(),
            8192,
            "not_requested",
            Allowance {
                tokenizer: TOKENIZER.into(),
                max_tokens: 128,
            },
        )
        .await
        .unwrap();
        assert_eq!(result.3.text, r#"{"evidence":[]}"#);
    }
}

#[cfg(test)]
mod source_block_order {
    fn source_block_indices(keys: &[&str]) -> Vec<usize> {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut index = std::collections::HashMap::<&str, usize>::new();
        for (i, key) in keys.iter().enumerate() {
            if let Some(&group) = index.get(key) {
                groups[group].push(i);
            } else {
                index.insert(*key, groups.len());
                groups.push(vec![i]);
            }
        }
        groups.into_iter().flatten().collect()
    }

    #[test]
    fn later_same_source_span_precedes_the_item_cap() {
        let mut keys = vec!["en09-snapshot"];
        let fillers: Vec<String> = (0..11).map(|i| format!("filler-{i:02}")).collect();
        keys.extend(fillers.iter().map(String::as_str));
        keys.push("en09-snapshot");
        let identity: Vec<usize> = (0..keys.len()).collect();
        assert!(
            !identity[..12].contains(&(keys.len() - 1)),
            "rank-first 12/4/2160 drops the later same-source ID"
        );
        let blocked = source_block_indices(&keys);
        assert_eq!(blocked[0], 0);
        assert_eq!(blocked[1], keys.len() - 1);
        assert!(
            blocked[..12].contains(&(keys.len() - 1)),
            "source-block must pack en09-p01-shaped IDs still in the fused window"
        );
    }

    fn lex_head_then_source_block(rows: &[(&str, Option<i32>)]) -> Vec<usize> {
        let mut head = Vec::new();
        let mut rest = Vec::new();
        for (i, (_, lex)) in rows.iter().enumerate() {
            if lex.is_some_and(|rank| rank <= 4) {
                head.push(i);
            } else {
                rest.push(i);
            }
        }
        let rest_keys: Vec<&str> = rest.iter().map(|&i| rows[i].0).collect();
        let blocked = source_block_indices(&rest_keys);
        head.extend(blocked.into_iter().map(|j| rest[j]));
        head
    }

    #[test]
    fn later_source_lexical_head_precedes_the_item_cap() {
        // en-q1 shape after 0010: three earlier sources dump 4 each and fill 12;
        // the needed later-source ID stays in fused 41 at source_first>1 with lex<=4.
        let mut rows: Vec<(&str, Option<i32>)> = Vec::new();
        for (src, lex) in [("dump-a", 1), ("dump-b", 2), ("dump-c", 3)] {
            rows.push((src, Some(lex)));
            for _ in 0..3 {
                rows.push((src, None));
            }
        }
        rows.push(("later-en09", Some(4)));
        let needed = rows.len() - 1;
        let identity: Vec<usize> = (0..rows.len()).collect();
        assert!(
            !identity[..12].contains(&needed),
            "rank-first 12/4/2160 drops the later-source lex head"
        );
        let blocked_keys: Vec<&str> = rows.iter().map(|row| row.0).collect();
        let blocked = source_block_indices(&blocked_keys);
        assert!(
            !blocked[..12].contains(&needed),
            "source-block 12/4/2160 still drops a later source whose source_first is not 1"
        );
        let headed = lex_head_then_source_block(&rows);
        assert!(
            headed[..12].contains(&needed),
            "lexical-head k=4 must pack the in-window later-source ID"
        );
    }

    fn source_first_of<'a>(
        rows: &'a [(&'a str, Option<i32>, usize)],
    ) -> std::collections::HashMap<&'a str, usize> {
        let mut first = std::collections::HashMap::new();
        for (i, (src, _, _)) in rows.iter().enumerate() {
            first.entry(*src).or_insert(i);
        }
        first
    }

    fn lex_head_defer_large(rows: &[(&str, Option<i32>, usize)], t: usize) -> Vec<usize> {
        // 0012: lex<=4 heads (non-huge), then source-block remainder ordered by
        // source_first = min fused index of the whole source (heads included),
        // then octet_length>t last.
        let source_first = source_first_of(rows);
        let is_huge = |i: usize| rows[i].2 > t;
        let is_head = |i: usize| !is_huge(i) && rows[i].1.is_some_and(|rank| rank <= 4);
        let mut heads = Vec::new();
        let mut rest = Vec::new();
        let mut huge = Vec::new();
        for i in 0..rows.len() {
            if is_huge(i) {
                huge.push(i);
            } else if is_head(i) {
                heads.push(i);
            } else {
                rest.push(i);
            }
        }
        rest.sort_by_key(|&i| (source_first[rows[i].0], i));
        let blocked = source_block_indices(&rest.iter().map(|&i| rows[i].0).collect::<Vec<_>>())
            .into_iter()
            .map(|j| rest[j])
            .collect::<Vec<_>>();
        heads.extend(blocked);
        heads.extend(huge);
        heads
    }

    #[test]
    fn huge_same_clause_lex_head_is_emitted_after_the_in_window_basis() {
        // en-q1 after 0011: lex-head admits a 4356-byte same-clause decoy and
        // 12/4/2160 token-stops before the rest of an in-window complete basis.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        for (src, lex) in [("dump-a", 1), ("later-en09", 4), ("dump-b", 2)] {
            rows.push((src, Some(lex), 520));
        }
        rows.push(("huge-en05", Some(3), 4356));
        for _ in 0..7 {
            rows.push(("remainder-en13", None, 520));
        }
        rows.push(("remainder-en03", None, 553));
        rows.push(("remainder-en03", None, 589));
        let needed_tail = rows.len() - 1;
        let huge = 3;
        let headed = lex_head_then_source_block(
            &rows
                .iter()
                .map(|(src, lex, _)| (*src, *lex))
                .collect::<Vec<_>>(),
        );
        assert!(
            headed[..6].contains(&huge),
            "0011 lex-head visits the huge same-clause decoy before the basis tail"
        );
        let deferred = lex_head_defer_large(&rows, 2048);
        assert!(
            !deferred[..12].contains(&huge),
            "size-defer must not spend the first 12 visits on the huge decoy"
        );
        assert!(
            deferred[..12].contains(&needed_tail),
            "size-defer must visit the in-window complete-basis tail before the huge decoy"
        );
    }

    fn headed_lex_neighbor(
        rows: &[(&str, Option<i32>, usize)],
        neighbor_k: i32,
        huge_t: usize,
    ) -> Vec<usize> {
        // 0014: lex<=4 heads, then extra_seq=1 with lex<=neighbor_k, then
        // source-block remainder, then huge. 0015: extra_seq>=3 of a source
        // that already attached that neighbor waits behind the remainder
        // (unheaded rows and extra_seq=2). Sources whose first extra missed
        // the neighbor band keep all extras in the remainder (en09-p01).
        let source_first = source_first_of(rows);
        let is_huge = |i: usize| rows[i].2 > huge_t;
        let is_head = |i: usize| !is_huge(i) && rows[i].1.is_some_and(|rank| rank <= 4);
        let mut heads = Vec::new();
        let mut headed = std::collections::HashSet::new();
        let mut extras = std::collections::HashMap::<&str, Vec<usize>>::new();
        for (i, (src, _, _)) in rows.iter().enumerate() {
            if is_head(i) {
                heads.push(i);
                headed.insert(*src);
            } else if !is_huge(i) {
                extras.entry(*src).or_default().push(i);
            }
        }
        for ids in extras.values_mut() {
            ids.sort_unstable();
        }
        let mut neighbors = Vec::new();
        let mut rest = Vec::new();
        let mut deferred = Vec::new();
        for (src, ids) in &extras {
            if headed.contains(src) && rows[ids[0]].1.is_some_and(|rank| rank <= neighbor_k) {
                neighbors.push(ids[0]);
                rest.extend(ids.iter().copied().skip(1).take(1));
                deferred.extend(ids.iter().copied().skip(2));
            } else {
                rest.extend(ids.iter().copied());
            }
        }
        neighbors.sort_by_key(|&i| (source_first[rows[i].0], i));
        rest.sort_by_key(|&i| (source_first[rows[i].0], i));
        deferred.sort_by_key(|&i| (source_first[rows[i].0], i));
        let blocked = source_block_indices(&rest.iter().map(|&i| rows[i].0).collect::<Vec<_>>())
            .into_iter()
            .map(|j| rest[j])
            .collect::<Vec<_>>();
        let deferred_block =
            source_block_indices(&deferred.iter().map(|&i| rows[i].0).collect::<Vec<_>>())
                .into_iter()
                .map(|j| deferred[j])
                .collect::<Vec<_>>();
        let huge: Vec<usize> = (0..rows.len()).filter(|&i| is_huge(i)).collect();
        heads.extend(neighbors);
        heads.extend(blocked);
        heads.extend(deferred_block);
        heads.extend(huge);
        heads
    }

    #[test]
    fn headed_lex_neighbor_packs_later_same_source_extra() {
        // Public-test shape: unheaded dumps have better source_first than the
        // headed source, so 0012 remainder source-block fills 12 with dumps
        // before the headed extra (zh10-p01, lex 11). Heads-first fixtures
        // inherit the extra's source_first from the head and already pack it.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        for src in ["dump-a", "dump-b"] {
            for _ in 0..4 {
                rows.push((src, None, 520));
            }
        }
        for (src, lex) in [
            ("head-a", 1),
            ("head-b", 2),
            ("head-need", 3),
            ("head-d", 4),
        ] {
            rows.push((src, Some(lex), 520));
        }
        rows.push(("head-need", Some(11), 520));
        let needed = rows.len() - 1;
        let deferred = lex_head_defer_large(&rows, 2048);
        assert!(
            !deferred[..12].contains(&needed),
            "0012 source-block 12/4/2160 still drops the headed source extra"
        );
        let neighbored = headed_lex_neighbor(&rows, 16, 2048);
        assert!(
            neighbored[..12].contains(&needed),
            "Funès-style lex<=16 extra of a k=4 head must pack before unheaded dumps fill 12"
        );
    }

    #[test]
    fn weak_lex_neighbor_does_not_steal_unheaded_fourth() {
        // en-q1: unheaded gold has better source_first than headed extras.
        // 0012 remainder already groups extras by the head's source_first, so
        // they only steal the 4th gold when promoted into the neighbor tier
        // (lex_cut 24). lex_cut 16 must leave that 4th row in the first 12.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        for _ in 0..4 {
            rows.push(("dump-early", None, 520));
        }
        for _ in 0..4 {
            rows.push(("unheaded-gold", None, 520));
        }
        let fourth_gold = rows.len() - 1;
        for (src, lex) in [("head-a", 1), ("head-b", 2), ("head-c", 4)] {
            rows.push((src, Some(lex), 520));
        }
        rows.push(("head-a", Some(23), 520));
        rows.push(("head-b", Some(23), 520));
        let wide = headed_lex_neighbor(&rows, 24, 2048);
        assert!(
            !wide[..12].contains(&fourth_gold),
            "precondition: two lex=23 extras under a too-wide band steal the 4th gold"
        );
        let bounded = headed_lex_neighbor(&rows, 16, 2048);
        assert!(
            bounded[..12].contains(&fourth_gold),
            "unheaded 4th gold row must stay in the 12/4/2160 window"
        );
    }

    #[test]
    fn headed_second_extra_stays_in_remainder() {
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        for src in ["dump-a", "dump-b"] {
            for _ in 0..4 {
                rows.push((src, None, 520));
            }
        }
        rows.push(("head-need", Some(3), 520));
        rows.push(("head-need", Some(11), 520));
        rows.push(("head-need", Some(12), 520));
        let first_extra = rows.len() - 2;
        let second_extra = rows.len() - 1;
        let neighbored = headed_lex_neighbor(&rows, 16, 2048);
        assert_eq!(neighbored[1], first_extra);
        assert!(
            neighbored.iter().position(|&i| i == second_extra)
                > neighbored.iter().position(|&i| i == first_extra)
        );
        assert!(
            !neighbored[..3].contains(&second_extra),
            "extra_seq>1 must not enter the neighbor tier"
        );
    }

    #[test]
    fn huge_headed_extra_is_never_a_neighbor() {
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        for src in ["dump-a", "dump-b", "dump-c"] {
            for _ in 0..4 {
                rows.push((src, None, 520));
            }
        }
        rows.push(("head-need", Some(3), 520));
        rows.push(("head-need", Some(11), 4356));
        let huge = rows.len() - 1;
        let neighbored = headed_lex_neighbor(&rows, 16, 2048);
        assert_eq!(*neighbored.last().unwrap(), huge);
        assert!(
            !neighbored[..12].contains(&huge),
            "octet_length>2048 extras must stay in the huge tier even when lex<=16"
        );
    }

    #[test]
    fn headed_extra_seq3_after_neighbor_defers_behind_unheaded() {
        // zh-q3 after 0014: head+neighbor+extra_seq=2 stay, but extra_seq>=3
        // (zh07-p00) still emits in the remainder source-block before unheaded
        // zh08-p01 and fills 12. Defer extra_seq>=3 only when the source already
        // attached a lex<=16 neighbor; keep singleton k=4 heads.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        rows.push(("dump-a", Some(2), 520));
        for _ in 0..3 {
            rows.push(("dump-a", None, 520));
        }
        rows.push(("dump-b", Some(3), 520));
        for _ in 0..2 {
            rows.push(("dump-b", None, 520));
        }
        rows.push(("dump-c", Some(4), 520));
        rows.push(("head-need", Some(1), 520));
        rows.push(("head-need", Some(11), 520));
        rows.push(("head-need", Some(12), 520));
        rows.push(("head-need", None, 235));
        rows.push(("unheaded-gold", None, 520));
        let stub = rows.len() - 2;
        let gold = rows.len() - 1;
        let extra2 = rows.len() - 3;
        let neighbor = rows.len() - 4;
        let head = rows.len() - 5;
        let singleton = rows.len() - 6;
        let current = headed_lex_neighbor(&rows, 16, 2048);
        assert!(
            current[..12].contains(&head) && current[..12].contains(&singleton),
            "k=4 heads including a later singleton must stay packed"
        );
        assert!(
            current[..12].contains(&neighbor) && current[..12].contains(&extra2),
            "neighbor and extra_seq=2 must stay in the early window"
        );
        assert!(
            current[..12].contains(&gold) && !current[..12].contains(&stub),
            "extra_seq>=3 after a real neighbor must yield a 12/4 slot to unheaded gold, packed {:?}",
            &current[..12]
        );
    }

    #[test]
    fn unheaded_lex_extra_seq3_stays_in_remainder() {
        // Unheaded source (no lex<=4 row) whose first extra is still lex<=16
        // must not use the neighbored extra_seq>=3 deferral.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        rows.push(("dump-a", Some(1), 520));
        for _ in 0..3 {
            rows.push(("dump-a", None, 520));
        }
        rows.push(("unheaded-lex", Some(10), 520));
        rows.push(("unheaded-lex", Some(11), 520));
        rows.push(("unheaded-lex", Some(12), 520));
        let third = rows.len() - 1;
        rows.push(("late-unheaded", None, 520));
        let late = rows.len() - 1;
        let ordered = headed_lex_neighbor(&rows, 16, 2048);
        let pos = |i: usize| ordered.iter().position(|&x| x == i).expect("row emitted");
        assert!(
            pos(third) < pos(late),
            "unheaded extra_seq>=3 must stay in the remainder, packed {ordered:?}"
        );
    }

    #[test]
    fn extra_seq3_without_neighbor_stays_in_remainder() {
        // en-q3: headed extra_seq=1 has lex=20 so it is not a neighbor.
        // extra_seq=3 gold (en09-p01) must stay with the remainder source-block,
        // not move behind a later unheaded source.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        rows.push(("dump-a", Some(1), 520));
        for _ in 0..3 {
            rows.push(("dump-a", None, 520));
        }
        rows.push(("head-en09", Some(2), 520));
        rows.push(("head-en09", Some(20), 520));
        rows.push(("head-en09", None, 233));
        rows.push(("head-en09", None, 526));
        let gold = rows.len() - 1;
        rows.push(("late-unheaded", None, 520));
        let late = rows.len() - 1;
        let ordered = headed_lex_neighbor(&rows, 16, 2048);
        let pos = |i: usize| ordered.iter().position(|&x| x == i).expect("row emitted");
        assert!(
            pos(gold) < pos(late),
            "lex-null extra_seq>=3 without a lex<=16 neighbor must stay before later unheaded rows, packed {ordered:?}"
        );
    }

    fn han_two_head_dump_rr(
        rows: &[(&str, Option<i32>, usize)],
        neighbor_k: i32,
        huge_t: usize,
    ) -> Vec<usize> {
        // 0016 Han-only: 0015 plus (1) extras of sources with 2+ lex heads wait
        // unless they are extra_seq=1 lex<=neighbor_k; (2) extra_seq>=2 of a
        // headed source that missed the neighbor band waits; (3) unheaded
        // remainder emits extra_seq=1 across sources before extra_seq=2.
        let source_first = source_first_of(rows);
        let is_huge = |i: usize| rows[i].2 > huge_t;
        let is_head = |i: usize| !is_huge(i) && rows[i].1.is_some_and(|rank| rank <= 4);
        let mut heads = Vec::new();
        let mut headed = std::collections::HashMap::<&str, usize>::new();
        let mut extras = std::collections::HashMap::<&str, Vec<usize>>::new();
        for (i, (src, _, _)) in rows.iter().enumerate() {
            if is_head(i) {
                heads.push(i);
                *headed.entry(*src).or_default() += 1;
            } else if !is_huge(i) {
                extras.entry(*src).or_default().push(i);
            }
        }
        for ids in extras.values_mut() {
            ids.sort_unstable();
        }
        let mut neighbors = Vec::new();
        let mut rest = Vec::new();
        let mut deferred = Vec::new();
        for (src, ids) in &extras {
            let nheads = headed.get(src).copied().unwrap_or(0);
            let has_head = nheads > 0;
            let neigh = has_head && rows[ids[0]].1.is_some_and(|rank| rank <= neighbor_k);
            if neigh {
                neighbors.push(ids[0]);
                rest.extend(ids.iter().copied().skip(1).take(1));
                deferred.extend(ids.iter().copied().skip(2));
                continue;
            }
            if nheads >= 2 {
                deferred.extend(ids.iter().copied());
                continue;
            }
            if has_head {
                rest.push(ids[0]);
                deferred.extend(ids.iter().copied().skip(1));
                continue;
            }
            rest.extend(ids.iter().copied());
        }
        neighbors.sort_by_key(|&i| (source_first[rows[i].0], i));
        let mut unheaded: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&i| !headed.contains_key(rows[i].0))
            .collect();
        let mut headed_rest: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&i| headed.contains_key(rows[i].0))
            .collect();
        headed_rest.sort_by_key(|&i| (source_first[rows[i].0], i));
        unheaded.sort_by_key(|&i| {
            let src = rows[i].0;
            let seq = extras[&src].iter().position(|&j| j == i).unwrap() + 1;
            (seq, source_first[src], i)
        });
        deferred.sort_by_key(|&i| (source_first[rows[i].0], i));
        let huge: Vec<usize> = (0..rows.len()).filter(|&i| is_huge(i)).collect();
        heads.extend(neighbors);
        heads.extend(headed_rest);
        heads.extend(unheaded);
        heads.extend(deferred);
        heads.extend(huge);
        heads
    }

    #[test]
    fn han_two_head_dump_rr_visits_late_unheaded_before_early_unheaded_span() {
        // zh-q1 after 0015: two lex heads on a dump source keep extras in the
        // remainder; an early unheaded source-block then fills 12 before a later
        // required unheaded row (zh11-p01). Han-only: defer two-head extras and
        // extra_seq>=2 without a neighbor, then extra_seq=1 round-robin.
        let mut rows: Vec<(&str, Option<i32>, usize)> = Vec::new();
        rows.push(("two-head", Some(1), 520));
        rows.push(("two-head", Some(2), 520));
        for _ in 0..3 {
            rows.push(("two-head", None, 520));
        }
        rows.push(("one-head", Some(3), 520));
        rows.push(("one-head", None, 520));
        rows.push(("one-head", None, 520));
        rows.push(("singleton", Some(4), 520));
        for _ in 0..4 {
            rows.push(("early-unheaded", None, 520));
        }
        rows.push(("late-gold", None, 520));
        let gold = rows.len() - 1;
        let current = headed_lex_neighbor(&rows, 16, 2048);
        assert!(
            !current[..12].contains(&gold),
            "precondition: 0015 source-block 12 still drops the late unheaded gold"
        );
        let ordered = han_two_head_dump_rr(&rows, 16, 2048);
        assert!(
            ordered[..12].contains(&gold),
            "Han two-head dump defer + unheaded extra_seq=1 rr must visit late gold, packed {:?}",
            &ordered[..12]
        );
        let singleton = rows
            .iter()
            .position(|(src, lex, _)| *src == "singleton" && *lex == Some(4))
            .unwrap();
        assert!(
            ordered[..12].contains(&singleton),
            "0011 singleton lex head must stay, packed {:?}",
            &ordered[..12]
        );
    }

    type HanAdjRow<'a> = (&'a str, Option<i32>, Option<i32>, usize, i32);

    fn han_unheaded_adj(rows: &[HanAdjRow<'_>], neighbor_k: i32, huge_t: usize) -> Vec<usize> {
        // 0017 Han-only: 0016 plus defer unheaded extra_seq=1 that fail
        // first_ok (sem<=10 OR lex<=6 OR lex-only lex<=16) and emit the next
        // passage of a kept unheaded first (p00→p01) beside that first.
        let mut source_first = std::collections::HashMap::<&str, usize>::new();
        for (i, (src, _, _, _, _)) in rows.iter().enumerate() {
            source_first.entry(*src).or_insert(i);
        }
        let is_huge = |i: usize| rows[i].3 > huge_t;
        let is_head = |i: usize| !is_huge(i) && rows[i].1.is_some_and(|rank| rank <= 4);
        let first_ok = |i: usize| {
            let lex = rows[i].1;
            let sem = rows[i].2;
            (sem.is_some_and(|rank| rank <= 10))
                || (lex.is_some_and(|rank| rank <= 6))
                || (sem.is_none() && lex.is_some_and(|rank| rank <= 16))
        };
        let mut heads = Vec::new();
        let mut headed = std::collections::HashMap::<&str, usize>::new();
        let mut extras = std::collections::HashMap::<&str, Vec<usize>>::new();
        for (i, (src, _, _, _, _)) in rows.iter().enumerate() {
            if is_head(i) {
                heads.push(i);
                *headed.entry(*src).or_default() += 1;
            } else if !is_huge(i) {
                extras.entry(*src).or_default().push(i);
            }
        }
        for ids in extras.values_mut() {
            ids.sort_unstable();
        }
        let mut neighbors = Vec::new();
        let mut rest = Vec::new();
        let mut deferred = Vec::new();
        for (src, ids) in &extras {
            let nheads = headed.get(src).copied().unwrap_or(0);
            let has_head = nheads > 0;
            let neigh = has_head && rows[ids[0]].1.is_some_and(|rank| rank <= neighbor_k);
            if neigh {
                neighbors.push(ids[0]);
                rest.extend(ids.iter().copied().skip(1).take(1));
                deferred.extend(ids.iter().copied().skip(2));
                continue;
            }
            if nheads >= 2 {
                deferred.extend(ids.iter().copied());
                continue;
            }
            if has_head {
                rest.push(ids[0]);
                deferred.extend(ids.iter().copied().skip(1));
                continue;
            }
            if !first_ok(ids[0]) {
                deferred.extend(ids.iter().copied());
                continue;
            }
            rest.push(ids[0]);
            let first_pass = rows[ids[0]].4;
            let mut saw_adj = false;
            for &i in ids.iter().skip(1) {
                if !saw_adj && rows[i].4 == first_pass + 1 {
                    rest.push(i);
                    saw_adj = true;
                } else {
                    deferred.push(i);
                }
            }
        }
        neighbors.sort_by_key(|&i| (source_first[rows[i].0], i));
        let mut unheaded: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&i| !headed.contains_key(rows[i].0))
            .collect();
        let mut headed_rest: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&i| headed.contains_key(rows[i].0))
            .collect();
        headed_rest.sort_by_key(|&i| (source_first[rows[i].0], i));
        unheaded.sort_by_key(|&i| {
            let src = rows[i].0;
            let ids = &extras[src];
            let seq = ids.iter().position(|&j| j == i).unwrap() + 1;
            let is_first = seq == 1;
            let is_adj = !is_first && rows[i].4 == rows[ids[0]].4 + 1;
            let emit_seq = if is_first || is_adj { 1 } else { seq };
            let group = i32::from(is_adj);
            (emit_seq, source_first[src], group, i)
        });
        deferred.sort_by_key(|&i| (source_first[rows[i].0], i));
        let huge: Vec<usize> = (0..rows.len()).filter(|&i| is_huge(i)).collect();
        heads.extend(neighbors);
        heads.extend(headed_rest);
        heads.extend(unheaded);
        heads.extend(deferred);
        heads.extend(huge);
        heads
    }

    #[test]
    fn han_unheaded_adj_visits_next_passage_before_dual_weak_firsts() {
        // zh-q1 after 0016: unheaded extra_seq=1 of zh12 is p00 (lex-only 11);
        // extra_seq=2 is p04; gold p01 is extra_seq=3. Dual-weak extra_seq=1
        // rows fill 12. Han-only: keep lex-only firsts and emit p00's next
        // passage beside it.
        let mut rows: Vec<HanAdjRow<'_>> = Vec::new();
        rows.push(("two-head", Some(1), Some(2), 520, 0));
        rows.push(("two-head", Some(2), Some(16), 520, 1));
        for pass in 2..5 {
            rows.push(("two-head", Some(20 + pass), Some(12 + pass), 520, pass));
        }
        rows.push(("one-head", Some(3), Some(4), 520, 0));
        rows.push(("one-head", Some(18), Some(5), 520, 1));
        rows.push(("singleton", Some(4), None, 520, 0));
        rows.push(("w0", Some(7), Some(21), 520, 0));
        rows.push(("w1", Some(8), Some(22), 520, 0));
        rows.push(("w2", Some(9), Some(23), 520, 0));
        rows.push(("w3", Some(10), Some(24), 520, 0));
        rows.push(("w4", Some(11), Some(25), 520, 0));
        rows.push(("w5", Some(12), Some(26), 520, 0));
        let p00 = rows.len();
        rows.push(("late-gold", Some(11), None, 520, 0));
        rows.push(("late-gold", Some(14), None, 520, 4));
        let p01 = rows.len();
        rows.push(("late-gold", Some(17), None, 520, 1));
        let current: Vec<usize> = {
            let mapped: Vec<(&str, Option<i32>, usize)> = rows
                .iter()
                .map(|&(src, lex, _, bytes, _)| (src, lex, bytes))
                .collect();
            han_two_head_dump_rr(&mapped, 16, 2048)
        };
        assert!(
            !current[..12].contains(&p01),
            "precondition: 0016 extra_seq RR still drops extra_seq=3 gold p01, packed {:?}",
            &current[..12]
        );
        let ordered = han_unheaded_adj(&rows, 16, 2048);
        assert!(
            ordered[..12].contains(&p00),
            "lex-only unheaded first p00 must stay, packed {:?}",
            &ordered[..12]
        );
        assert!(
            ordered[..12].contains(&p01),
            "han_unheaded_adj must pack p00's next passage, packed {:?}",
            &ordered[..12]
        );
        let singleton = rows
            .iter()
            .position(|(src, lex, _, _, _)| *src == "singleton" && *lex == Some(4))
            .unwrap();
        assert!(
            ordered[..12].contains(&singleton),
            "0011 singleton lex head must stay, packed {:?}",
            &ordered[..12]
        );
        let p00_pos = ordered.iter().position(|&i| i == p00).unwrap();
        let p01_pos = ordered.iter().position(|&i| i == p01).unwrap();
        assert!(
            p00_pos < p01_pos,
            "adjacent next passage emits after its first, packed {:?}",
            &ordered[..12]
        );
    }

    #[allow(clippy::too_many_lines)]
    fn han_deferred_span(rows: &[HanAdjRow<'_>], neighbor_k: i32, huge_t: usize) -> Vec<usize> {
        // 0018 Han-only: 0017 plus inverted one-head neighbor, two-head
        // extra_seq<=2 lex<=16 remainder, unheaded extra_seq=1 fails when
        // lex>=16, deferred sem-ok firsts promote pass+1 and pass+3, and
        // extra_seq 2-3 of the single best first_ok unheaded source pair
        // when that source has no adj.
        let mut source_first = std::collections::HashMap::<&str, usize>::new();
        for (i, (src, _, _, _, _)) in rows.iter().enumerate() {
            source_first.entry(*src).or_insert(i);
        }
        let is_huge = |i: usize| rows[i].3 > huge_t;
        let is_head = |i: usize| !is_huge(i) && rows[i].1.is_some_and(|rank| rank <= 4);
        let first_ok = |i: usize| {
            let lex = rows[i].1;
            let sem = rows[i].2;
            lex.is_none_or(|rank| rank < 16)
                && ((sem.is_some_and(|rank| rank <= 10))
                    || (lex.is_some_and(|rank| rank <= 6))
                    || (sem.is_none() && lex.is_some_and(|rank| rank <= 16)))
        };
        let mut heads = Vec::new();
        let mut headed = std::collections::HashMap::<&str, usize>::new();
        let mut extras = std::collections::HashMap::<&str, Vec<usize>>::new();
        for (i, (src, _, _, _, _)) in rows.iter().enumerate() {
            if is_head(i) {
                heads.push(i);
                *headed.entry(*src).or_default() += 1;
            } else if !is_huge(i) {
                extras.entry(*src).or_default().push(i);
            }
        }
        for ids in extras.values_mut() {
            ids.sort_unstable();
        }
        let mut first_ok_candidates = Vec::new();
        for (src, ids) in &extras {
            if headed.contains_key(src) {
                continue;
            }
            if first_ok(ids[0]) {
                first_ok_candidates.push(ids[0]);
            }
        }
        // SQL pair_pick ORDER BY sem NULLS LAST, lex NULLS LAST, source_first,
        // then item/revision/passage. This helper has no IDs; `i` is the
        // fused-order stand-in for that remaining key.
        first_ok_candidates.sort_by_key(|&i| {
            (
                rows[i].2.unwrap_or(i32::MAX),
                rows[i].1.unwrap_or(i32::MAX),
                source_first[rows[i].0],
                i,
            )
        });
        let pair_src = first_ok_candidates.first().map(|&i| rows[i].0);
        let mut neighbors = Vec::new();
        let mut rest = Vec::new();
        let mut deferred = Vec::new();
        for (src, ids) in &extras {
            let nheads = headed.get(src).copied().unwrap_or(0);
            let has_head = nheads > 0;
            let lex0 = rows[ids[0]].1;
            let sem0 = rows[ids[0]].2;
            let band = lex0.is_some_and(|rank| rank <= neighbor_k);
            let neigh = has_head
                && band
                && (nheads >= 2
                    || sem0.is_some_and(|sem| sem <= 10 || lex0.is_some_and(|lex| lex > 8)));
            if neigh {
                neighbors.push(ids[0]);
                rest.extend(ids.iter().copied().skip(1).take(1));
                deferred.extend(ids.iter().copied().skip(2));
                continue;
            }
            if has_head && band {
                deferred.extend(ids.iter().copied());
                continue;
            }
            if nheads >= 2 {
                if band {
                    rest.push(ids[0]);
                } else {
                    deferred.push(ids[0]);
                }
                for &i in ids.iter().skip(1).take(1) {
                    if rows[i].1.is_some_and(|rank| rank <= neighbor_k) {
                        rest.push(i);
                    } else {
                        deferred.push(i);
                    }
                }
                deferred.extend(ids.iter().copied().skip(2));
                continue;
            }
            if has_head {
                rest.push(ids[0]);
                deferred.extend(ids.iter().copied().skip(1));
                continue;
            }
            let ok = first_ok(ids[0]);
            let sem_deferred = !ok && sem0.is_some_and(|rank| rank <= 10);
            if !ok && !sem_deferred {
                deferred.extend(ids.iter().copied());
                continue;
            }
            if ok {
                rest.push(ids[0]);
            } else {
                deferred.push(ids[0]);
            }
            let first_pass = rows[ids[0]].4;
            // Match SQL `source_has_adj` (bool_or over the source), not a
            // step-local flag: any pass+1 (or deferred +1/+3) suppresses pair.
            let source_has_adj = ids.iter().skip(1).any(|&i| {
                let pass = rows[i].4;
                (sem_deferred && (pass == first_pass + 1 || pass == first_pass + 3))
                    || (ok && pass == first_pass + 1)
            });
            let allow_pair = ok && pair_src == Some(*src) && !source_has_adj;
            for &i in ids.iter().skip(1) {
                let pass = rows[i].4;
                let seq = ids.iter().position(|&j| j == i).unwrap() + 1;
                let is_adj = (sem_deferred && (pass == first_pass + 1 || pass == first_pass + 3))
                    || (ok && pass == first_pass + 1);
                if is_adj || (allow_pair && (2..=3).contains(&seq)) {
                    rest.push(i);
                } else {
                    deferred.push(i);
                }
            }
        }
        neighbors.sort_by_key(|&i| (source_first[rows[i].0], i));
        let mut unheaded: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&i| !headed.contains_key(rows[i].0))
            .collect();
        let mut headed_rest: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|&i| headed.contains_key(rows[i].0))
            .collect();
        headed_rest.sort_by_key(|&i| (source_first[rows[i].0], i));
        unheaded.sort_by_key(|&i| {
            let src = rows[i].0;
            let ids = &extras[src];
            let seq = ids.iter().position(|&j| j == i).unwrap() + 1;
            let first_pass = rows[ids[0]].4;
            let ok = first_ok(ids[0]);
            let sem_deferred = !ok && rows[ids[0]].2.is_some_and(|rank| rank <= 10);
            let is_first = seq == 1 && ok;
            let is_adj = !is_first
                && ((ok && rows[i].4 == first_pass + 1)
                    || (sem_deferred
                        && (rows[i].4 == first_pass + 1 || rows[i].4 == first_pass + 3)));
            let is_pair = ok
                && pair_src == Some(src)
                && (2..=3).contains(&seq)
                && !ids.iter().skip(1).any(|&j| rows[j].4 == first_pass + 1);
            let emit_seq = if is_first || is_adj || is_pair {
                1
            } else {
                seq
            };
            let group = if is_adj {
                1
            } else if is_pair {
                seq - 1
            } else {
                0
            };
            (emit_seq, source_first[src], group, i)
        });
        deferred.sort_by_key(|&i| (source_first[rows[i].0], i));
        let huge: Vec<usize> = (0..rows.len()).filter(|&i| is_huge(i)).collect();
        heads.extend(neighbors);
        heads.extend(headed_rest);
        heads.extend(unheaded);
        heads.extend(deferred);
        heads.extend(huge);
        heads
    }

    #[test]
    fn han_deferred_span_visits_skip_passage_of_sem_ok_deferred_first() {
        // zh-q2 after 0017: unheaded extra_seq=1 is p00 (lex 29, sem 6);
        // gold p03 is extra_seq=5 / pass+3. Dual-weak firsts fill 12.
        // Han-only: defer lex>=16 firsts that still have sem<=10 and emit
        // p00's next passage and pass+3 beside the remainder.
        let mut rows: Vec<HanAdjRow<'_>> = Vec::new();
        rows.push(("two-head", Some(1), Some(2), 520, 0));
        rows.push(("two-head", Some(2), Some(16), 520, 1));
        for pass in 2..5 {
            rows.push(("two-head", Some(20 + pass), Some(12 + pass), 520, pass));
        }
        rows.push(("one-head", Some(3), Some(4), 520, 0));
        rows.push(("one-head", Some(18), Some(5), 520, 1));
        rows.push(("singleton", Some(4), None, 520, 0));
        rows.push(("w0", Some(7), Some(21), 520, 0));
        rows.push(("w1", Some(8), Some(22), 520, 0));
        rows.push(("w2", Some(9), Some(23), 520, 0));
        rows.push(("w3", Some(10), Some(24), 520, 0));
        rows.push(("w4", Some(11), Some(25), 520, 0));
        rows.push(("w5", Some(12), Some(26), 520, 0));
        rows.push(("late-gold", Some(29), Some(6), 520, 0));
        rows.push(("late-gold", Some(30), Some(8), 520, 1));
        rows.push(("late-gold", Some(31), Some(12), 520, 2));
        let p03 = rows.len();
        rows.push(("late-gold", Some(32), Some(23), 520, 3));
        let current = han_unheaded_adj(&rows, 16, 2048);
        assert!(
            !current[..12].contains(&p03),
            "precondition: 0017 adj +1 still drops pass+3 gold p03, packed {:?}",
            &current[..12]
        );
        let ordered = han_deferred_span(&rows, 16, 2048);
        assert!(
            ordered[..12].contains(&p03),
            "han_deferred_span must pack deferred first's pass+3, packed {:?}",
            &ordered[..12]
        );
        let singleton = rows
            .iter()
            .position(|(src, lex, _, _, _)| *src == "singleton" && *lex == Some(4))
            .unwrap();
        assert!(
            ordered[..12].contains(&singleton),
            "0011 singleton lex head must stay, packed {:?}",
            &ordered[..12]
        );
        let p01 = rows
            .iter()
            .position(|(src, _, _, _, pass)| *src == "late-gold" && *pass == 1)
            .unwrap();
        assert!(
            ordered[..12].contains(&p01),
            "deferred sem-ok first still promotes next passage, packed {:?}",
            &ordered[..12]
        );
    }

    #[test]
    fn han_deferred_span_pairs_extra_seq2_and_3_of_first_ok_unheaded_without_adj() {
        // Public 0018 gate: first_ok p00, skipped p01, extras p02/p03. Seven
        // first_ok decoys plus four k=4 heads fill 12 before extra_seq 2-3
        // under 0017. Pair those extras when the source has no adj.
        let mut rows: Vec<HanAdjRow<'_>> = vec![
            ("two-head", Some(1), Some(2), 520, 0),
            ("two-head", Some(2), Some(3), 520, 1),
            ("one-head", Some(3), Some(4), 520, 0),
            ("singleton", Some(4), None, 520, 0),
        ];
        let p00 = rows.len();
        rows.push(("late-gold", Some(5), Some(1), 520, 0));
        let p02 = rows.len();
        rows.push(("late-gold", Some(20), Some(8), 520, 2));
        let p03 = rows.len();
        rows.push(("late-gold", Some(21), Some(9), 520, 3));
        for i in 0..7 {
            rows.push((
                ["w0", "w1", "w2", "w3", "w4", "w5", "w6"][i],
                Some(8 + i32::try_from(i).unwrap()),
                Some(4 + i32::try_from(i).unwrap()),
                520,
                0,
            ));
        }
        let current = han_unheaded_adj(&rows, 16, 2048);
        assert!(
            current[..12].contains(&p00)
                && !current[..12].contains(&p02)
                && !current[..12].contains(&p03),
            "precondition: 0017 packs first_ok p00 and drops pair extras, packed {:?}",
            &current[..12]
        );
        let ordered = han_deferred_span(&rows, 16, 2048);
        assert!(
            ordered[..12].contains(&p00)
                && ordered[..12].contains(&p02)
                && ordered[..12].contains(&p03),
            "han_deferred_span must pair extra_seq 2-3 of first_ok unheaded without adj, packed {:?}",
            &ordered[..12]
        );
        let singleton = rows
            .iter()
            .position(|(src, lex, _, _, _)| *src == "singleton" && *lex == Some(4))
            .unwrap();
        assert!(
            ordered[..12].contains(&singleton),
            "0011 singleton lex head must stay, packed {:?}",
            &ordered[..12]
        );
    }
}
