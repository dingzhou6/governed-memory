#[cfg(feature = "benchmark-trace")]
#[doc(hidden)]
pub mod benchmark_trace;

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgPool, Row, postgres::PgRow};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const READ_QUERY: &str = "SELECT item_id, revision_id, content, recorded_at,
           valid_from, valid_until, validity_status
    FROM read_current_item($1, $2, $3, $4, $5, $6)";

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);
const AUTHENTICATION_TIMEOUT: Duration = Duration::from_millis(500);
const EMPTY_BODY_TIMEOUT: Duration = Duration::from_millis(100);
const CREATE_BODY_TIMEOUT: Duration = Duration::from_millis(500);
const CREATE_BODY_LIMIT: usize = 64 * 1024;
const SEARCH_BODY_LIMIT: usize = 128 * 1024;
const MAX_EXTERNAL_VECTOR_DIMENSIONS: usize = 4096;

/// Search excerpt allowances in UTF-8 bytes, independent of model tokens.
#[derive(Clone, Copy, Debug)]
pub struct SearchLimits {
    default_context_bytes: u16,
    max_context_bytes: u16,
}

impl SearchLimits {
    /// Configure positive allowances without silently clamping requests.
    ///
    /// # Errors
    /// Returns an error unless `1 <= default <= maximum <= 65535`.
    pub fn new(default_context_bytes: u32, max_context_bytes: u32) -> Result<Self, &'static str> {
        if default_context_bytes == 0 || default_context_bytes > max_context_bytes {
            return Err("invalid search context limits");
        }
        Ok(Self {
            default_context_bytes: u16::try_from(default_context_bytes)
                .map_err(|_| "invalid search context limits")?,
            max_context_bytes: u16::try_from(max_context_bytes)
                .map_err(|_| "invalid search context limits")?,
        })
    }
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            default_context_bytes: 8192,
            max_context_bytes: 16384,
        }
    }
}

pub fn router(pool: PgPool) -> Router {
    router_with_search_limits(pool, SearchLimits::default())
}

/// Construct the same public routes with operator-selected search allowances.
pub fn router_with_search_limits(pool: PgPool, limits: SearchLimits) -> Router {
    Router::new()
        .route("/v1/collections", get(list_collections))
        .route("/v1/items", get(list_items))
        .route(
            "/v1/items/{item_id}",
            get(read_item).put(correct_memory).delete(forget_memory),
        )
        .route("/v1/memories", post(create_memory))
        .route("/v1/search", post(search_memories))
        .layer(Extension(limits))
        .with_state(pool)
}

async fn list_collections(State(pool): State<PgPool>, request: Request) -> Response {
    list_resources(pool, request, ListKind::Collections).await
}

async fn list_items(State(pool): State<PgPool>, request: Request) -> Response {
    list_resources(pool, request, ListKind::Items).await
}

async fn list_resources(pool: PgPool, request: Request, kind: ListKind) -> Response {
    let request_id = new_request_id();
    let prepared = match prepare_list(&pool, request).await {
        Ok(prepared) => prepared,
        Err(rejection) => {
            return finish_list_error(
                &pool,
                request_id,
                rejection.error,
                rejection.digest.as_deref(),
            )
            .await;
        }
    };
    match list_resources_from_database(&pool, &prepared, &request_id, kind).await {
        Ok(list) => Json(list).into_response(),
        Err(error) => finish_list_error(&pool, request_id, error, Some(&prepared.digest)).await,
    }
}

async fn prepare_list(pool: &PgPool, request: Request) -> Result<PreparedList, RequestRejection> {
    let Some(token) = bearer_token(&request) else {
        return Err(RequestRejection::unattributed(
            RequestFailure::Unauthenticated,
        ));
    };
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let reader = match tokio::time::timeout(
        AUTHENTICATION_TIMEOUT,
        authenticate_reader(pool, &digest, "list"),
    )
    .await
    {
        Ok(Ok(reader)) => reader,
        Ok(Err(error)) => return Err(RequestRejection::attributed(error, digest)),
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::AuthenticationTimeout { source },
                digest,
            ));
        }
    };
    let query = match request.uri().query() {
        Some(query) if query.len() <= 600 => Some(query.to_owned()),
        Some(_) => {
            return Err(RequestRejection::attributed(
                RequestFailure::Malformed,
                digest,
            ));
        }
        None => None,
    };
    match tokio::time::timeout(EMPTY_BODY_TIMEOUT, to_bytes(request.into_body(), 0)).await {
        Ok(Ok(body)) if body.is_empty() => {}
        Ok(Ok(_)) => {
            return Err(RequestRejection::attributed(
                RequestFailure::Malformed,
                digest,
            ));
        }
        Ok(Err(source)) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyRead { source },
                digest,
            ));
        }
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyTimeout { source },
                digest,
            ));
        }
    }
    let (limit, cursor) = parse_list_query(query.as_deref())
        .ok_or_else(|| RequestRejection::attributed(RequestFailure::Malformed, digest.clone()))?;
    Ok(PreparedList {
        digest,
        reader,
        limit,
        cursor,
    })
}

fn bearer_token(request: &Request) -> Option<&str> {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| (32..=128).contains(&value.len()) && value.is_ascii())
}

fn parse_list_query(query: Option<&str>) -> Option<(i32, Option<String>)> {
    let mut limit = None;
    let mut cursor = None;
    if let Some(query) = query {
        if query.is_empty() {
            return None;
        }
        for parameter in query.split('&') {
            let (name, value) = parameter.split_once('=')?;
            match name {
                "limit" if limit.is_none() => {
                    let parsed = value.parse::<i32>().ok()?;
                    if !(1..=100).contains(&parsed) {
                        return None;
                    }
                    limit = Some(parsed);
                }
                "cursor"
                    if cursor.is_none()
                        && value.len() == 32
                        && value.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
                {
                    cursor = Some(value.to_ascii_lowercase());
                }
                _ => return None,
            }
        }
    }
    Some((limit.unwrap_or(20), cursor))
}

async fn search_memories(
    State(pool): State<PgPool>,
    Extension(limits): Extension<SearchLimits>,
    request: Request,
) -> Response {
    let request_id = new_request_id();
    let prepared = match prepare_search(&pool, request, limits).await {
        Ok(prepared) => prepared,
        Err(rejection) => {
            return finish_search_error(
                &pool,
                request_id,
                rejection.error,
                rejection.digest.as_deref(),
            )
            .await;
        }
    };
    let semantic = if prepared.input.semantic {
        semantic_plan(
            &pool,
            &prepared.digest,
            &prepared.reader,
            &prepared.input.query,
            prepared
                .input
                .scope
                .0
                .as_ref()
                .map(|scope| scope.subjects.as_slice()),
            prepared.input.semantic_query.as_ref(),
        )
        .await
    } else {
        SemanticPlan::NotRequested
    };
    let first = search_memories_in_database(
        &pool,
        &prepared.digest,
        &prepared.reader,
        &prepared.input,
        &request_id,
        &semantic,
    )
    .await;
    let result = if first.is_err() && matches!(semantic, SemanticPlan::Ready { .. }) {
        search_memories_in_database(
            &pool,
            &prepared.digest,
            &prepared.reader,
            &prepared.input,
            &request_id,
            &SemanticPlan::Pending("semantic_unavailable"),
        )
        .await
    } else {
        first
    };
    match result {
        Ok(found) => Json(found).into_response(),
        Err(error) => finish_search_error(&pool, request_id, error, Some(&prepared.digest)).await,
    }
}

async fn prepare_search(
    pool: &PgPool,
    request: Request,
    limits: SearchLimits,
) -> Result<PreparedSearch, RequestRejection> {
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| (32..=128).contains(&value.len()) && value.is_ascii())
    else {
        return Err(RequestRejection::unattributed(
            RequestFailure::Unauthenticated,
        ));
    };
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let reader = match tokio::time::timeout(
        AUTHENTICATION_TIMEOUT,
        authenticate_reader(pool, &digest, "search"),
    )
    .await
    {
        Ok(Ok(reader)) => reader,
        Ok(Err(error)) => return Err(RequestRejection::attributed(error, digest)),
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::AuthenticationTimeout { source },
                digest,
            ));
        }
    };
    if request.uri().query().is_some()
        || !request
            .headers()
            .get("content-type")
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"application/json"))
    {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    }
    let body = match tokio::time::timeout(
        CREATE_BODY_TIMEOUT,
        to_bytes(request.into_body(), SEARCH_BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(source)) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyRead { source },
                digest,
            ));
        }
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyTimeout { source },
                digest,
            ));
        }
    };
    if !bounded_json_structure_with_limit(&body, MAX_EXTERNAL_VECTOR_DIMENSIONS + 32) {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    }
    let mut input: SearchInput = serde_json::from_slice(&body).map_err(|source| {
        RequestRejection::attributed(RequestFailure::JsonBody { source }, digest.clone())
    })?;
    input
        .max_context_bytes
        .get_or_insert(limits.default_context_bytes);
    validate_search_input_with_limits(&input, limits)
        .map_err(|error| RequestRejection::attributed(error, digest.clone()))?;
    Ok(PreparedSearch {
        digest,
        reader,
        input,
    })
}

#[cfg(test)]
fn validate_search_input(input: &SearchInput) -> Result<(), RequestFailure> {
    validate_search_input_with_limits(input, SearchLimits::default())
}

