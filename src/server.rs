use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, IF_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::io::ReaderStream;

use crate::catalog::{Catalog, CatalogItem, CheckReport, ItemView};
use crate::config::Config;
use crate::hook::{
    HookItems, HookOperation, HookOrigin, PostSaveHook, PreparedPostSaveHook, SUPPRESS_HOOK_HEADER,
    revision,
};
use crate::library::{ItemPatch, LibraryLayout, LibraryStore, NewItem, RemovedItem};
use crate::{Error, Result as LantaiResult};

type ApiResult<T> = std::result::Result<T, ApiError>;
const ORIGIN_HEADER: &str = "X-Lantai-Origin";

#[derive(Clone)]
pub(crate) struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    config: Config,
    layout: LibraryLayout,
    cache: RwLock<CacheSnapshot>,
    mutation: Mutex<()>,
    hook: PostSaveHook,
}

#[derive(Clone)]
struct CacheSnapshot {
    source: Arc<str>,
    revision: String,
    items: Arc<[CatalogItem]>,
    report: Arc<CheckReport>,
    disk_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<JsonValue>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Option<JsonValue>,
}

/// Unknown parameters are rejected rather than ignored: a client still sending
/// the removed `tag`/`type` filters would otherwise receive the whole library
/// and mistake it for a filtered result.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    q: Option<String>,
    collection: Option<String>,
    sort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExportQuery {
    ids: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateItemRequest {
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default)]
    citation_key: Option<String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
}

/// Unknown fields are rejected for the same reason as `ListQuery`: a body
/// still spelling the membership list `tags` must fail rather than report a
/// successful change it did not make.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchItemRequest {
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    set_raw: BTreeMap<String, String>,
    #[serde(default)]
    unset: Vec<String>,
    collections: Option<Vec<String>>,
    citation_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImportRequest {
    source: String,
}

#[derive(Debug, Serialize)]
struct ItemResponse {
    #[serde(flatten)]
    item: ItemView,
    revision: String,
}

#[derive(Debug, Serialize)]
struct ListResponse {
    items: Vec<ItemView>,
    revision: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    /// The daemon's own version, so a client can refuse to speak to a build
    /// whose field names it does not share.
    version: &'static str,
    revision: String,
    entries: usize,
    warnings: usize,
    errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_error: Option<String>,
}

pub async fn serve(
    config: Config,
    layout: LibraryLayout,
    config_path: std::path::PathBuf,
) -> LantaiResult<()> {
    let address =
        config
            .api_address
            .parse::<SocketAddr>()
            .map_err(|error| Error::InvalidSocketAddress {
                address: config.api_address.clone(),
                message: error.to_string(),
            })?;
    if !address.ip().is_loopback() {
        return Err(Error::NonLoopbackAddress {
            address: address.to_string(),
        });
    }

    let hook = PostSaveHook::new(config.post_save_hook.as_ref(), &config_path, layout.clone());
    let connector_config = config.clone();
    let connector_layout = layout.clone();
    let state = AppState::new_with_hook(config, layout, hook.clone())?;
    spawn_watcher(state.clone())?;
    let app = native_router(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| Error::Listen {
            address: address.to_string(),
            source,
        })?;
    println!("Lantai REST API listening on http://{address}");
    let mut native_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .map_err(|source| Error::Listen {
                address: address.to_string(),
                source,
            })
    });
    let mut connector_task = tokio::spawn(crate::connector::serve(
        connector_config,
        connector_layout,
        hook,
    ));
    let result = tokio::select! {
        result = &mut native_task => join_server(result, address.to_string()),
        result = &mut connector_task => join_server(result, CONNECTOR_ADDRESS_LABEL.to_owned()),
        () = shutdown_signal() => Ok(()),
    };
    native_task.abort();
    connector_task.abort();
    result
}

const CONNECTOR_ADDRESS_LABEL: &str = "127.0.0.1:23119";

fn join_server(
    result: std::result::Result<LantaiResult<()>, tokio::task::JoinError>,
    address: String,
) -> LantaiResult<()> {
    result.map_err(|error| Error::Daemon {
        address,
        message: error.to_string(),
    })?
}

impl AppState {
    #[cfg(test)]
    pub(crate) fn new(config: Config, layout: LibraryLayout) -> LantaiResult<Self> {
        let hook = PostSaveHook::new(
            config.post_save_hook.as_ref(),
            std::path::Path::new("config.toml"),
            layout.clone(),
        );
        Self::new_with_hook(config, layout, hook)
    }

