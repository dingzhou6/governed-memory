//! Opt-in, request-scoped benchmark observation. Never serialized in public responses.
use serde_json::Value;
use std::{cell::RefCell, future::Future};
tokio::task_local! {static EVENTS: RefCell<Vec<Value>>;}
pub async fn capture<F: Future>(future: F) -> (F::Output, Vec<Value>) {
    EVENTS
        .scope(RefCell::new(Vec::new()), async move {
            let output = future.await;
            (output, EVENTS.with(RefCell::take))
        })
        .await
}
pub(crate) fn enabled() -> bool {
    EVENTS.try_with(|_| ()).is_ok()
}
pub(crate) fn record(event: Value) {
    let _ = EVENTS.try_with(|events| events.borrow_mut().push(event));
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn whole_passage_trace_observes_actual_budget_without_changing_it() {
        let (remaining, events) = super::capture(async {
            let mut budget = crate::ExcerptBudget::new(4);
            assert_eq!(budget.take_whole("abc"), Some("abc".into()));
            assert_eq!(budget.take_whole("é"), None);
            budget.remaining
        })
        .await;
        assert_eq!(remaining, 1);
        assert_eq!(
            events.len(),
            2,
            "actual whole-passage decisions must be observable"
        );
        assert_eq!(events[0]["accepted"], true);
        assert_eq!(events[1]["accepted"], false);
        assert_eq!(events[1]["remaining_before"], 1);
        assert!(!super::enabled());
    }
}

pub(crate) fn row(
    row: &sqlx::postgres::PgRow,
    ordinal: usize,
    decision: &str,
    remaining: usize,
    selected_count: usize,
) {
    use sqlx::Row;
    if !enabled() {
        return;
    }
    let mut value = serde_json::json!({"stage":"packing","ordinal":ordinal,"decision":decision,"remaining_bytes":remaining,"selected_count":selected_count});
    for key in [
        "item_id",
        "revision_id",
        "source_revision_id",
        "extraction_set_id",
        "passage_id",
        "hit_kind",
        "locator",
        "content",
        "recorded_at",
        "valid_from",
        "valid_until",
        "validity_status",
    ] {
        value[key] = row
            .try_get::<Option<String>, _>(key)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::String);
    }
    value["parent_passage_ids"] = serde_json::json!(
        row.try_get::<Option<Vec<String>>, _>("parent_passage_ids")
            .ok()
            .flatten()
    );
    value["candidate_omitted"] =
        serde_json::json!(row.try_get::<bool, _>("candidate_omitted").ok());
    record(value);
}
pub(crate) fn tail(
    rows: &[sqlx::postgres::PgRow],
    start: usize,
    decision: &str,
    remaining: usize,
    selected_count: usize,
) {
    if enabled() {
        for (ordinal, item) in rows.iter().enumerate().skip(start) {
            row(item, ordinal, decision, remaining, selected_count);
        }
    }
}

#[cfg(test)]
mod isolation_tests {
    #[tokio::test]
    async fn empty_scope_cannot_reuse_previous_trace() {
        let ((), first) = super::capture(async {
            super::record(serde_json::json!({"private":"first"}));
        })
        .await;
        assert_eq!(first.len(), 1);
        let ((), second) = super::capture(async {}).await;
        assert!(second.is_empty());
        assert!(!super::enabled());
        super::record(serde_json::json!({"outside":"ignored"}));
        let ((), third) = super::capture(async {}).await;
        assert!(third.is_empty());
    }
}