fn validate_search_input_with_limits(
    input: &SearchInput,
    limits: SearchLimits,
) -> Result<(), RequestFailure> {
    if !(1..=4096).contains(&input.query.len())
        || input.query.trim().is_empty()
        || input.query.contains('\0')
        || !(1..=limits.max_context_bytes).contains(
            &input
                .max_context_bytes
                .unwrap_or(limits.default_context_bytes),
        )
        || input.scope.0.as_ref().is_some_and(|scope| {
            !(1..=8).contains(&scope.subjects.len())
                || scope
                    .subjects
                    .iter()
                    .any(|subject| !valid_opaque_id(subject))
                || scope
                    .subjects
                    .iter()
                    .enumerate()
                    .any(|(index, subject)| scope.subjects[..index].contains(subject))
        })
    {
        return Err(RequestFailure::Malformed);
    }
    if input.query_mode != QueryMode::BoundedV1
        && (input.semantic || input.semantic_query.is_some())
    {
        return Err(RequestFailure::Malformed);
    }
    if input.time_mode != TimeMode::Current {
        return Err(RequestFailure::UnsupportedTimeSemantics);
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Query preparation, authorization and response packing share one transaction.
async fn search_memories_in_database(
    pool: &PgPool,
    digest: &[u8],
    reader: &ReaderContext,
    input: &SearchInput,
    request_id: &str,
    semantic: &SemanticPlan,
) -> Result<SearchResponse, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", digest)
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "app.operation", "search")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "1000ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "app.tenant_id", &reader.tenant_id)
        .await
        .map_err(RequestFailure::storage)?;
    let effective_semantic = if let SemanticPlan::Ready { generation, .. } = semantic {
        let row = sqlx::query(
            "SELECT generation_id,semantic_status,diagnostic
             FROM semantic_readiness($1,$2,$3)",
        )
        .bind(&reader.tenant_id)
        .bind(&reader.credential_id)
        .bind(input.scope.0.as_ref().map(|scope| &scope.subjects))
        .fetch_one(&mut *transaction)
        .await
        .map_err(classify_list_database_error)?;
        let current_generation = row
            .try_get::<Option<String>, _>("generation_id")
            .map_err(RequestFailure::storage)?;
        let status = row
            .try_get::<String, _>("semantic_status")
            .map_err(RequestFailure::storage)?;
        let diagnostic = row
            .try_get::<String, _>("diagnostic")
            .map_err(RequestFailure::storage)?;
        if status == "ready" && current_generation.as_ref() == Some(generation) {
            semantic.clone()
        } else if status == "failed" {
            SemanticPlan::Failed(sanitized_semantic_diagnostic(&diagnostic))
        } else {
            SemanticPlan::Pending(sanitized_semantic_diagnostic(&diagnostic))
        }
    } else {
        semantic.clone()
    };
    #[cfg(feature = "benchmark-trace")]
    if benchmark_trace::enabled() {
        set_local(&mut transaction, "app.benchmark_search_trace", "enabled")
            .await
            .map_err(RequestFailure::storage)?;
        set_local(&mut transaction, "app.benchmark_search_trace_rows", "")
            .await
            .map_err(RequestFailure::storage)?;
    }
    let query = match &effective_semantic {
        SemanticPlan::Ready { .. } => {
            "SELECT item_id, revision_id, content, recorded_at,
                valid_from, valid_until, validity_status, hit_kind,
                source_revision_id, extraction_set_id, passage_id, locator,
                parent_passage_ids, candidate_omitted
         FROM search_hybrid_current_memories($1,$2,$3,$4,$5,$6,$7,$8)"
        }
        _ if input.query_mode == QueryMode::DisjunctiveV2 => {
            "SELECT item_id, revision_id, content, recorded_at,
                valid_from, valid_until, validity_status, hit_kind,
                source_revision_id, extraction_set_id, passage_id, locator,
                parent_passage_ids, candidate_omitted
         FROM search_current_memories_disjunctive_v2($1,$2,$3,$4,$5,$6)"
        }
        _ if input.query_mode == QueryMode::DisjunctiveV3 => {
            "SELECT item_id, revision_id, content, recorded_at,
                valid_from, valid_until, validity_status, hit_kind,
                source_revision_id, extraction_set_id, passage_id, locator,
                parent_passage_ids, candidate_omitted
         FROM search_current_memories_disjunctive_v3($1,$2,$3,$4,$5,$6)"
        }
        _ => {
            "SELECT item_id, revision_id, content, recorded_at,
                valid_from, valid_until, validity_status, hit_kind,
                source_revision_id, extraction_set_id, passage_id, locator,
                parent_passage_ids, candidate_omitted
         FROM search_current_memories($1,$2,$3,$4,$5,$6)"
        }
    };
    let mut database_query = sqlx::query(query)
        .bind(&reader.tenant_id)
        .bind(&reader.credential_id)
        .bind(request_id)
        .bind(&input.query)
        .bind(i32::from(
            input.max_context_bytes.expect("prepared search budget"),
        ))
        .bind(
            input
                .scope
                .0
                .as_ref()
                .map(|scope| scope.subjects.as_slice()),
        );
    if let SemanticPlan::Ready { generation, vector } = &effective_semantic {
        database_query = database_query.bind(generation).bind(vector);
    }
    let rows = database_query
        .fetch_all(&mut *transaction)
        .await
        .map_err(classify_list_database_error)?;
    #[cfg(feature = "benchmark-trace")]
    if benchmark_trace::enabled() {
        let observed: Option<String> =
            sqlx::query_scalar("SELECT current_setting('app.benchmark_search_trace_rows',true)")
                .fetch_one(&mut *transaction)
                .await
                .map_err(RequestFailure::storage)?;
        benchmark_trace::record(serde_json::json!({"stage":"sql_candidates","observed":observed}));
        for (ordinal, row) in rows.iter().enumerate() {
            benchmark_trace::row(
                row,
                ordinal,
                "sql_returned",
                usize::from(input.max_context_bytes.expect("prepared search budget")),
                0,
            );
        }
    }
    let no_candidates = rows.is_empty();
    let semantic_status = effective_semantic.status();
    let (items, context_bytes, truncated) = pack_search_rows(&rows, input, semantic_status)?;
    let mut warnings = effective_semantic.diagnostics(no_candidates);
    if input.query_mode != QueryMode::BoundedV1 {
        let status: String =
            sqlx::query_scalar("SELECT current_setting('app.lexical_preparation_status',true)")
                .fetch_one(&mut *transaction)
                .await
                .map_err(RequestFailure::storage)?;
        match status.as_str() {
            "disjunctive" | "explicit_syntax" => {}
            "fallback_bound" | "fallback_curly" | "fallback_empty" | "fallback_roundtrip" => {
                warnings.push("lexical_disjunction_fallback");
            }
            _ => return Err(RequestFailure::Malformed),
        }
    }
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    Ok(SearchResponse {
        request_id: request_id.to_owned(),
        status: "ready",
        items,
        context_bytes,
        truncated,
        warnings,
    })
}

async fn semantic_plan(
    pool: &PgPool,
    digest: &[u8],
    reader: &ReaderContext,
    query: &str,
    scope_subjects: Option<&[String]>,
    external: Option<&SemanticQueryInput>,
) -> SemanticPlan {
    let result = tokio::time::timeout(Duration::from_millis(150), async {
        let mut tx = pool.begin().await?;
        set_local_bytes(&mut tx, "app.credential_digest", digest).await?;
        set_local(&mut tx, "app.operation", "search").await?;
        set_local(&mut tx, "statement_timeout", "100ms").await?;
        set_local(&mut tx, "app.tenant_id", &reader.tenant_id).await?;
        let row = sqlx::query(
            "SELECT generation_id,model_version,input_recipe_version,dimensions,
                    semantic_status,diagnostic
             FROM semantic_readiness($1,$2,$3)",
        )
        .bind(&reader.tenant_id)
        .bind(&reader.credential_id)
        .bind(scope_subjects)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>((
            row.try_get::<Option<String>, _>("generation_id")?,
            row.try_get::<Option<String>, _>("model_version")?,
            row.try_get::<Option<String>, _>("input_recipe_version")?,
            row.try_get::<Option<i32>, _>("dimensions")?,
            row.try_get::<String, _>("semantic_status")?,
            row.try_get::<String, _>("diagnostic")?,
        ))
    })
    .await;
    match result {
        Ok(Ok((Some(generation), Some(model), Some(recipe), Some(dimensions), status, _)))
            if status == "ready" =>
        {
            if let Some(external) = external {
                external_semantic_plan(generation, &model, &recipe, dimensions, query, external)
            } else {
                query_embedding_fixture(generation, &model, &recipe, dimensions, query).await
            }
        }
        Ok(Ok((_, _, _, _, status, diagnostic))) if status == "failed" => {
            SemanticPlan::Failed(sanitized_semantic_diagnostic(&diagnostic))
        }
        Ok(Ok((_, _, _, _, _, diagnostic))) => {
            SemanticPlan::Pending(sanitized_semantic_diagnostic(&diagnostic))
        }
        Ok(Err(_)) | Err(_) => SemanticPlan::Pending("semantic_database_unavailable"),
    }
}

fn external_semantic_plan(
    generation: String,
    model: &str,
    recipe: &str,
    dimensions: i32,
    query: &str,
    external: &SemanticQueryInput,
) -> SemanticPlan {
    let (
        Some(external_generation),
        Some(external_model),
        Some(external_recipe),
        Some(external_dimensions),
        Some(external_digest),
        Some(external_vector),
    ) = (
        external.generation_id.as_deref(),
        external.model_version.as_deref(),
        external.input_recipe_version.as_deref(),
        external.dimensions,
        external.original_query_sha256.as_deref(),
        external.vector.as_deref(),
    )
    else {
        return SemanticPlan::Pending("semantic_external_input_invalid");
    };
    let expected_digest = Sha256::digest(query.as_bytes()).iter().fold(
        String::with_capacity(64),
        |mut value, byte| {
            write!(value, "{byte:02x}").expect("write digest");
            value
        },
    );
    let squared_norm = external_vector.iter().fold(0.0_f64, |sum, value| {
        let component = f64::from(*value);
        sum + component * component
    });
    let valid = valid_opaque_id(external_generation)
        && (1..=128).contains(&external_model.len())
        && !external_model.contains('\0')
        && valid_opaque_id(external_recipe)
        && external_digest.len() == 64
        && external_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && external_generation == generation
        && external_model == model
        && external_recipe == recipe
        && usize::try_from(dimensions).ok() == Some(external_dimensions)
        && external_dimensions == external_vector.len()
        && (1..=MAX_EXTERNAL_VECTOR_DIMENSIONS).contains(&external_dimensions)
        && external_digest == expected_digest
        && external_vector.iter().all(|value| value.is_finite())
        && squared_norm.is_finite()
        && squared_norm > 0.0;
    if !valid {
        return SemanticPlan::Pending("semantic_external_input_invalid");
    }
    let vector =
        serde_json::to_string(external_vector).expect("finite bounded external vector serializes");
    SemanticPlan::Ready { generation, vector }
}

fn deterministic_query_vector(query: &str) -> String {
    let digest = Sha256::digest(query.as_bytes());
    format!(
        "[{},{},{}]",
        u16::from(digest[0]) + 1,
        u16::from(digest[1]) + 1,
        u16::from(digest[2]) + 1
    )
}