    fn new_with_hook(
        config: Config,
        layout: LibraryLayout,
        hook: PostSaveHook,
    ) -> LantaiResult<Self> {
        let source = layout.read_utf8()?;
        let catalog = Catalog::parse(&layout.bibliography, &source)?;
        if !catalog.is_syntactically_valid() {
            return Err(Error::DegradedBibliography {
                path: layout.bibliography.clone(),
                message: "cannot start the daemon without an initially valid library".to_owned(),
            });
        }
        let snapshot = CacheSnapshot {
            revision: revision(&source),
            items: catalog.items().collect::<Vec<_>>().into(),
            report: Arc::new(catalog.check()),
            source: Arc::from(source),
            disk_error: None,
        };
        Ok(Self {
            inner: Arc::new(AppStateInner {
                config,
                layout,
                cache: RwLock::new(snapshot),
                mutation: Mutex::new(()),
                hook,
            }),
        })
    }

    async fn snapshot(&self) -> CacheSnapshot {
        self.inner.cache.read().await.clone()
    }

    async fn refresh(&self) {
        match self.inner.layout.read_utf8().and_then(|source| {
            let catalog = Catalog::parse(&self.inner.layout.bibliography, &source)?;
            let valid = catalog.is_syntactically_valid();
            let items = catalog.items().collect::<Vec<_>>();
            let report = catalog.check();
            Ok((source, valid, items, report))
        }) {
            Ok((source, true, items, report)) => {
                let revision = revision(&source);
                let mut cache = self.inner.cache.write().await;
                if cache.revision != revision || cache.disk_error.is_some() {
                    *cache = CacheSnapshot {
                        source: Arc::from(source),
                        revision,
                        items: items.into(),
                        report: Arc::new(report),
                        disk_error: None,
                    };
                }
            }
            Ok((_, false, _, _)) => {
                self.inner.cache.write().await.disk_error =
                    Some("the bibliography contains malformed BibLaTeX".to_owned());
            }
            Err(error) => {
                self.inner.cache.write().await.disk_error = Some(error.to_string());
            }
        }
    }

    async fn refresh_watched(&self) {
        let disk_source = match self.inner.layout.read_utf8() {
            Ok(source) => source,
            Err(error) => {
                self.inner.cache.write().await.disk_error = Some(error.to_string());
                return;
            }
        };
        let snapshot = self.snapshot().await;
        if revision(&disk_source) == snapshot.revision {
            if snapshot.disk_error.is_some() {
                self.refresh().await;
            }
            return;
        }
        let valid = Catalog::parse(&self.inner.layout.bibliography, &disk_source)
            .is_ok_and(|catalog| catalog.is_syntactically_valid());
        if !valid {
            self.inner.cache.write().await.disk_error =
                Some("the bibliography contains malformed BibLaTeX".to_owned());
            return;
        }

        let store = self.store();
        match tokio::task::spawn_blocking(move || store.format()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.inner.cache.write().await.disk_error = Some(error.to_string());
                return;
            }
            Err(error) => {
                self.inner.cache.write().await.disk_error = Some(error.to_string());
                return;
            }
        }
        self.refresh().await;
    }

    fn store(&self) -> LibraryStore {
        LibraryStore::new(self.inner.layout.clone())
    }
}

pub(crate) fn native_router(state: AppState) -> Router {
    let body_limit = usize::try_from(state.inner.config.attachment_limit_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(1024 * 1024);
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/items", get(list_items).post(create_item))
        .route(
            "/api/v1/items/{id}",
            get(get_item).patch(patch_item).delete(delete_item),
        )
        .route("/api/v1/items/{id}/attachments", post(upload_attachment))
        .route(
            "/api/v1/items/{id}/attachments/{attachment_id}",
            get(download_attachment).delete(delete_attachment),
        )
        .route("/api/v1/export", get(export_library))
        .route("/api/v1/import", post(import_library))
        .route("/api/v1/format", post(format_library))
        .route("/api/v1/check", get(check_library))
        .route("/api/v1/trash", get(list_trash).delete(purge_trash))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let expected = format!("Bearer {}", state.inner.config.api_token);
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected);
    if authorized {
        next.run(request).await
    } else {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid bearer token",
        )
        .into_response()
    }
}

async fn health(State(state): State<AppState>) -> ApiResult<Response> {
    let snapshot = state.snapshot().await;
    let report = &snapshot.report;
    let degraded = snapshot.disk_error.is_some() || report.errors > 0;
    Ok(json_response(
        StatusCode::OK,
        &HealthResponse {
            status: if degraded { "degraded" } else { "ok" },
            version: env!("CARGO_PKG_VERSION"),
            revision: snapshot.revision.clone(),
            entries: report.entries,
            warnings: report.warnings,
            errors: report.errors,
            disk_error: snapshot.disk_error,
        },
        &snapshot.revision,
    ))
}

async fn list_items(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Response> {
    let filter = crate::query::Query::parse_str(query.q.as_deref().unwrap_or_default())?
        .with_collection(query.collection.as_deref());
    let sort = query
        .sort
        .as_deref()
        .map(crate::query::Sort::parse)
        .transpose()?;
    let snapshot = state.snapshot().await;
    let mut items = snapshot
        .items
        .iter()
        .filter(|item| filter.matches(item))
        .cloned()
        .map(ItemView::from)
        .collect::<Vec<_>>();
    if let Some(sort) = sort {
        sort.apply(&mut items);
    }
    Ok(json_response(
        StatusCode::OK,
        &ListResponse {
            items,
            revision: snapshot.revision.clone(),
        },
        &snapshot.revision,
    ))
}

async fn get_item(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let snapshot = state.snapshot().await;
    let item = find_indexed_item(&snapshot.items, &id)?;
    Ok(json_response(
        StatusCode::OK,
        &ItemResponse {
            item: item.into(),
            revision: snapshot.revision.clone(),
        },
        &snapshot.revision,
    ))
}

async fn create_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateItemRequest>,
) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let added = run_blocking(move || {
        store.add_item(NewItem {
            entry_type: request.entry_type,
            citation_key: request.citation_key,
            fields: request.fields.into_iter().collect(),
        })
    })
    .await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let item = find_indexed_item(&snapshot.items, &added.uuid.to_string())?;
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::ItemCreate,
        HookItems::Uuids(vec![added.uuid]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(json_response(
        StatusCode::CREATED,
        &ItemResponse {
            item: item.into(),
            revision: snapshot.revision.clone(),
        },
        &snapshot.revision,
    ))
}

async fn patch_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<PatchItemRequest>,
) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let result = run_blocking(move || {
        store.patch_item(
            &id,
            ItemPatch {
                set: request.set,
                set_raw: request.set_raw,
                unset: request.unset,
                collections: request.collections,
                citation_key: request.citation_key,
            },
        )
    })
    .await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let item = find_indexed_item(&snapshot.items, &result.uuid.to_string())?;
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::ItemUpdate,
        HookItems::Uuids(vec![result.uuid]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(json_response(
        StatusCode::OK,
        &ItemResponse {
            item: item.into(),
            revision: snapshot.revision.clone(),
        },
        &snapshot.revision,
    ))
}

async fn delete_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let removed = run_blocking(move || store.remove_item(&id)).await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let mut response = StatusCode::NO_CONTENT.into_response();
    insert_etag(response.headers_mut(), &snapshot.revision);
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::ItemDelete,
        HookItems::Uuids(Vec::new()),
        vec![removed],
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(response)
}

async fn upload_attachment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let limit = state.inner.config.attachment_limit_bytes;
    let mut upload = None;
    let mut title = None;
    let mut requested_media_type = None;

    while let Some(mut field) = multipart.next_field().await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            error.to_string(),
        )
    })? {
        match field.name() {
            Some("file") => {
                if upload.is_some() {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_multipart",
                        "exactly one file field is required",
                    ));
                }
                let filename = field.file_name().unwrap_or("attachment").to_owned();
                let field_media_type = field.content_type().map(str::to_owned);
                let mut temporary = tempfile::NamedTempFile::new().map_err(|error| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "upload_failed",
                        error.to_string(),
                    )
                })?;
                let mut size = 0_u64;
                while let Some(chunk) = field.chunk().await.map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_multipart",
                        error.to_string(),
                    )
                })? {
                    size = size.saturating_add(chunk.len() as u64);
                    if size > limit {
                        return Err(ApiError::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "attachment_too_large",
                            format!("attachment exceeds the configured limit of {limit} bytes"),
                        ));
                    }
                    temporary.write_all(&chunk).map_err(|error| {
                        ApiError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "upload_failed",
                            error.to_string(),
                        )
                    })?;
                }
                temporary.as_file_mut().sync_all().map_err(|error| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "upload_failed",
                        error.to_string(),
                    )
                })?;
                upload = Some((temporary, filename, field_media_type));
            }
            Some("title") => {
                title = Some(field.text().await.map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_multipart",
                        error.to_string(),
                    )
                })?);
            }
            Some("media_type") => {
                requested_media_type = Some(field.text().await.map_err(|error| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_multipart",
                        error.to_string(),
                    )
                })?);
            }
            _ => {}
        }
    }
    let (temporary, filename, field_media_type) = upload.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            "a file field is required",
        )
    })?;
    let media_type = requested_media_type.or(field_media_type).or_else(|| {
        Some(
            mime_guess::from_path(&filename)
                .first_or_octet_stream()
                .essence_str()
                .to_owned(),
        )
    });
    let store = state.store();
    let attached = run_blocking(move || {
        store.attach_file_named(
            &id,
            temporary.path(),
            &filename,
            title.as_deref(),
            media_type.as_deref(),
            limit,
        )
    })
    .await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::AttachmentCreate,
        HookItems::Uuids(vec![attached.item_uuid]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(json_response(
        StatusCode::CREATED,
        &attached,
        &snapshot.revision,
    ))
}