async fn query_embedding_fixture(
    generation: String,
    model: &str,
    recipe: &str,
    dimensions: i32,
    query: &str,
) -> SemanticPlan {
    if dimensions != 3 || recipe != "deterministic-input-v1" {
        return SemanticPlan::Pending("semantic_configuration_missing");
    }
    match model {
        "deterministic-fixture-provider-failed" => SemanticPlan::Failed("query_embedding_failed"),
        "deterministic-fixture-provider-timeout" => SemanticPlan::Failed("query_embedding_timeout"),
        "deterministic-fixture-invalid-response" => SemanticPlan::Failed("query_embedding_invalid"),
        "deterministic-fixture-delayed-test" => {
            tokio::time::sleep(Duration::from_millis(250)).await;
            SemanticPlan::Ready {
                generation,
                vector: deterministic_query_vector(query),
            }
        }
        "deterministic-fixture-v1" => SemanticPlan::Ready {
            generation,
            vector: deterministic_query_vector(query),
        },
        _ => SemanticPlan::Pending("semantic_configuration_missing"),
    }
}

fn sanitized_semantic_diagnostic(value: &str) -> &'static str {
    match value {
        "semantic_configuration_missing" => "semantic_configuration_missing",
        "extraction_not_ready" => "extraction_not_ready",
        "semantic_processing_pending" => "semantic_processing_pending",
        "semantic_processing_failed" => "semantic_processing_failed",
        "semantic_external_input_invalid" => "semantic_external_input_invalid",
        _ => "semantic_unavailable",
    }
}

#[allow(clippy::too_many_lines)] // Keep opt-in observations beside the unchanged admission branches.
fn pack_search_rows(
    rows: &[PgRow],
    input: &SearchInput,
    semantic_status: &'static str,
) -> Result<(Vec<SearchItem>, usize, bool), RequestFailure> {
    let candidate_omitted = rows
        .first()
        .map(|row| row.try_get("candidate_omitted"))
        .transpose()
        .map_err(RequestFailure::storage)?
        .unwrap_or(false);
    let mut budget = ExcerptBudget::new(usize::from(
        input.max_context_bytes.expect("prepared search budget"),
    ));
    let mut items = Vec::with_capacity(rows.len().min(12));
    let mut source_contributions = HashMap::new();
    let mut accepted_passages = HashSet::new();
    let mut accepted_direct_passages = HashSet::new();
    let mut source_contribution_omitted = false;
    for (ordinal, row) in rows.iter().enumerate() {
        #[cfg(not(feature = "benchmark-trace"))]
        let _ = ordinal;
        if items.len() == 12 {
            #[cfg(feature = "benchmark-trace")]
            benchmark_trace::tail(
                rows,
                ordinal,
                "unvisited_item_limit",
                budget.remaining,
                items.len(),
            );
            break;
        }
        let hit_kind: String = row.try_get("hit_kind").map_err(RequestFailure::storage)?;
        let passage_hit = matches!(
            hit_kind.as_str(),
            "passage" | "semantic_passage" | "adjacent_continuation"
        );
        let source_key = passage_source_key(row, passage_hit)?;
        let passage_identity = source_key
            .as_ref()
            .map(|key| {
                Ok::<_, RequestFailure>((
                    key.clone(),
                    row.try_get::<String, _>("passage_id")
                        .map_err(RequestFailure::storage)?,
                ))
            })
            .transpose()?;
        if hit_kind == "adjacent_continuation" {
            let parent_passage_ids = row
                .try_get::<Vec<String>, _>("parent_passage_ids")
                .map_err(RequestFailure::storage)?;
            let source_key = source_key.clone().expect("passage source key");
            if !parent_passage_ids.iter().any(|parent_passage_id| {
                accepted_direct_passages.contains(&(source_key.clone(), parent_passage_id.clone()))
            }) {
                #[cfg(feature = "benchmark-trace")]
                benchmark_trace::row(
                    row,
                    ordinal,
                    "parent_not_accepted",
                    budget.remaining,
                    items.len(),
                );
                source_contribution_omitted = true;
                continue;
            }
        }
        if passage_identity
            .as_ref()
            .is_some_and(|identity| accepted_passages.contains(identity))
        {
            #[cfg(feature = "benchmark-trace")]
            benchmark_trace::row(
                row,
                ordinal,
                "duplicate_passage",
                budget.remaining,
                items.len(),
            );
            source_contribution_omitted = true;
            continue;
        }
        if source_key
            .as_ref()
            .is_some_and(|key| source_contributions.get(key).copied().unwrap_or(0) == 4)
        {
            #[cfg(feature = "benchmark-trace")]
            benchmark_trace::row(row, ordinal, "source_limit", budget.remaining, items.len());
            source_contribution_omitted = true;
            continue;
        }
        let Some(item) = search_item_row(row, &mut budget, semantic_status)? else {
            #[cfg(feature = "benchmark-trace")]
            benchmark_trace::row(row, ordinal, "byte_budget", budget.remaining, items.len());
            if budget.remaining > 0 {
                continue;
            }
            #[cfg(feature = "benchmark-trace")]
            benchmark_trace::tail(
                rows,
                ordinal + 1,
                "unvisited_byte_stop",
                budget.remaining,
                items.len(),
            );
            break;
        };
        if let Some(key) = source_key {
            *source_contributions.entry(key).or_insert(0) += 1;
        }
        if let Some(identity) = passage_identity {
            accepted_passages.insert(identity.clone());
            if hit_kind == "passage" {
                accepted_direct_passages.insert(identity);
            }
        }
        #[cfg(feature = "benchmark-trace")]
        benchmark_trace::row(row, ordinal, "accepted", budget.remaining, items.len() + 1);
        items.push(item);
    }
    let truncated = candidate_omitted
        || source_contribution_omitted
        || budget.truncated
        || rows.len() > items.len();
    let context_bytes =
        usize::from(input.max_context_bytes.expect("prepared search budget")) - budget.remaining;
    Ok((items, context_bytes, truncated))
}

type PassageSourceKey = (String, String, String, String);

fn passage_source_key(
    row: &PgRow,
    passage_hit: bool,
) -> Result<Option<PassageSourceKey>, RequestFailure> {
    if !passage_hit {
        return Ok(None);
    }
    Ok(Some((
        row.try_get("item_id").map_err(RequestFailure::storage)?,
        row.try_get("revision_id")
            .map_err(RequestFailure::storage)?,
        row.try_get("source_revision_id")
            .map_err(RequestFailure::storage)?,
        row.try_get("extraction_set_id")
            .map_err(RequestFailure::storage)?,
    )))
}

async fn list_resources_from_database(
    pool: &PgPool,
    prepared: &PreparedList,
    request_id: &str,
    kind: ListKind,
) -> Result<ListResponse, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", &prepared.digest)
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "app.operation", "list")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "1000ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(
        &mut transaction,
        "app.tenant_id",
        &prepared.reader.tenant_id,
    )
    .await
    .map_err(RequestFailure::storage)?;
    let rows = sqlx::query(
        "SELECT resource_id, revision_id, collection_id, audience_kind,
                recorded_at, valid_from, valid_until, validity_status, next_cursor
         FROM list_current_resources($1, $2, $3, $4, $5, $6)",
    )
    .bind(&prepared.reader.tenant_id)
    .bind(&prepared.reader.credential_id)
    .bind(request_id)
    .bind(kind.as_str())
    .bind(prepared.limit)
    .bind(prepared.cursor.as_deref())
    .fetch_all(&mut *transaction)
    .await
    .map_err(classify_list_database_error)?;
    let next_cursor = rows
        .first()
        .map(|row| row.try_get("next_cursor"))
        .transpose()
        .map_err(RequestFailure::storage)?
        .flatten();
    let entries = rows
        .iter()
        .map(|row| list_entry(row, kind))
        .collect::<Result<Vec<_>, _>>()?;
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    Ok(ListResponse {
        request_id: request_id.to_owned(),
        status: "ready",
        items: entries,
        truncated: next_cursor.is_some(),
        next_cursor,
    })
}

fn list_entry(row: &PgRow, kind: ListKind) -> Result<ListEntry, RequestFailure> {
    match kind {
        ListKind::Collections => Ok(ListEntry::Collection(CollectionListItem {
            collection_id: row
                .try_get("resource_id")
                .map_err(RequestFailure::storage)?,
            audience_kind: row
                .try_get("audience_kind")
                .map_err(RequestFailure::storage)?,
        })),
        ListKind::Items => Ok(ListEntry::Item(ItemListItem {
            item_id: row
                .try_get("resource_id")
                .map_err(RequestFailure::storage)?,
            revision_id: row
                .try_get("revision_id")
                .map_err(RequestFailure::storage)?,
            collection_id: row
                .try_get("collection_id")
                .map_err(RequestFailure::storage)?,
            recorded_at: row
                .try_get("recorded_at")
                .map_err(RequestFailure::storage)?,
            valid_from: row.try_get("valid_from").map_err(RequestFailure::storage)?,
            valid_until: row
                .try_get("valid_until")
                .map_err(RequestFailure::storage)?,
            validity_status: row
                .try_get("validity_status")
                .map_err(RequestFailure::storage)?,
        })),
    }
}

fn search_item_row(
    row: &PgRow,
    budget: &mut ExcerptBudget,
    semantic_status: &'static str,
) -> Result<Option<SearchItem>, RequestFailure> {
    let content: String = row.try_get("content").map_err(RequestFailure::storage)?;
    let hit_kind: String = row.try_get("hit_kind").map_err(RequestFailure::storage)?;
    let passage_hit = matches!(
        hit_kind.as_str(),
        "passage" | "semantic_passage" | "adjacent_continuation"
    );
    let Some(excerpt) = (if passage_hit {
        budget.take_whole(&content)
    } else {
        budget.take(&content)
    }) else {
        return Ok(None);
    };
    let citation = if passage_hit {
        let locator = serde_json::from_str(
            &row.try_get::<String, _>("locator")
                .map_err(RequestFailure::storage)?,
        )
        .map_err(|source| RequestFailure::StoredJson { source })?;
        Some(SearchCitation {
            source_revision_id: row
                .try_get("source_revision_id")
                .map_err(RequestFailure::storage)?,
            extraction_set_id: row
                .try_get("extraction_set_id")
                .map_err(RequestFailure::storage)?,
            passage_id: row.try_get("passage_id").map_err(RequestFailure::storage)?,
            locator,
        })
    } else {
        None
    };
    Ok(Some(SearchItem {
        item_id: row.try_get("item_id").map_err(RequestFailure::storage)?,
        revision_id: row
            .try_get("revision_id")
            .map_err(RequestFailure::storage)?,
        excerpt,
        recorded_at: row
            .try_get("recorded_at")
            .map_err(RequestFailure::storage)?,
        valid_from: row.try_get("valid_from").map_err(RequestFailure::storage)?,
        valid_until: row
            .try_get("valid_until")
            .map_err(RequestFailure::storage)?,
        validity_status: row
            .try_get("validity_status")
            .map_err(RequestFailure::storage)?,
        reason: match hit_kind.as_str() {
            "adjacent_continuation" => "adjacent_continuation",
            "semantic_memory" | "semantic_passage" => "semantic",
            _ => "lexical",
        },
        semantic_status,
        citation,
    }))
}

struct ExcerptBudget {
    remaining: usize,
    truncated: bool,
}

impl ExcerptBudget {
    const fn new(limit: usize) -> Self {
        Self {
            remaining: limit,
            truncated: false,
        }
    }

    fn take(&mut self, content: &str) -> Option<String> {
        if self.remaining == 0 {
            self.truncated = true;
            return None;
        }
        let excerpt = bounded_excerpt(content, self.remaining.min(512));
        if excerpt.is_empty() && !content.is_empty() {
            self.truncated = true;
            return None;
        }
        self.remaining -= excerpt.len();
        self.truncated |= excerpt.len() < content.len();
        Some(excerpt)
    }

    fn take_whole(&mut self, content: &str) -> Option<String> {
        #[cfg(feature = "benchmark-trace")]
        benchmark_trace::record(
            serde_json::json!({"stage":"whole_passage_budget","content_bytes":content.len(),"remaining_before":self.remaining,"accepted":content.len()<=self.remaining}),
        );
        if content.len() > self.remaining {
            self.truncated = true;
            return None;
        }
        self.remaining -= content.len();
        Some(content.to_owned())
    }
}

fn bounded_excerpt(content: &str, max_bytes: usize) -> String {
    let mut end = content.len().min(max_bytes);
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content[..end].to_owned()
}

async fn create_memory(State(pool): State<PgPool>, request: Request) -> Response {
    let request_id = new_request_id();
    let prepared = match prepare_create(&pool, request).await {
        Ok(prepared) => prepared,
        Err(rejection) => {
            return finish_create_error(
                &pool,
                request_id,
                rejection.error,
                rejection.digest.as_deref(),
                None,
            )
            .await;
        }
    };
    match create_memory_in_database(
        &pool,
        &prepared.digest,
        &prepared.writer,
        &request_id,
        &prepared.key_digest,
        &prepared.request_digest,
        &prepared.input,
    )
    .await
    {
        Ok(created) => (
            if created.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            Json(created),
        )
            .into_response(),
        Err(error) => {
            finish_create_error(
                &pool,
                request_id,
                error,
                Some(&prepared.digest),
                Some(&prepared.key_digest),
            )
            .await
        }
    }
}

async fn prepare_create(
    pool: &PgPool,
    request: Request,
) -> Result<PreparedCreate, RequestRejection> {
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| (32..=128).contains(&value.len()) && value.is_ascii())
    else {
        return Err(RequestRejection::unattributed(
            RequestFailure::Unauthenticated,
        ));
    };
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let writer = match tokio::time::timeout(
        AUTHENTICATION_TIMEOUT,
        authenticate_writer(pool, &digest, "create"),
    )
    .await
    {
        Ok(Ok(writer)) => writer,
        Ok(Err(error)) => return Err(RequestRejection::attributed(error, digest)),
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::AuthenticationTimeout { source },
                digest,
            ));
        }
    };

    if request.uri().query().is_some()
        || !request
            .headers()
            .get("content-type")
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"application/json"))
    {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    }
    let Some(idempotency_key) = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            (16..=128).contains(&value.len())
                && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        })
        .map(str::to_owned)
    else {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    };
    let key_digest = Sha256::digest(idempotency_key.as_bytes()).to_vec();

    let body = match tokio::time::timeout(
        CREATE_BODY_TIMEOUT,
        to_bytes(request.into_body(), CREATE_BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(source)) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyRead { source },
                digest,
            ));
        }
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyTimeout { source },
                digest,
            ));
        }
    };
    let (input, request_digest) = parse_create_body(&body)
        .map_err(|error| RequestRejection::attributed(error, digest.clone()))?;
    Ok(PreparedCreate {
        digest,
        writer,
        key_digest,
        request_digest,
        input,
    })
}

fn parse_create_body(body: &[u8]) -> Result<(CreateMemoryInput, Vec<u8>), RequestFailure> {
    if !bounded_json_structure(body) {
        return Err(RequestFailure::Malformed);
    }
    let input: CreateMemoryInput =
        serde_json::from_slice(body).map_err(|source| RequestFailure::JsonBody { source })?;
    if !(1..=32 * 1024).contains(&input.content.len())
        || input.content.contains('\0')
        || !(1..=8).contains(&input.subjects.len())
        || input
            .subjects
            .iter()
            .any(|subject| !valid_opaque_id(subject))
        || input
            .subjects
            .iter()
            .enumerate()
            .any(|(index, subject)| input.subjects[..index].contains(subject))
    {
        return Err(RequestFailure::Malformed);
    }
    let canonical_body =
        serde_json::to_vec(&input).map_err(|source| RequestFailure::JsonBody { source })?;
    let request_digest = canonical_request_digest(&canonical_body);
    Ok((input, request_digest))
}

async fn authenticate_writer(
    pool: &PgPool,
    digest: &[u8],
    operation: &str,
) -> Result<WriterContext, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "100ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", digest)
        .await
        .map_err(RequestFailure::storage)?;
    let row = sqlx::query("SELECT tenant_id, credential_id FROM resolve_current_writer($1)")
        .bind(operation)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(RequestFailure::storage)?
        .ok_or(RequestFailure::Unauthenticated)?;
    let writer = WriterContext {
        tenant_id: row.try_get("tenant_id").map_err(RequestFailure::storage)?,
        credential_id: row
            .try_get("credential_id")
            .map_err(RequestFailure::storage)?,
    };
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    Ok(writer)
}

async fn correct_memory(
    State(pool): State<PgPool>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let request_id = new_request_id();
    let prepared = match prepare_correct(&pool, path, request).await {
        Ok(prepared) => prepared,
        Err(rejection) => {
            return finish_correct_error(
                &pool,
                request_id,
                rejection.error,
                rejection.digest.as_deref(),
                None,
            )
            .await;
        }
    };
    match correct_memory_in_database(
        &pool,
        &prepared.digest,
        &prepared.writer,
        &request_id,
        &prepared.key_digest,
        &prepared.request_digest,
        &prepared.item_id,
        &prepared.input,
    )
    .await
    {
        Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
        Err(error) => {
            finish_correct_error(
                &pool,
                request_id,
                error,
                Some(&prepared.digest),
                Some(&prepared.key_digest),
            )
            .await
        }
    }
}

async fn prepare_correct(
    pool: &PgPool,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<PreparedCorrect, RequestRejection> {
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| (32..=128).contains(&v.len()) && v.is_ascii())
    else {
        return Err(RequestRejection::unattributed(
            RequestFailure::Unauthenticated,
        ));
    };
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let writer = match tokio::time::timeout(
        AUTHENTICATION_TIMEOUT,
        authenticate_writer(pool, &digest, "correct"),
    )
    .await
    {
        Ok(Ok(writer)) => writer,
        Ok(Err(error)) => return Err(RequestRejection::attributed(error, digest)),
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::AuthenticationTimeout { source },
                digest,
            ));
        }
    };
    let Ok(Path(item_id)) = path else {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    };
    if request.uri().query().is_some()
        || !valid_opaque_id(&item_id)
        || !has_json_content_type(&request)
    {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    }
    let Some(key) = request
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| (16..=128).contains(&v.len()) && v.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        .map(str::to_owned)
    else {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    };
    let body = match tokio::time::timeout(
        CREATE_BODY_TIMEOUT,
        to_bytes(request.into_body(), CREATE_BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(source)) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyRead { source },
                digest,
            ));
        }
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyTimeout { source },
                digest,
            ));
        }
    };
    let input: CorrectMemoryInput = match parse_correct_body(&body) {
        Ok(input) => input,
        Err(error) => return Err(RequestRejection::attributed(error, digest)),
    };
    let key_digest = Sha256::digest(key.as_bytes()).to_vec();
    let canonical = match serde_json::to_vec(&input) {
        Ok(body) => body,
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::JsonBody { source },
                digest,
            ));
        }
    };
    let request_digest =
        mutation_request_digest("PUT", &format!("/v1/items/{item_id}"), &canonical);
    Ok(PreparedCorrect {
        digest,
        writer,
        key_digest,
        request_digest,
        item_id,
        input,
    })
}

fn has_json_content_type(request: &Request) -> bool {
    request
        .headers()
        .get("content-type")
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"application/json"))
}

fn parse_correct_body(body: &[u8]) -> Result<CorrectMemoryInput, RequestFailure> {
    if !bounded_json_structure(body) {
        return Err(RequestFailure::Malformed);
    }
    let input: CorrectMemoryInput =
        serde_json::from_slice(body).map_err(|source| RequestFailure::JsonBody { source })?;
    if !valid_opaque_id(&input.expected_revision_id)
        || !(1..=32 * 1024).contains(&input.content.len())
        || input.content.contains('\0')
        || !(1..=8).contains(&input.subjects.len())
        || input.subjects.iter().any(|s| !valid_opaque_id(s))
        || input
            .subjects
            .iter()
            .enumerate()
            .any(|(i, s)| input.subjects[..i].contains(s))
    {
        return Err(RequestFailure::Malformed);
    }
    Ok(input)
}