async fn download_attachment(
    State(state): State<AppState>,
    Path((id, attachment_id)): Path<(String, uuid::Uuid)>,
) -> ApiResult<Response> {
    let store = state.store();
    let (attachment, path) =
        run_blocking(move || store.attachment_file(&id, attachment_id)).await?;
    let file = tokio::fs::File::open(&path).await.map_err(|error| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "attachment_not_found",
            error.to_string(),
        )
    })?;
    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    if let Ok(content_type) = HeaderValue::from_str(&attachment.media_type) {
        response.headers_mut().insert(CONTENT_TYPE, content_type);
    }
    let filename = crate::attachments::sanitize_filename(std::path::Path::new(&attachment.title));
    if let Ok(disposition) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    {
        response
            .headers_mut()
            .insert(CONTENT_DISPOSITION, disposition);
    }
    let snapshot = state.snapshot().await;
    insert_etag(response.headers_mut(), &snapshot.revision);
    Ok(response)
}

async fn delete_attachment(
    State(state): State<AppState>,
    Path((id, attachment_id)): Path<(String, uuid::Uuid)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let detached = run_blocking(move || store.detach_attachment(&id, attachment_id)).await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::AttachmentDelete,
        HookItems::Uuids(vec![detached.item_uuid]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(json_response(StatusCode::OK, &detached, &snapshot.revision))
}

async fn export_library(
    State(state): State<AppState>,
    Query(query): Query<ExportQuery>,
) -> ApiResult<Response> {
    let snapshot = state.snapshot().await;
    let ids = query
        .ids
        .as_deref()
        .map(|ids| {
            ids.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let source = snapshot.source.to_string();
    let store = state.store();
    let exported = run_blocking(move || store.export_biblatex_from(&source, &ids)).await?;
    let mut response = exported.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-bibtex; charset=utf-8"),
    );
    insert_etag(response.headers_mut(), &snapshot.revision);
    Ok(response)
}

async fn import_library(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ImportRequest>,
) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let added = run_blocking(move || store.import_biblatex(&request.source)).await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::ItemImport,
        HookItems::Uuids(added.iter().map(|item| item.uuid).collect()),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(json_response(
        StatusCode::CREATED,
        &added,
        &snapshot.revision,
    ))
}

async fn format_library(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    let before = state.snapshot().await.revision;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let formatted = run_blocking(move || store.format()).await?;
    state.refresh().await;
    let snapshot = state.snapshot().await;
    let prepared = prepare_hook(
        &state,
        &headers,
        before,
        HookOperation::LibraryFormat,
        HookItems::All,
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(json_response(
        StatusCode::OK,
        &formatted,
        &snapshot.revision,
    ))
}

async fn check_library(State(state): State<AppState>) -> ApiResult<Response> {
    let store = state.store();
    let report: CheckReport = run_blocking(move || store.check()).await?;
    let snapshot = state.snapshot().await;
    Ok(json_response(StatusCode::OK, &report, &snapshot.revision))
}

async fn list_trash(State(state): State<AppState>) -> ApiResult<Response> {
    let store = state.store();
    let entries = run_blocking(move || store.trash_entries()).await?;
    let snapshot = state.snapshot().await;
    Ok(json_response(StatusCode::OK, &entries, &snapshot.revision))
}

async fn purge_trash(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let _guard = state.inner.mutation.lock().await;
    require_current_revision(&state, &headers).await?;
    let store = state.store();
    let purged = run_blocking(move || store.purge_trash()).await?;
    let snapshot = state.snapshot().await;
    Ok(json_response(
        StatusCode::OK,
        &json!({ "purged": purged }),
        &snapshot.revision,
    ))
}

async fn require_current_revision(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let supplied = headers
        .get(IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "precondition_required",
                "mutating requests require If-Match",
            )
        })?;
    let current = state.snapshot().await.revision;
    if supplied.trim_matches('"') == current {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "revision_conflict",
            message: "the library revision has changed".to_owned(),
            details: Some(json!({ "current_revision": current })),
        })
    }
}