#[allow(clippy::too_many_arguments)]
async fn correct_memory_in_database(
    pool: &PgPool,
    digest: &[u8],
    writer: &WriterContext,
    request_id: &str,
    key_digest: &[u8],
    request_digest: &[u8],
    item_id: &str,
    input: &CorrectMemoryInput,
) -> Result<CreateMemory, RequestFailure> {
    let mut tx = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local_bytes(&mut tx, "app.credential_digest", digest)
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut tx, "lock_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut tx, "statement_timeout", "1000ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut tx, "app.tenant_id", &writer.tenant_id)
        .await
        .map_err(RequestFailure::storage)?;
    let row = sqlx::query("SELECT item_id, revision_id, operation_id, completed_at, replayed FROM correct_private_memory($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
        .bind(&writer.tenant_id).bind(&writer.credential_id).bind(request_id)
        .bind(key_digest).bind(request_digest).bind(item_id)
        .bind(&input.expected_revision_id).bind(&input.content).bind(&input.subjects)
        .bind(input.valid_from.as_deref()).bind(input.valid_until.as_deref())
        .fetch_one(&mut *tx).await.map_err(classify_create_database_error)?;
    let replayed: bool = row.try_get("replayed").map_err(RequestFailure::storage)?;
    let item_id: Option<String> = row.try_get("item_id").map_err(RequestFailure::storage)?;
    let revision_id: Option<String> = row
        .try_get("revision_id")
        .map_err(RequestFailure::storage)?;
    let forgotten = item_id.is_none();
    let receipt = CreateMemory {
        request_id: (!forgotten).then(|| request_id.to_owned()),
        status: (!forgotten).then_some("ready"),
        operation: forgotten.then_some("correct"),
        item_id,
        revision_id,
        operation_id: row
            .try_get("operation_id")
            .map_err(RequestFailure::storage)?,
        completed_at: row
            .try_get("completed_at")
            .map_err(RequestFailure::storage)?,
        replayed,
        lexical_status: (!replayed).then_some("ready"),
        semantic_status: (!replayed).then_some("not_requested"),
    };
    tx.commit().await.map_err(RequestFailure::storage)?;
    Ok(receipt)
}

async fn forget_memory(
    State(pool): State<PgPool>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let request_id = new_request_id();
    let prepared = match prepare_forget(&pool, path, request).await {
        Ok(prepared) => prepared,
        Err(rejection) => {
            return finish_forget_error(
                &pool,
                request_id,
                rejection.error,
                rejection.digest.as_deref(),
                None,
            )
            .await;
        }
    };
    match forget_memory_in_database(&pool, &request_id, &prepared).await {
        Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
        Err(error) => {
            finish_forget_error(
                &pool,
                request_id,
                error,
                Some(&prepared.digest),
                Some(&prepared.key_digest),
            )
            .await
        }
    }
}

async fn prepare_forget(
    pool: &PgPool,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<PreparedForget, RequestRejection> {
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| (32..=128).contains(&value.len()) && value.is_ascii())
    else {
        return Err(RequestRejection::unattributed(
            RequestFailure::Unauthenticated,
        ));
    };
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let writer = match tokio::time::timeout(
        AUTHENTICATION_TIMEOUT,
        authenticate_writer(pool, &digest, "forget"),
    )
    .await
    {
        Ok(Ok(writer)) => writer,
        Ok(Err(error)) => return Err(RequestRejection::attributed(error, digest)),
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::AuthenticationTimeout { source },
                digest,
            ));
        }
    };
    let Ok(Path(item_id)) = path else {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    };
    if request.uri().query().is_some()
        || !valid_opaque_id(&item_id)
        || !has_json_content_type(&request)
    {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    }
    let Some(key) = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            (16..=128).contains(&value.len())
                && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        })
    else {
        return Err(RequestRejection::attributed(
            RequestFailure::Malformed,
            digest,
        ));
    };
    let key_digest = Sha256::digest(key.as_bytes()).to_vec();
    let body = match tokio::time::timeout(
        CREATE_BODY_TIMEOUT,
        to_bytes(request.into_body(), CREATE_BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(source)) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyRead { source },
                digest,
            ));
        }
        Err(source) => {
            return Err(RequestRejection::attributed(
                RequestFailure::BodyTimeout { source },
                digest,
            ));
        }
    };
    let (input, canonical) = parse_forget_body(&body)
        .map_err(|error| RequestRejection::attributed(error, digest.clone()))?;
    let request_digest =
        mutation_request_digest("DELETE", &format!("/v1/items/{item_id}"), &canonical);
    Ok(PreparedForget {
        digest,
        writer,
        key_digest,
        request_digest,
        item_id,
        input,
    })
}

fn parse_forget_body(body: &[u8]) -> Result<(ForgetMemoryInput, Vec<u8>), RequestFailure> {
    if !bounded_json_structure(body) {
        return Err(RequestFailure::Malformed);
    }
    let input: ForgetMemoryInput =
        serde_json::from_slice(body).map_err(|source| RequestFailure::JsonBody { source })?;
    if !valid_opaque_id(&input.expected_revision_id) {
        return Err(RequestFailure::Malformed);
    }
    let canonical =
        serde_json::to_vec(&input).map_err(|source| RequestFailure::JsonBody { source })?;
    Ok((input, canonical))
}

async fn forget_memory_in_database(
    pool: &PgPool,
    request_id: &str,
    prepared: &PreparedForget,
) -> Result<ForgetMemory, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", &prepared.digest)
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "1000ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(
        &mut transaction,
        "app.tenant_id",
        &prepared.writer.tenant_id,
    )
    .await
    .map_err(RequestFailure::storage)?;
    let row = sqlx::query(
        "SELECT operation_id, completed_at, replayed, purge_state, deletion_generation
         FROM forget_private_memory($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(&prepared.writer.tenant_id)
    .bind(&prepared.writer.credential_id)
    .bind(request_id)
    .bind(&prepared.key_digest)
    .bind(&prepared.request_digest)
    .bind(&prepared.item_id)
    .bind(&prepared.input.expected_revision_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(classify_create_database_error)?;
    let replayed: bool = row.try_get("replayed").map_err(RequestFailure::storage)?;
    let receipt = ForgetMemory {
        request_id: (!replayed).then(|| request_id.to_owned()),
        status: (!replayed)
            .then(|| row.try_get::<String, _>("purge_state"))
            .transpose()
            .map_err(RequestFailure::storage)?,
        operation: "forget",
        operation_id: row
            .try_get("operation_id")
            .map_err(RequestFailure::storage)?,
        completed_at: row
            .try_get("completed_at")
            .map_err(RequestFailure::storage)?,
        replayed,
    };
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    Ok(receipt)
}

fn mutation_request_digest(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for component in [method.as_bytes(), path.as_bytes(), body] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    hasher.finalize().to_vec()
}

#[allow(clippy::too_many_arguments)]
async fn create_memory_in_database(
    pool: &PgPool,
    digest: &[u8],
    writer: &WriterContext,
    request_id: &str,
    key_digest: &[u8],
    request_digest: &[u8],
    input: &CreateMemoryInput,
) -> Result<CreateMemory, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", digest)
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "1000ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "app.tenant_id", &writer.tenant_id)
        .await
        .map_err(RequestFailure::storage)?;
    let row = sqlx::query(
        "SELECT item_id, revision_id, operation_id, completed_at, replayed
         FROM create_private_memory($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(&writer.tenant_id)
    .bind(&writer.credential_id)
    .bind(request_id)
    .bind(key_digest)
    .bind(request_digest)
    .bind(&input.content)
    .bind(&input.subjects)
    .bind(input.valid_from.as_deref())
    .bind(input.valid_until.as_deref())
    .fetch_one(&mut *transaction)
    .await
    .map_err(classify_create_database_error)?;
    let replayed = row.try_get("replayed").map_err(RequestFailure::storage)?;
    let item_id: Option<String> = row.try_get("item_id").map_err(RequestFailure::storage)?;
    let revision_id: Option<String> = row
        .try_get("revision_id")
        .map_err(RequestFailure::storage)?;
    let forgotten = item_id.is_none();
    let created = CreateMemory {
        request_id: (!forgotten).then(|| request_id.to_owned()),
        status: (!forgotten).then_some("ready"),
        operation: forgotten.then_some("create"),
        item_id,
        revision_id,
        operation_id: row
            .try_get("operation_id")
            .map_err(RequestFailure::storage)?,
        completed_at: row
            .try_get("completed_at")
            .map_err(RequestFailure::storage)?,
        replayed,
        lexical_status: (!replayed).then_some("ready"),
        semantic_status: (!replayed).then_some("not_requested"),
    };
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    Ok(created)
}

fn valid_opaque_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn canonical_request_digest(body: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for component in [b"POST".as_slice(), b"/v1/memories".as_slice(), body] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    hasher.finalize().to_vec()
}

fn bounded_json_structure(body: &[u8]) -> bool {
    bounded_json_structure_with_limit(body, 256)
}

fn bounded_json_structure_with_limit(body: &[u8], member_limit: usize) -> bool {
    #[derive(Clone, Copy)]
    enum Container {
        Object,
        Array { has_element: bool },
    }

    let mut stack = Vec::with_capacity(16);
    let mut in_string = false;
    let mut escaped = false;
    let mut members_and_elements = 0_usize;
    for &byte in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte == b'"' {
            if let Some(Container::Array { has_element }) = stack.last_mut() {
                *has_element = true;
            }
            in_string = true;
            continue;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        if let Some(Container::Array { has_element }) = stack.last_mut()
            && !matches!(byte, b']' | b',')
        {
            *has_element = true;
        }
        match byte {
            b'{' => stack.push(Container::Object),
            b'[' => stack.push(Container::Array { has_element: false }),
            b'}' => {
                if !matches!(stack.pop(), Some(Container::Object)) {
                    return false;
                }
            }
            b']' => match stack.pop() {
                Some(Container::Array { has_element }) => {
                    members_and_elements += usize::from(has_element);
                }
                _ => return false,
            },
            b':' => members_and_elements += 1,
            b',' if matches!(stack.last(), Some(Container::Array { .. })) => {
                members_and_elements += 1;
            }
            _ => {}
        }
        if stack.len() > 16 || members_and_elements > member_limit {
            return false;
        }
    }
    !in_string && stack.is_empty() && members_and_elements <= member_limit
}

async fn read_item(
    State(pool): State<PgPool>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let request_id = new_request_id();
    let Some(token) = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| (32..=128).contains(&value.len()) && value.is_ascii())
    else {
        return finish_error(&pool, request_id, RequestFailure::Unauthenticated, None).await;
    };
    let digest = Sha256::digest(token.as_bytes()).to_vec();
    let reader = match tokio::time::timeout(
        AUTHENTICATION_TIMEOUT,
        authenticate_reader(&pool, &digest, "read"),
    )
    .await
    {
        Ok(Ok(reader)) => reader,
        Ok(Err(error)) => return finish_error(&pool, request_id, error, Some(&digest)).await,
        Err(source) => {
            return finish_error(
                &pool,
                request_id,
                RequestFailure::AuthenticationTimeout { source },
                Some(&digest),
            )
            .await;
        }
    };
    let read_query = match parse_read_query(request.uri().query()) {
        Ok(query) => query,
        Err(error) => return finish_error(&pool, request_id, error, Some(&digest)).await,
    };
    let Ok(Path(item_id)) = path else {
        return finish_error(&pool, request_id, RequestFailure::Malformed, Some(&digest)).await;
    };
    if item_id.is_empty()
        || item_id.len() > 64
        || !item_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return finish_error(&pool, request_id, RequestFailure::Malformed, Some(&digest)).await;
    }
    match tokio::time::timeout(EMPTY_BODY_TIMEOUT, to_bytes(request.into_body(), 0)).await {
        Ok(Ok(body)) if body.is_empty() => {}
        Ok(Ok(_)) => {
            return finish_error(&pool, request_id, RequestFailure::Malformed, Some(&digest)).await;
        }
        Ok(Err(source)) => {
            return finish_error(
                &pool,
                request_id,
                RequestFailure::BodyRead { source },
                Some(&digest),
            )
            .await;
        }
        Err(source) => {
            return finish_error(
                &pool,
                request_id,
                RequestFailure::BodyTimeout { source },
                Some(&digest),
            )
            .await;
        }
    }
    match read_item_from_database(
        &pool,
        &item_id,
        read_query.expected_revision_id.as_deref(),
        (!read_query.subject_ids.is_empty()).then_some(read_query.subject_ids.as_slice()),
        &digest,
        &reader,
        &request_id,
    )
    .await
    {
        Ok(item) => Json(item).into_response(),
        Err(error) => finish_error(&pool, request_id, error, Some(&digest)).await,
    }
}

fn parse_read_query(query: Option<&str>) -> Result<ReadQuery, RequestFailure> {
    let Some(query) = query else {
        return Ok(ReadQuery::default());
    };
    if query.is_empty() || query.len() > 1024 {
        return Err(RequestFailure::Malformed);
    }
    let mut parsed = ReadQuery::default();
    let mut time_mode_seen = false;
    for parameter in query.split('&') {
        let Some((name, value)) = parameter.split_once('=') else {
            return Err(RequestFailure::Malformed);
        };
        match name {
            "expected_revision_id" if parsed.expected_revision_id.is_none() => {
                if !valid_opaque_id(value) {
                    return Err(RequestFailure::Malformed);
                }
                parsed.expected_revision_id = Some(value.to_owned());
            }
            "time_mode" if !time_mode_seen => {
                time_mode_seen = true;
                parsed.time_mode = TimeMode::parse(value).ok_or(RequestFailure::Malformed)?;
            }
            "subject_id" if parsed.subject_ids.len() < 8 => {
                if !valid_opaque_id(value) || parsed.subject_ids.iter().any(|id| id == value) {
                    return Err(RequestFailure::Malformed);
                }
                parsed.subject_ids.push(value.to_owned());
            }
            _ => return Err(RequestFailure::Malformed),
        }
    }
    if parsed.time_mode != TimeMode::Current {
        return Err(RequestFailure::UnsupportedTimeSemantics);
    }
    Ok(parsed)
}

async fn authenticate_reader(
    pool: &PgPool,
    digest: &[u8],
    operation: &str,
) -> Result<ReaderContext, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "100ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", digest)
        .await
        .map_err(RequestFailure::storage)?;
    let row = sqlx::query("SELECT tenant_id, credential_id FROM resolve_current_reader($1)")
        .bind(operation)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(RequestFailure::storage)?
        .ok_or(RequestFailure::Unauthenticated)?;
    let reader = ReaderContext {
        tenant_id: row.try_get("tenant_id").map_err(RequestFailure::storage)?,
        credential_id: row
            .try_get("credential_id")
            .map_err(RequestFailure::storage)?,
    };
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    Ok(reader)
}

async fn read_item_from_database(
    pool: &PgPool,
    item_id: &str,
    expected_revision_id: Option<&str>,
    subject_ids: Option<&[String]>,
    digest: &[u8],
    reader: &ReaderContext,
    request_id: &str,
) -> Result<ReadItem, RequestFailure> {
    let mut transaction = pool.begin().await.map_err(RequestFailure::storage)?;
    set_local_bytes(&mut transaction, "app.credential_digest", digest)
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "app.operation", "read")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "lock_timeout", "250ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "statement_timeout", "1000ms")
        .await
        .map_err(RequestFailure::storage)?;
    set_local(&mut transaction, "app.tenant_id", &reader.tenant_id)
        .await
        .map_err(RequestFailure::storage)?;
    let row = sqlx::query(READ_QUERY)
        .bind(&reader.tenant_id)
        .bind(&reader.credential_id)
        .bind(request_id)
        .bind(item_id)
        .bind(expected_revision_id)
        .bind(subject_ids)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(classify_database_error)?;
    let item = row.map(|row| read_item_row(&row, request_id)).transpose()?;
    transaction
        .commit()
        .await
        .map_err(RequestFailure::storage)?;
    item.ok_or(RequestFailure::Unavailable)
}

fn new_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let value = ((nanos & u128::from(u64::MAX)) << 64) | u128::from(sequence);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        value >> 96,
        (value >> 80) & 0xffff,
        (value >> 64) & 0xffff,
        (value >> 48) & 0xffff,
        value & 0xffff_ffff_ffff
    )
}

fn classify_database_error(error: sqlx::Error) -> RequestFailure {
    match error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .as_deref()
    {
        Some("22023") => RequestFailure::DatabaseInputRejected { source: error },
        Some("42501") => RequestFailure::AuthorityRejected { source: error },
        Some("P0003") => RequestFailure::DatabaseStale { source: error },
        _ => RequestFailure::storage(error),
    }
}

fn classify_create_database_error(error: sqlx::Error) -> RequestFailure {
    match error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .as_deref()
    {
        Some("22007" | "22023") => RequestFailure::DatabaseInputRejected { source: error },
        Some("42501") => RequestFailure::AuthorityRejected { source: error },
        Some("P0002") => RequestFailure::DatabaseUnavailable { source: error },
        Some("P0003") => RequestFailure::DatabaseStale { source: error },
        Some("P0004") => RequestFailure::DatabaseIdempotencyConflict { source: error },
        _ => RequestFailure::storage(error),
    }
}

fn classify_list_database_error(error: sqlx::Error) -> RequestFailure {
    match error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .as_deref()
    {
        Some("22023") => RequestFailure::DatabaseInputRejected { source: error },
        Some("42501") => RequestFailure::AuthorityRejected { source: error },
        _ => RequestFailure::storage(error),
    }
}

async fn finish_create_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
    key_digest: Option<&[u8]>,
) -> Response {
    finish_operation_error(
        pool,
        request_id,
        error,
        digest,
        key_digest,
        RequestOperation::Create,
    )
    .await
}

async fn finish_correct_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
    key_digest: Option<&[u8]>,
) -> Response {
    finish_operation_error(
        pool,
        request_id,
        error,
        digest,
        key_digest,
        RequestOperation::Correct,
    )
    .await
}

async fn finish_forget_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
    key_digest: Option<&[u8]>,
) -> Response {
    finish_operation_error(
        pool,
        request_id,
        error,
        digest,
        key_digest,
        RequestOperation::Forget,
    )
    .await
}

async fn finish_search_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
) -> Response {
    finish_operation_error(
        pool,
        request_id,
        error,
        digest,
        None,
        RequestOperation::Search,
    )
    .await
}

async fn finish_list_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
) -> Response {
    finish_operation_error(
        pool,
        request_id,
        error,
        digest,
        None,
        RequestOperation::List,
    )
    .await
}

async fn finish_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
) -> Response {
    finish_operation_error(
        pool,
        request_id,
        error,
        digest,
        None,
        RequestOperation::Read,
    )
    .await
}

async fn finish_operation_error(
    pool: &PgPool,
    request_id: String,
    error: RequestFailure,
    digest: Option<&[u8]>,
    key_digest: Option<&[u8]>,
    operation: RequestOperation,
) -> Response {
    let outcome = match (&operation, &error) {
        (
            RequestOperation::Read,
            RequestFailure::Unavailable | RequestFailure::DatabaseUnavailable { .. },
        ) => None,
        (_, RequestFailure::Unavailable | RequestFailure::DatabaseUnavailable { .. }) => {
            Some("unavailable")
        }
        _ => error.rejection_outcome(),
    };
    if let Some(outcome) = outcome {
        audit_rejection(
            pool,
            &request_id,
            operation.as_str(),
            outcome,
            digest,
            key_digest,
        )
        .await;
    }
    error.response(request_id)
}

async fn audit_rejection(
    pool: &PgPool,
    request_id: &str,
    operation: &str,
    outcome: &str,
    digest: Option<&[u8]>,
    key_digest: Option<&[u8]>,
) {
    let Some(mut connection) = pool.try_acquire() else {
        return;
    };
    let _audit_result = write_rejection_audit(
        &mut connection,
        request_id,
        operation,
        outcome,
        digest,
        key_digest,
    )
    .await;
}

async fn write_rejection_audit(
    connection: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    request_id: &str,
    operation: &str,
    outcome: &str,
    digest: Option<&[u8]>,
    key_digest: Option<&[u8]>,
) -> Result<(), AuditFailure> {
    let mut transaction = connection.begin().await?;
    set_local(&mut transaction, "lock_timeout", "100ms").await?;
    set_local(&mut transaction, "statement_timeout", "250ms").await?;
    if let Some(digest) = digest {
        set_local_bytes(&mut transaction, "app.credential_digest", digest).await?;
    }
    if let Some(key_digest) = key_digest {
        set_local_bytes(&mut transaction, "app.idempotency_key_digest", key_digest).await?;
    }
    sqlx::query("SELECT record_request_rejection($1, $2, $3)")
        .bind(request_id)
        .bind(operation)
        .bind(outcome)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

fn read_item_row(row: &PgRow, request_id: &str) -> Result<ReadItem, RequestFailure> {
    Ok(ReadItem {
        request_id: request_id.to_owned(),
        status: "ready",
        item_id: row.try_get("item_id").map_err(RequestFailure::storage)?,
        revision_id: row
            .try_get("revision_id")
            .map_err(RequestFailure::storage)?,
        content: row.try_get("content").map_err(RequestFailure::storage)?,
        recorded_at: row
            .try_get("recorded_at")
            .map_err(RequestFailure::storage)?,
        valid_from: row.try_get("valid_from").map_err(RequestFailure::storage)?,
        valid_until: row
            .try_get("valid_until")
            .map_err(RequestFailure::storage)?,
        validity_status: row
            .try_get("validity_status")
            .map_err(RequestFailure::storage)?,
    })
}

async fn set_local(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT set_config($1, $2, true)")
        .bind(key)
        .bind(value)
        .fetch_one(&mut **transaction)
        .await
        .map(|_| ())
}

async fn set_local_bytes(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
    value: &[u8],
) -> Result<(), sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT set_config($1, encode($2::bytea, 'hex'), true)")
        .bind(key)
        .bind(value)
        .fetch_one(&mut **transaction)
        .await
        .map(|_| ())
}