fn prepare_hook(
    state: &AppState,
    headers: &HeaderMap,
    before: String,
    operation: HookOperation,
    affected: HookItems,
    removed: Vec<RemovedItem>,
) -> Option<PreparedPostSaveHook> {
    if headers
        .get(SUPPRESS_HOOK_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1")
    {
        return None;
    }
    let origin = if headers
        .get(ORIGIN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cli"))
    {
        HookOrigin::Cli
    } else {
        HookOrigin::Rest
    };
    state
        .inner
        .hook
        .prepare(Some(before), operation, origin, affected, removed)
}

async fn run_prepared_hook(prepared: Option<PreparedPostSaveHook>) {
    if let Some(prepared) = prepared
        && let Err(error) = tokio::task::spawn_blocking(move || prepared.run()).await
    {
        eprintln!("warning: post-save hook task failed: {error}");
    }
}

fn find_indexed_item(items: &[CatalogItem], id: &str) -> LantaiResult<CatalogItem> {
    let parsed_uuid = uuid::Uuid::parse_str(id).ok();
    let matches = items
        .iter()
        .filter(|item| {
            parsed_uuid.is_some_and(|uuid| item.uuid == Some(uuid)) || item.citation_key == id
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(Error::ItemNotFound { id: id.to_owned() }),
        [item] => Ok((*item).clone()),
        _ => Err(Error::AmbiguousItem { id: id.to_owned() }),
    }
}

fn json_response<T: Serialize>(status: StatusCode, value: &T, revision: &str) -> Response {
    let mut response = (status, Json(value)).into_response();
    insert_etag(response.headers_mut(), revision);
    response
}

fn insert_etag(headers: &mut HeaderMap, revision: &str) {
    if let Ok(value) = HeaderValue::from_str(&format!("\"{revision}\"")) {
        headers.insert(ETAG, value);
    }
}

async fn run_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> LantaiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task_failed",
                error.to_string(),
            )
        })?
        .map_err(ApiError::from)
}