#[derive(Serialize)]
struct ReadItem {
    request_id: String,
    status: &'static str,
    item_id: String,
    revision_id: String,
    content: String,
    recorded_at: String,
    valid_from: Option<String>,
    valid_until: Option<String>,
    validity_status: String,
}

struct ReaderContext {
    tenant_id: String,
    credential_id: String,
}

struct PreparedList {
    digest: Vec<u8>,
    reader: ReaderContext,
    limit: i32,
    cursor: Option<String>,
}

#[derive(Clone, Copy)]
enum ListKind {
    Collections,
    Items,
}

impl ListKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Collections => "collections",
            Self::Items => "items",
        }
    }
}

#[derive(Serialize)]
struct ListResponse {
    request_id: String,
    status: &'static str,
    items: Vec<ListEntry>,
    truncated: bool,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ListEntry {
    Collection(CollectionListItem),
    Item(ItemListItem),
}

#[derive(Serialize)]
struct CollectionListItem {
    collection_id: String,
    audience_kind: String,
}

#[derive(Serialize)]
struct ItemListItem {
    item_id: String,
    revision_id: String,
    collection_id: String,
    recorded_at: Option<String>,
    valid_from: Option<String>,
    valid_until: Option<String>,
    validity_status: String,
}

struct PreparedSearch {
    digest: Vec<u8>,
    reader: ReaderContext,
    input: SearchInput,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum QueryMode {
    #[default]
    BoundedV1,
    DisjunctiveV2,
    DisjunctiveV3,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    #[serde(default)]
    query_mode: QueryMode,
    #[serde(default)]
    semantic: bool,
    #[serde(default)]
    semantic_query: Option<SemanticQueryInput>,
    #[serde(default)]
    scope: OptionalSearchScope,
    #[serde(default, deserialize_with = "present_context_bytes")]
    max_context_bytes: Option<u16>,
    #[serde(default)]
    time_mode: TimeMode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticQueryInput {
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model_version: Option<String>,
    #[serde(default)]
    input_recipe_version: Option<String>,
    #[serde(default)]
    dimensions: Option<usize>,
    #[serde(default)]
    original_query_sha256: Option<String>,
    #[serde(default)]
    vector: Option<Vec<f32>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchScope {
    subjects: Vec<String>,
}

#[derive(Default)]
struct OptionalSearchScope(Option<SearchScope>);

impl<'de> Deserialize<'de> for OptionalSearchScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        SearchScope::deserialize(deserializer).map(|scope| Self(Some(scope)))
    }
}

fn present_context_bytes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u16>, D::Error> {
    u16::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum TimeMode {
    #[default]
    Current,
    ValidOn,
    KnownAsOf,
}

impl TimeMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "current" => Some(Self::Current),
            "valid_on" => Some(Self::ValidOn),
            "known_as_of" => Some(Self::KnownAsOf),
            _ => None,
        }
    }
}

#[derive(Default)]
struct ReadQuery {
    expected_revision_id: Option<String>,
    time_mode: TimeMode,
    subject_ids: Vec<String>,
}

#[derive(Serialize)]
struct SearchResponse {
    request_id: String,
    status: &'static str,
    items: Vec<SearchItem>,
    context_bytes: usize,
    truncated: bool,
    warnings: Vec<&'static str>,
}

#[derive(Clone)]
enum SemanticPlan {
    NotRequested,
    Pending(&'static str),
    Failed(&'static str),
    Ready { generation: String, vector: String },
}

impl SemanticPlan {
    const fn status(&self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::Pending(_) => "pending",
            Self::Failed(_) => "failed",
            Self::Ready { .. } => "ready",
        }
    }

    fn diagnostics(&self, no_match: bool) -> Vec<&'static str> {
        let mut diagnostics = match self {
            Self::NotRequested => Vec::new(),
            Self::Pending(value) | Self::Failed(value) => vec!["lexical_ready", *value],
            Self::Ready { .. } => vec!["lexical_ready", "semantic_ready"],
        };
        if no_match && matches!(self, Self::Ready { .. }) {
            diagnostics.push("no_match");
        }
        diagnostics
    }
}

#[derive(Serialize)]
struct SearchItem {
    item_id: String,
    revision_id: String,
    excerpt: String,
    recorded_at: String,
    valid_from: Option<String>,
    valid_until: Option<String>,
    validity_status: String,
    reason: &'static str,
    semantic_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    citation: Option<SearchCitation>,
}

#[derive(Serialize)]
struct SearchCitation {
    source_revision_id: String,
    extraction_set_id: String,
    passage_id: String,
    locator: serde_json::Value,
}

struct WriterContext {
    tenant_id: String,
    credential_id: String,
}

struct PreparedCreate {
    digest: Vec<u8>,
    writer: WriterContext,
    key_digest: Vec<u8>,
    request_digest: Vec<u8>,
    input: CreateMemoryInput,
}

struct PreparedCorrect {
    digest: Vec<u8>,
    writer: WriterContext,
    key_digest: Vec<u8>,
    request_digest: Vec<u8>,
    item_id: String,
    input: CorrectMemoryInput,
}

struct PreparedForget {
    digest: Vec<u8>,
    writer: WriterContext,
    key_digest: Vec<u8>,
    request_digest: Vec<u8>,
    item_id: String,
    input: ForgetMemoryInput,
}

struct RequestRejection {
    error: RequestFailure,
    digest: Option<Vec<u8>>,
}

impl RequestRejection {
    const fn unattributed(error: RequestFailure) -> Self {
        Self {
            error,
            digest: None,
        }
    }

    fn attributed(error: RequestFailure, digest: Vec<u8>) -> Self {
        Self {
            error,
            digest: Some(digest),
        }
    }
}

enum RequestOperation {
    Read,
    Search,
    List,
    Create,
    Correct,
    Forget,
}

impl RequestOperation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Search => "search",
            Self::List => "list",
            Self::Create => "create",
            Self::Correct => "correct",
            Self::Forget => "forget",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateMemoryInput {
    content: String,
    subjects: Vec<String>,
    #[serde(default, skip_serializing_if = "OptionalTimestamp::is_absent")]
    valid_from: OptionalTimestamp,
    #[serde(default, skip_serializing_if = "OptionalTimestamp::is_absent")]
    valid_until: OptionalTimestamp,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CorrectMemoryInput {
    expected_revision_id: String,
    content: String,
    subjects: Vec<String>,
    #[serde(default, skip_serializing_if = "OptionalTimestamp::is_absent")]
    valid_from: OptionalTimestamp,
    #[serde(default, skip_serializing_if = "OptionalTimestamp::is_absent")]
    valid_until: OptionalTimestamp,
}

#[derive(Default, Serialize)]
#[serde(transparent)]
struct OptionalTimestamp(Option<String>);

impl OptionalTimestamp {
    const fn is_absent(&self) -> bool {
        self.0.is_none()
    }

    fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<'de> Deserialize<'de> for OptionalTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.contains('\0') {
            return Err(serde::de::Error::custom("invalid timestamp"));
        }
        Ok(Self(Some(value)))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ForgetMemoryInput {
    expected_revision_id: String,
}

#[derive(Serialize)]
struct CreateMemory {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision_id: Option<String>,
    operation_id: String,
    completed_at: String,
    replayed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    lexical_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_status: Option<&'static str>,
}

#[derive(Serialize)]
struct ForgetMemory {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    operation: &'static str,
    operation_id: String,
    completed_at: String,
    replayed: bool,
}

#[derive(Debug, Error)]
enum RequestFailure {
    #[error("malformed request")]
    Malformed,
    #[error("authentication required")]
    Unauthenticated,
    #[error("item unavailable")]
    Unavailable,
    #[error("requested time semantics are unsupported")]
    UnsupportedTimeSemantics,
    #[error("storage unavailable")]
    Storage {
        #[source]
        source: sqlx::Error,
    },
    #[error("stored JSON is invalid")]
    StoredJson {
        #[source]
        source: serde_json::Error,
    },
    #[error("request authority rejected")]
    AuthorityRejected {
        #[source]
        source: sqlx::Error,
    },
    #[error("database rejected request input")]
    DatabaseInputRejected {
        #[source]
        source: sqlx::Error,
    },
    #[error("requested resource unavailable")]
    DatabaseUnavailable {
        #[source]
        source: sqlx::Error,
    },
    #[error("stale current revision")]
    DatabaseStale {
        #[source]
        source: sqlx::Error,
    },
    #[error("idempotency key conflicts with completed request")]
    DatabaseIdempotencyConflict {
        #[source]
        source: sqlx::Error,
    },
    #[error("authentication storage timed out")]
    AuthenticationTimeout {
        #[source]
        source: tokio::time::error::Elapsed,
    },
    #[error("request body read timed out")]
    BodyTimeout {
        #[source]
        source: tokio::time::error::Elapsed,
    },
    #[error("request body read failed")]
    BodyRead {
        #[source]
        source: axum::Error,
    },
    #[error("request JSON is invalid")]
    JsonBody {
        #[source]
        source: serde_json::Error,
    },
}

impl RequestFailure {
    fn storage(source: sqlx::Error) -> Self {
        Self::Storage { source }
    }

    fn rejection_outcome(&self) -> Option<&'static str> {
        match self {
            Self::Malformed
            | Self::DatabaseInputRejected { .. }
            | Self::BodyTimeout { .. }
            | Self::BodyRead { .. }
            | Self::JsonBody { .. } => Some("malformed"),
            Self::Unauthenticated | Self::AuthorityRejected { .. } => Some("unauthenticated"),
            Self::Storage { .. } | Self::StoredJson { .. } | Self::AuthenticationTimeout { .. } => {
                Some("storage_unavailable")
            }
            Self::DatabaseStale { .. } => Some("stale_context"),
            Self::DatabaseIdempotencyConflict { .. } => Some("idempotency_conflict"),
            Self::UnsupportedTimeSemantics => Some("unsupported_time_semantics"),
            Self::Unavailable | Self::DatabaseUnavailable { .. } => None,
        }
    }

    fn response(&self, request_id: String) -> Response {
        let (http_status, code) = match self {
            Self::Malformed
            | Self::DatabaseInputRejected { .. }
            | Self::BodyTimeout { .. }
            | Self::BodyRead { .. }
            | Self::JsonBody { .. } => (StatusCode::BAD_REQUEST, "malformed"),
            Self::Unauthenticated | Self::AuthorityRejected { .. } => {
                (StatusCode::UNAUTHORIZED, "unauthenticated")
            }
            Self::Unavailable | Self::DatabaseUnavailable { .. } => {
                (StatusCode::NOT_FOUND, "unavailable")
            }
            Self::DatabaseStale { .. } => (StatusCode::CONFLICT, "stale_context"),
            Self::DatabaseIdempotencyConflict { .. } => {
                (StatusCode::CONFLICT, "idempotency_conflict")
            }
            Self::UnsupportedTimeSemantics => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_time_semantics",
            ),
            Self::Storage { .. } | Self::StoredJson { .. } | Self::AuthenticationTimeout { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
            }
        };
        (
            http_status,
            Json(ErrorBody {
                request_id,
                status: code,
                code,
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Error)]
#[error("rejection audit storage operation failed")]
struct AuditFailure(#[from] sqlx::Error);

#[derive(Serialize)]
struct ErrorBody {
    request_id: String,
    status: &'static str,
    code: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[tokio::test]
    async fn storage_failure_keeps_its_source_behind_a_sanitized_response() {
        let failure = RequestFailure::Storage {
            source: sqlx::Error::PoolClosed,
        };

        let source = failure.source().expect("storage failure source");
        assert!(source.downcast_ref::<sqlx::Error>().is_some());

        let response = failure.response("00000000-0000-0000-0000-000000000001".to_owned());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("read sanitized error body");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("parse sanitized error body");
        assert_eq!(
            body,
            serde_json::json!({
                "request_id": "00000000-0000-0000-0000-000000000001",
                "status": "storage_unavailable",
                "code": "storage_unavailable"
            })
        );
        assert!(!body.to_string().contains("closed pool"));
    }

    #[test]
    fn json_structure_limits_are_exact_and_ignore_quoted_delimiters() {
        let depth_16 = format!("{}0{}", "[".repeat(16), "]".repeat(16));
        let depth_17 = format!("{}0{}", "[".repeat(17), "]".repeat(17));
        assert!(bounded_json_structure(depth_16.as_bytes()));
        assert!(!bounded_json_structure(depth_17.as_bytes()));

        let elements_256 = format!("[{}]", vec!["null"; 256].join(","));
        let elements_257 = format!("[{}]", vec!["null"; 257].join(","));
        assert!(bounded_json_structure(elements_256.as_bytes()));
        assert!(!bounded_json_structure(elements_257.as_bytes()));

        let members_256 = format!(
            "{{{}}}",
            (0..256)
                .map(|index| format!("\"k{index}\":null"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let members_257 = format!(
            "{{{}}}",
            (0..257)
                .map(|index| format!("\"k{index}\":null"))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(bounded_json_structure(members_256.as_bytes()));
        assert!(!bounded_json_structure(members_257.as_bytes()));
        assert!(bounded_json_structure(
            br#"{"quoted":"[{}:,\\\" ]","array":[1]}"#
        ));

        let external_4096 = format!("[{}]", vec!["1"; 4096].join(","));
        assert!(bounded_json_structure_with_limit(
            external_4096.as_bytes(),
            MAX_EXTERNAL_VECTOR_DIMENSIONS
        ));
        let external_4097 = format!("[{}]", vec!["1"; 4097].join(","));
        assert!(!bounded_json_structure_with_limit(
            external_4097.as_bytes(),
            MAX_EXTERNAL_VECTOR_DIMENSIONS
        ));
    }

    #[test]
    fn external_vector_boundary_and_cosine_validity_are_exact() {
        let query = "bounded query";
        let digest =
            Sha256::digest(query.as_bytes())
                .iter()
                .fold(String::new(), |mut value, byte| {
                    write!(value, "{byte:02x}").unwrap();
                    value
                });
        let input = |vector: Vec<f32>| SemanticQueryInput {
            generation_id: Some("generation".to_owned()),
            model_version: Some("model".to_owned()),
            input_recipe_version: Some("recipe".to_owned()),
            dimensions: Some(vector.len()),
            original_query_sha256: Some(digest.clone()),
            vector: Some(vector),
        };
        assert!(matches!(
            external_semantic_plan(
                "generation".to_owned(),
                "model",
                "recipe",
                4096,
                query,
                &input(vec![1.0; 4096]),
            ),
            SemanticPlan::Ready { .. }
        ));
        for candidate in [
            input(vec![1.0; 4097]),
            input(vec![0.0; 4096]),
            input(vec![f32::NAN; 4096]),
        ] {
            assert!(matches!(
                external_semantic_plan(
                    "generation".to_owned(),
                    "model",
                    "recipe",
                    i32::try_from(candidate.dimensions.unwrap()).unwrap(),
                    query,
                    &candidate,
                ),
                SemanticPlan::Pending("semantic_external_input_invalid")
            ));
        }
    }

    #[test]
    fn query_mode_is_explicit_and_lexical_only() {
        let default: SearchInput =
            serde_json::from_value(serde_json::json!({"query":"alpha"})).unwrap();
        assert!(default.query_mode == QueryMode::BoundedV1);
        assert!(validate_search_input(&default).is_ok());
        for mode in ["disjunctive_v2", "disjunctive_v3"] {
            for semantic in [false, true] {
                let candidate: SearchInput = serde_json::from_value(
                    serde_json::json!({"query":"alpha","query_mode":mode,"semantic":semantic}),
                )
                .unwrap();
                assert_eq!(validate_search_input(&candidate).is_ok(), !semantic);
            }
            let external: SearchInput = serde_json::from_value(
                serde_json::json!({"query":"alpha","query_mode":mode,"semantic_query":{}}),
            )
            .unwrap();
            assert!(validate_search_input(&external).is_err());
        }
        assert!(
            serde_json::from_value::<SearchInput>(
                serde_json::json!({"query":"alpha","query_mode":"unknown"})
            )
            .is_err()
        );
    }

    #[test]
    fn whitespace_free_excerpt_uses_the_exact_byte_bound() {
        let mut budget = ExcerptBudget::new(512);
        let excerpt = budget
            .take(&"x".repeat(513))
            .expect("first excerpt fits the aggregate budget");
        assert_eq!(excerpt.len(), 512);
        assert_eq!(budget.remaining, 0);
        assert!(budget.truncated);
    }

    #[test]
    fn multibyte_excerpt_stays_on_a_utf8_boundary() {
        let excerpt = bounded_excerpt(&"é".repeat(300), 511);
        assert_eq!(excerpt.len(), 510);
        assert!(excerpt.is_char_boundary(excerpt.len()));

        let mut budget = ExcerptBudget::new(4096);
        for _ in 0..7 {
            assert!(budget.take(&"x".repeat(512)).is_some());
        }
        assert!(budget.take(&"x".repeat(511)).is_some());
        assert_eq!(budget.remaining, 1);
        assert!(budget.take("é").is_none());
        assert!(budget.truncated);
    }

    #[test]
    fn multiple_excerpts_cannot_exceed_the_aggregate_budget() {
        let mut budget = ExcerptBudget::new(1024);
        let content = "z".repeat(512);
        let excerpts = (0..3)
            .filter_map(|_| budget.take(&content))
            .collect::<Vec<_>>();
        assert_eq!(excerpts.len(), 2);
        assert_eq!(excerpts.iter().map(String::len).sum::<usize>(), 1024);
        assert_eq!(budget.remaining, 0);
        assert!(budget.truncated);
    }
}

#[cfg(test)]
mod search_limit_contract_tests {
    use super::*;

    #[test]
    fn configured_context_limits_and_missing_only_default() {
        let limits = SearchLimits::default();
        assert_eq!(
            (limits.default_context_bytes, limits.max_context_bytes),
            (8192, 16384)
        );
        for (default, maximum) in [(0, 1), (2, 1), (1, 65536), (65536, 65536)] {
            assert!(SearchLimits::new(default, maximum).is_err());
        }
        for body in [
            r#"{"query":"q","max_context_bytes":null}"#,
            r#"{"query":"q","max_context_bytes":1.5}"#,
            r#"{"query":"q","max_context_bytes":65536}"#,
        ] {
            assert!(serde_json::from_str::<SearchInput>(body).is_err());
        }
        let omitted: SearchInput = serde_json::from_str(r#"{"query":"q"}"#).unwrap();
        assert!(omitted.max_context_bytes.is_none());
        assert!(validate_search_input_with_limits(&omitted, limits).is_ok());
        for (budget, accepted) in [
            (0, false),
            (4096, true),
            (8192, true),
            (16384, true),
            (16385, false),
        ] {
            let input: SearchInput =
                serde_json::from_value(serde_json::json!({"query":"q","max_context_bytes":budget}))
                    .unwrap();
            assert_eq!(
                validate_search_input_with_limits(&input, limits).is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn maximum_legal_escaped_search_response_fits_consumer_allowance() {
        let mut remaining = 65535;
        let mut items = Vec::new();
        for index in 0..12 {
            let bytes = remaining.min(5462);
            remaining -= bytes;
            items.push(SearchItem {
                item_id: format!("{index:0>64}"),
                revision_id: "r".repeat(64),
                excerpt: "\u{1}".repeat(bytes),
                recorded_at: "999999-12-31T23:59:59.999999Z".into(),
                valid_from: Some("999999-12-31T23:59:59.999999Z".into()),
                valid_until: Some("999999-12-31T23:59:59.999999Z".into()),
                validity_status: "known".into(),
                reason: "adjacent_continuation",
                semantic_status: "not_requested",
                citation: Some(SearchCitation {
                    source_revision_id: "s".repeat(64),
                    extraction_set_id: "e".repeat(64),
                    passage_id: format!("{index:0>64}"),
                    locator: serde_json::json!({"x":"z".repeat(2039)}),
                }),
            });
        }
        assert_eq!(remaining, 0);
        for item in &items {
            assert!(
                serde_json::to_vec(&item.citation.as_ref().unwrap().locator)
                    .unwrap()
                    .len()
                    <= 2048
            );
        }
        let response = SearchResponse {
            request_id: "f".repeat(36),
            status: "ready",
            items,
            context_bytes: 65535,
            truncated: true,
            warnings: vec!["lexical_ready", "semantic_ready", "no_matches"],
        };
        let bytes = serde_json::to_vec(&response).unwrap();
        assert!(bytes.len() > 393_210);
        assert!(bytes.len() < 512 * 1024);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["context_bytes"],
            65535
        );
    }
}