fn spawn_watcher(state: AppState) -> LantaiResult<()> {
    let path = state.inner.layout.bibliography.clone();
    let watch_root = path
        .parent()
        .ok_or_else(|| Error::InvalidLibraryPath { path: path.clone() })?
        .to_owned();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let _ = sender.send(event);
    })
    .map_err(|error| Error::Watch {
        path: path.clone(),
        message: error.to_string(),
    })?;
    watcher
        .watch(&watch_root, RecursiveMode::NonRecursive)
        .map_err(|error| Error::Watch {
            path: path.clone(),
            message: error.to_string(),
        })?;
    tokio::spawn(async move {
        let _watcher = watcher;
        while let Some(event) = receiver.recv().await {
            if event.is_err() {
                continue;
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
            while receiver.try_recv().is_ok() {}
            state.refresh_watched().await;
        }
    });
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let (status, code) = match &error {
            Error::ItemNotFound { .. } | Error::AttachmentNotFound { .. } => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Error::AmbiguousItem { .. }
            | Error::DuplicateCitationKey { .. }
            | Error::DuplicateUuid { .. }
            | Error::SourceChanged { .. }
            | Error::DegradedBibliography { .. } => (StatusCode::CONFLICT, "conflict"),
            Error::InvalidFieldArgument { .. }
            | Error::EmptyFieldName
            | Error::DuplicateField { .. }
            | Error::InvalidEntryType { .. }
            | Error::InvalidCitationKey { .. }
            | Error::ReservedField { .. }
            | Error::InvalidRawExpression { .. }
            | Error::InvalidFileField { .. }
            | Error::UnsafeAttachmentPath { .. }
            | Error::AttachmentTooLarge { .. }
            | Error::AttachmentNotFile { .. }
            | Error::InvalidQueryTerm { .. }
            | Error::InvalidSortKey { .. } => (StatusCode::BAD_REQUEST, "invalid_request"),
            Error::ImportHasNoEntries => (StatusCode::BAD_REQUEST, "invalid_request"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code,
                    message: self.message,
                    details: self.details,
                },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn native_mutation_runs_cli_origin_hook_and_honors_suppression() {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let event_path = directory.path().join("event.json");
        let calls_path = directory.path().join("calls");
        let layout = LibraryLayout::new(bibliography).unwrap();
        layout.initialize().unwrap();
        let mut config = Config::new(layout.bibliography.clone());
        config.api_token = "test-token".to_owned();
        config.post_save_hook = Some(crate::config::PostSaveHookConfig {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "cat > \"$1\"; printf x >> \"$2\"".to_owned(),
                "lantai-hook".to_owned(),
                event_path.display().to_string(),
                calls_path.display().to_string(),
            ],
            timeout_seconds: 30,
        });
        let app = native_router(AppState::new(config, layout).unwrap());

        let health = app
            .clone()
            .oneshot(authorized("GET", "/api/v1/health", Body::empty()))
            .await
            .unwrap();
        let etag = health.headers()[ETAG].to_str().unwrap().to_owned();
        let body = json!({"type": "article", "fields": {"title": "Hooked"}}).to_string();
        let mut create = authorized("POST", "/api/v1/items", Body::from(body.clone()));
        create.headers_mut().insert(IF_MATCH, etag.parse().unwrap());
        create
            .headers_mut()
            .insert(ORIGIN_HEADER, "cli".parse().unwrap());
        let created = app.clone().oneshot(create).await.unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let next_etag = created.headers()[ETAG].to_str().unwrap().to_owned();
        let event: JsonValue =
            serde_json::from_str(&std::fs::read_to_string(&event_path).unwrap()).unwrap();
        assert_eq!(event["origin"], "cli");
        assert_eq!(event["operation"], "item.create");
        assert_eq!(event["items"][0]["title"], "Hooked");

        let mut suppressed = authorized("POST", "/api/v1/items", Body::from(body));
        suppressed
            .headers_mut()
            .insert(IF_MATCH, next_etag.parse().unwrap());
        suppressed
            .headers_mut()
            .insert(SUPPRESS_HOOK_HEADER, "1".parse().unwrap());
        assert_eq!(
            app.oneshot(suppressed).await.unwrap().status(),
            StatusCode::CREATED
        );
        assert_eq!(std::fs::read_to_string(calls_path).unwrap(), "x");
    }

    fn test_state() -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().unwrap();
        let bibliography = directory.path().join("references.bib");
        let layout = LibraryLayout::new(bibliography).unwrap();
        layout.initialize().unwrap();
        let mut config = Config::new(layout.bibliography.clone());
        config.api_token = "test-token".to_owned();
        (directory, AppState::new(config, layout).unwrap())
    }

    fn authorized(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(AUTHORIZATION, "Bearer test-token")
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn api_requires_auth_and_enforces_etag_mutations() {
        let (_directory, state) = test_state();
        let app = native_router(state);

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let health = app
            .clone()
            .oneshot(authorized("GET", "/api/v1/health", Body::empty()))
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let etag = health.headers()[ETAG].to_str().unwrap().to_owned();

        let request = json!({
            "type": "article",
            "fields": {
                "author": "Lovelace, Ada",
                "date": "1843",
                "title": "A Sketch"
            }
        });
        let missing_precondition = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/api/v1/items",
                Body::from(request.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(
            missing_precondition.status(),
            StatusCode::PRECONDITION_REQUIRED
        );

        let mut create = authorized("POST", "/api/v1/items", Body::from(request.to_string()));
        create.headers_mut().insert(IF_MATCH, etag.parse().unwrap());
        let created = app.clone().oneshot(create).await.unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let new_etag = created.headers()[ETAG].to_str().unwrap().to_owned();
        assert_ne!(new_etag, etag);
        let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        let item_id = body["uuid"].as_str().unwrap();
        assert_eq!(body["title"], "A Sketch");
        assert!(body["fields"].is_array());
        assert_eq!(body["collections"], json!([]));
        assert_eq!(body["attachments"], json!([]));
        assert_eq!(
            new_etag,
            format!("\"{}\"", body["revision"].as_str().unwrap())
        );

        let mut stale = authorized("POST", "/api/v1/items", Body::from(request.to_string()));
        stale.headers_mut().insert(IF_MATCH, etag.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(stale).await.unwrap().status(),
            StatusCode::CONFLICT
        );

        let fetched = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/api/v1/items/{item_id}"),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(fetched.status(), StatusCode::OK);
        let body = to_bytes(fetched.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["title"], "A Sketch");
        assert_eq!(body["entry_type"], "article");

        let patch = json!({"set": {"title": "A Revised Sketch"}});
        let mut patch_request = authorized(
            "PATCH",
            &format!("/api/v1/items/{item_id}"),
            Body::from(patch.to_string()),
        );
        patch_request
            .headers_mut()
            .insert(IF_MATCH, new_etag.parse().unwrap());
        let patched = app.clone().oneshot(patch_request).await.unwrap();
        assert_eq!(patched.status(), StatusCode::OK);
        let patched_etag = patched.headers()[ETAG].to_str().unwrap().to_owned();
        let body = to_bytes(patched.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["title"], "A Revised Sketch");
        assert_eq!(
            patched_etag,
            format!("\"{}\"", body["revision"].as_str().unwrap())
        );

        let listed = app
            .oneshot(authorized("GET", "/api/v1/items", Body::empty()))
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["title"], "A Revised Sketch");
        assert!(body["items"][0]["fields"].is_array());
        assert!(body["revision"].is_string());
    }

    #[tokio::test]
    async fn api_list_ands_filters_without_stripping_rich_fields() {
        let (_directory, state) = test_state();
        state
            .store()
            .import_biblatex(concat!(
                "@article{first,title={Needle},keywords={keep},",
                "custom=\"raw \" # {value},file={External:/tmp/first.pdf:application/pdf}}\n",
                "@article{second,title={Needle},keywords={drop}}\n",
                "@book{third,title={Needle},keywords={keep}}\n",
                "@article{fourth,title={Other},keywords={keep}}\n"
            ))
            .unwrap();
        state.refresh().await;
        let app = native_router(state);

        let all = app
            .clone()
            .oneshot(authorized("GET", "/api/v1/items", Body::empty()))
            .await
            .unwrap();
        let body = to_bytes(all.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["citation_key"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["first", "second", "third", "fourth"]
        );

        let filtered = app
            .oneshot(authorized(
                "GET",
                "/api/v1/items?q=needle&collection=KEEP",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(filtered.status(), StatusCode::OK);
        assert!(filtered.headers().contains_key(ETAG));
        let body = to_bytes(filtered.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        let items = body["items"].as_array().unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item["citation_key"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["first", "third"],
            "entry type is no longer a filter"
        );
        assert_eq!(items[0]["title"], "Needle");
        assert_eq!(items[0]["collections"], json!(["keep"]));
        assert_eq!(items[0]["attachments"][0]["uuid"], JsonValue::Null);
        assert_eq!(items[0]["attachments"][0]["path"], "/tmp/first.pdf");
        assert!(
            items[0]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| { field["name"] == "custom" && field["raw"] == "\"raw \" # {value}" })
        );
    }

    /// `q` speaks the same language the CLI does, so a saved search works from
    /// either side.
    #[tokio::test]
    async fn api_list_speaks_the_query_language_and_sorts() {
        let (_directory, state) = test_state();
        state
            .store()
            .import_biblatex(concat!(
                "@article{early,title={Early Paper},author={Ada Lovelace},year={1843}}\n",
                "@book{late,title={Late Book},author={Ada Lovelace},year={2019}}\n",
                "@article{other,title={Two Words},author={Grace Hopper}}\n"
            ))
            .unwrap();
        state.refresh().await;
        let app = native_router(state);

        let keys = |query: &str| {
            let app = app.clone();
            let query = query.to_owned();
            async move {
                let response = app
                    .oneshot(authorized(
                        "GET",
                        &format!("/api/v1/items?{query}"),
                        Body::empty(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{query}");
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body: JsonValue = serde_json::from_slice(&body).unwrap();
                body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["citation_key"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };

        assert_eq!(keys("q=type%3Aarticle+author%3Alovelace").await, ["early"]);
        assert_eq!(keys("q=year%3A1900..").await, ["late"]);
        assert_eq!(keys("q=-year%3A").await, ["other"]);
        assert_eq!(
            keys("sort=-year").await,
            ["late", "early", "other"],
            "items with no year sort last"
        );
        assert_eq!(
            keys("q=%22Two+Words%22").await,
            ["other"],
            "quoting makes whitespace literal"
        );
        assert!(
            keys("q=Two+Paper").await.is_empty(),
            "unquoted whitespace separates terms, and both must match"
        );

        for query in ["q=year%3Asoon", "sort=-"] {
            let response = app
                .clone()
                .oneshot(authorized(
                    "GET",
                    &format!("/api/v1/items?{query}"),
                    Body::empty(),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        }
    }

    /// A client that still speaks the old field names must be told so, not
    /// handed the whole library or a success it did not get.
    #[tokio::test]
    async fn renamed_filters_and_fields_are_rejected_rather_than_ignored() {
        let (_directory, state) = test_state();
        let app = native_router(state);

        for query in ["tag=keep", "type=article"] {
            let response = app
                .clone()
                .oneshot(authorized(
                    "GET",
                    &format!("/api/v1/items?{query}"),
                    Body::empty(),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{query} should be refused"
            );
        }

        // axum reports an unparseable body as 422 rather than 400; what matters
        // is that it is refused instead of silently changing nothing.
        let stale = json!({"tags": ["keep"]}).to_string();
        let mut patch = authorized("PATCH", "/api/v1/items/missing", Body::from(stale));
        patch
            .headers_mut()
            .insert(IF_MATCH, "\"anything\"".parse().unwrap());
        assert_eq!(
            app.oneshot(patch).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a body spelling collections `tags` should be refused"
        );
    }

    #[tokio::test]
    async fn cache_retains_last_valid_source_during_malformed_external_edit() {
        let (_directory, state) = test_state();
        let valid = "@misc{valid, lantaiid={cc9e50c4-55ee-4471-b17c-c41684f64bf9}}\n";
        std::fs::write(&state.inner.layout.bibliography, valid).unwrap();
        state.refresh().await;
        let valid_snapshot = state.snapshot().await;

        std::fs::write(&state.inner.layout.bibliography, "@misc{broken").unwrap();
        state.refresh().await;
        let degraded = state.snapshot().await;
        assert_eq!(degraded.source, valid_snapshot.source);
        assert_eq!(degraded.revision, valid_snapshot.revision);
        assert_eq!(degraded.items[0].citation_key, "valid");
        assert!(degraded.disk_error.is_some());

        let recovered = "@misc{recovered, lantaiid={5a45466b-d74f-4072-b026-dad615c7dcec}}\n";
        std::fs::write(&state.inner.layout.bibliography, recovered).unwrap();
        state.refresh().await;
        let recovered_snapshot = state.snapshot().await;
        assert_eq!(&*recovered_snapshot.source, recovered);
        assert_eq!(recovered_snapshot.items[0].citation_key, "recovered");
        assert!(recovered_snapshot.disk_error.is_none());
    }

    #[tokio::test]
    async fn watcher_canonicalizes_valid_external_edits_and_marks_malformed_edits() {
        let (_directory, state) = test_state();
        spawn_watcher(state.clone()).unwrap();
        std::fs::write(
            &state.inner.layout.bibliography,
            "% external\n@misc{external,title={Title},custom=\"a \" # {b}}\n",
        )
        .unwrap();

        let canonical = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let source = state.inner.layout.read_utf8().unwrap();
                let snapshot = state.snapshot().await;
                if source.contains("@misc{external,\n")
                    && source.contains("lantaiid = {")
                    && *snapshot.source == source
                {
                    break source;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("watcher did not canonicalize the external edit");
        assert!(canonical.contains("% external"));
        assert!(canonical.contains("custom = \"a \" # {b}"));
        assert!(state.snapshot().await.disk_error.is_none());

        std::fs::write(&state.inner.layout.bibliography, "@misc{broken").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if state.snapshot().await.disk_error.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("watcher did not mark the malformed edit");
        assert_eq!(&*state.snapshot().await.source, canonical);
    }

    #[tokio::test]
    async fn api_streams_managed_attachment_upload_download_and_delete() {
        let (_directory, state) = test_state();
        let item = state
            .store()
            .add_item(crate::library::NewItem {
                entry_type: "article".to_owned(),
                citation_key: Some("attachment-test".to_owned()),
                fields: vec![("title".to_owned(), "Attachment test".to_owned())],
            })
            .unwrap();
        state.refresh().await;
        let revision = state.snapshot().await.revision;
        let app = native_router(state.clone());
        let boundary = "lantai-test-boundary";
        let multipart = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nPaper\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"paper.pdf\"\r\nContent-Type: application/pdf\r\n\r\nPDF bytes\r\n--{boundary}--\r\n"
        );
        let mut upload = authorized(
            "POST",
            &format!("/api/v1/items/{}/attachments", item.uuid),
            Body::from(multipart),
        );
        upload.headers_mut().insert(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}")
                .parse()
                .unwrap(),
        );
        upload
            .headers_mut()
            .insert(IF_MATCH, format!("\"{revision}\"").parse().unwrap());

        let uploaded = app.clone().oneshot(upload).await.unwrap();
        assert_eq!(uploaded.status(), StatusCode::CREATED);
        let uploaded_etag = uploaded.headers()[ETAG].to_str().unwrap().to_owned();
        let body = to_bytes(uploaded.into_body(), usize::MAX).await.unwrap();
        let attached: JsonValue = serde_json::from_slice(&body).unwrap();
        let attachment_id = attached["attachment_uuid"].as_str().unwrap();
        assert_eq!(attached["title"], "Paper");

        let downloaded = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/api/v1/items/{}/attachments/{attachment_id}", item.uuid),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(downloaded.headers()[CONTENT_TYPE], "application/pdf");
        let body = to_bytes(downloaded.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"PDF bytes");

        let mut delete = authorized(
            "DELETE",
            &format!("/api/v1/items/{}/attachments/{attachment_id}", item.uuid),
            Body::empty(),
        );
        delete
            .headers_mut()
            .insert(IF_MATCH, uploaded_etag.parse().unwrap());
        let deleted = app.oneshot(delete).await.unwrap();
        assert_eq!(deleted.status(), StatusCode::OK);
        assert_eq!(state.store().trash_entries().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn api_imports_raw_biblatex_and_filters_canonical_export() {
        let (_directory, state) = test_state();
        let app = native_router(state);
        let health = app
            .clone()
            .oneshot(authorized("GET", "/api/v1/health", Body::empty()))
            .await
            .unwrap();
        let etag = health.headers()[ETAG].to_str().unwrap().to_owned();
        let import = json!({
            "source": concat!(
                "% imported\n",
                "@misc{first, title={First}, custom=\"a \" # {b}}\n",
                "@misc{second, title={Second}}\n"
            )
        });
        let mut request = authorized("POST", "/api/v1/import", Body::from(import.to_string()));
        request
            .headers_mut()
            .insert(IF_MATCH, etag.parse().unwrap());

        let imported = app.clone().oneshot(request).await.unwrap();
        assert_eq!(imported.status(), StatusCode::CREATED);
        let body = to_bytes(imported.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        let exported = app
            .oneshot(authorized("GET", "/api/v1/export?ids=first", Body::empty()))
            .await
            .unwrap();
        assert_eq!(exported.status(), StatusCode::OK);
        let body = to_bytes(exported.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("% imported"));
        assert!(body.contains("@misc{first,"));
        assert!(body.contains("custom = \"a \" # {b}"));
        assert!(!body.contains("@misc{second,"));
    }
}
