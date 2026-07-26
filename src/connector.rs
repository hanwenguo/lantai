use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use memmap2::MmapOptions;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value as JsonValue, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::catalog::Catalog;
use crate::collections;
use crate::config::Config;
use crate::hook::{HookItems, HookOperation, HookOrigin, PostSaveHook, PreparedPostSaveHook};
use crate::library::{LibraryLayout, LibraryStore, NewItem};
use crate::zotero::{ZoteroItem, map_item};
use crate::{Error, Result as LantaiResult};

const CONNECTOR_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 23119);
const CONNECTOR_API_VERSION: &str = "3";
const LIBRARY_NAME: &str = "Lantai";
const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
const BUSY_SESSION_TTL: Duration = Duration::from_secs(60);

type ConnectorResult<T> = std::result::Result<T, ConnectorError>;

#[derive(Clone)]
struct ConnectorState {
    inner: Arc<ConnectorStateInner>,
}

struct ConnectorStateInner {
    layout: LibraryLayout,
    attachment_limit_bytes: u64,
    sessions: Mutex<HashMap<String, SaveSession>>,
    /// Tag applied to new captures, chosen from the Connector's save popup.
    ///
    /// Zotero saves into whatever collection its window has selected, falling
    /// back to a remembered folder when closed. Lantai has no window, so the
    /// daemon remembers the last chosen target for the life of the process.
    selected_target: Mutex<Option<String>>,
    mutation: Mutex<()>,
    hook: PostSaveHook,
}

#[derive(Clone)]
struct SaveSession {
    created: Instant,
    action: SaveAction,
    items: HashMap<String, Uuid>,
    current_user_collections: Vec<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SaveAction {
    Items,
    Snapshot,
    StandaloneAttachment,
}

#[derive(Debug)]
struct ConnectorError {
    status: StatusCode,
    code: &'static str,
}

#[derive(Deserialize)]
struct SaveItemsRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    items: Vec<ZoteroItem>,
}

#[derive(Default, Deserialize)]
struct SessionQuery {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct AttachmentMetadata {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "contentType")]
    content_type: Option<String>,
    #[serde(default, rename = "parentItemID")]
    parent_item_id: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "sessionID")]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct SaveSnapshotRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    url: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct SaveSingleFileRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    items: Vec<SingleFileItem>,
}

#[derive(Deserialize)]
struct RawSaveSingleFileRequest<'a> {
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "snapshotContent", borrow)]
    snapshot_content: Option<&'a RawValue>,
    #[serde(default)]
    items: Vec<SingleFileItem>,
}

struct SingleFileUpload {
    request: SaveSingleFileRequest,
    snapshot: NamedTempFile,
    size: u64,
}

#[derive(Deserialize)]
struct SingleFileItem {
    id: String,
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    #[serde(rename = "sessionID")]
    session_id: String,
    target: String,
    #[serde(default)]
    tags: SessionTags,
    #[serde(default)]
    note: String,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum SessionTags {
    #[default]
    Empty,
    List(Vec<String>),
    CommaSeparated(String),
}

pub async fn serve(config: Config, layout: LibraryLayout, hook: PostSaveHook) -> LantaiResult<()> {
    let state = ConnectorState::new_with_hook(config.attachment_limit_bytes, layout, hook);
    let listener = tokio::net::TcpListener::bind(CONNECTOR_ADDRESS)
        .await
        .map_err(|source| Error::Listen {
            address: CONNECTOR_ADDRESS.to_string(),
            source,
        })?;
    println!("Lantai Zotero Connector endpoint listening on http://{CONNECTOR_ADDRESS}");
    axum::serve(listener, connector_router(state))
        .await
        .map_err(|source| Error::Listen {
            address: CONNECTOR_ADDRESS.to_string(),
            source,
        })
}

impl ConnectorState {
    #[cfg(test)]
    fn new(attachment_limit_bytes: u64, layout: LibraryLayout) -> Self {
        let hook = PostSaveHook::new(None, Path::new("config.toml"), layout.clone());
        Self::new_with_hook(attachment_limit_bytes, layout, hook)
    }

    fn new_with_hook(
        attachment_limit_bytes: u64,
        layout: LibraryLayout,
        hook: PostSaveHook,
    ) -> Self {
        Self {
            inner: Arc::new(ConnectorStateInner {
                layout,
                attachment_limit_bytes,
                sessions: Mutex::new(HashMap::new()),
                selected_target: Mutex::new(None),
                mutation: Mutex::new(()),
                hook,
            }),
        }
    }

    fn store(&self) -> LibraryStore {
        LibraryStore::new(self.inner.layout.clone())
    }

    /// Every collection in the library, deduped and ordered.
    async fn library_collections(&self) -> ConnectorResult<BTreeSet<String>> {
        let layout = self.inner.layout.clone();
        run_blocking(move || {
            let source = layout.read_utf8()?;
            let catalog = Catalog::parse(&layout.bibliography, &source)?;
            Ok(collections::of_items(
                catalog.items().map(|item| item.collections),
            ))
        })
        .await
        .map_err(|_| ConnectorError::internal("LIBRARY_READ_FAILED"))
    }

    async fn selected_target(&self) -> Option<String> {
        self.inner.selected_target.lock().await.clone()
    }

    async fn set_selected_target(&self, path: Option<String>) {
        *self.inner.selected_target.lock().await = path;
    }

    async fn reserve_session(&self, id: &str, action: SaveAction) -> ConnectorResult<()> {
        if id.trim().is_empty() {
            return Err(ConnectorError::bad_request("SESSION_ID_NOT_PROVIDED"));
        }
        let mut sessions = self.inner.sessions.lock().await;
        gc_sessions(&mut sessions);
        if sessions.contains_key(id) {
            return Err(ConnectorError::new(StatusCode::CONFLICT, "SESSION_EXISTS"));
        }
        sessions.insert(
            id.to_owned(),
            SaveSession {
                created: Instant::now(),
                action,
                items: HashMap::new(),
                current_user_collections: Vec::new(),
            },
        );
        Ok(())
    }

    /// Record the items a save produced.
    ///
    /// `applied` are the collections the save already wrote, so a later
    /// `updateSession` rebases them away instead of leaving a stale collection
    /// behind when the user picks a different one.
    async fn finish_session(&self, id: &str, items: HashMap<String, Uuid>, applied: Vec<String>) {
        if let Some(session) = self.inner.sessions.lock().await.get_mut(id) {
            session.items = items;
            session.current_user_collections = applied;
        }
    }

    async fn remove_session(&self, id: &str) {
        self.inner.sessions.lock().await.remove(id);
    }

    async fn session(&self, id: &str) -> ConnectorResult<SaveSession> {
        let mut sessions = self.inner.sessions.lock().await;
        gc_sessions(&mut sessions);
        sessions
            .get(id)
            .cloned()
            .ok_or_else(|| ConnectorError::bad_request("SESSION_NOT_FOUND"))
    }
}

fn connector_router(state: ConnectorState) -> Router {
    let body_limit = usize::try_from(state.inner.attachment_limit_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(1024 * 1024);
    Router::new()
        .route("/connector/ping", get(ping_get).post(ping_post))
        .route("/connector/saveItems", post(save_items))
        .route("/connector/saveAttachment", post(save_attachment))
        .route(
            "/connector/saveStandaloneAttachment",
            post(save_standalone_attachment),
        )
        .route("/connector/saveSnapshot", post(save_snapshot))
        .route("/connector/saveSingleFile", post(save_single_file))
        .route(
            "/connector/getSelectedCollection",
            post(get_selected_collection),
        )
        .route("/connector/updateSession", post(update_session))
        .route(
            "/connector/hasAttachmentResolvers",
            post(has_attachment_resolvers),
        )
        .route("/connector/delaySync", post(delay_sync))
        .route(
            "/connector/getClientHostnames",
            post(empty_compatibility_list),
        )
        .route("/connector/proxies", post(empty_compatibility_list))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn(connector_security))
        .with_state(state)
}

async fn connector_security(request: Request, next: Next) -> Response {
    let host_allowed = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(valid_connector_host);
    if !host_allowed {
        return connector_headers(ConnectorError::bad_request("INVALID_HOST").into_response());
    }

    let origin = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if request.method() == Method::OPTIONS {
        let response = if origin.as_deref() == Some("https://www.zotero.org") {
            StatusCode::NO_CONTENT.into_response()
        } else {
            ConnectorError::new(StatusCode::FORBIDDEN, "ORIGIN_NOT_ALLOWED").into_response()
        };
        return connector_cors(connector_headers(response), origin.as_deref());
    }

    let navigational_ping =
        request.method() == Method::GET && request.uri().path() == "/connector/ping";
    let allowed_request = request
        .headers()
        .contains_key("X-Zotero-Connector-API-Version")
        || request.headers().contains_key("Zotero-Allowed-Request");
    if !navigational_ping && !allowed_request {
        return connector_headers(
            ConnectorError::new(StatusCode::FORBIDDEN, "BROWSER_REQUEST_REJECTED").into_response(),
        );
    }
    if request.method() == Method::POST && !request.headers().contains_key(CONTENT_LENGTH) {
        return connector_headers(
            ConnectorError::bad_request("CONTENT_LENGTH_NOT_PROVIDED").into_response(),
        );
    }

    let response = next.run(request).await;
    connector_cors(connector_headers(response), origin.as_deref())
}

async fn ping_get() -> Html<&'static str> {
    Html("<!doctype html><html><body>Zotero is running (provided by Lantai)</body></html>")
}

async fn ping_post() -> Json<JsonValue> {
    Json(json!({
        "prefs": {
            "automaticSnapshots": true,
            "downloadAssociatedFiles": true,
            "supportsAttachmentUpload": true,
            "supportsTagsAutocomplete": true,
            "canUserAddNote": false,
            "googleDocsAddAnnotationEnabled": false,
            "googleDocsCitationExplorerEnabled": false,
            "reportActiveURL": false
        }
    }))
}

async fn save_items(
    State(state): State<ConnectorState>,
    headers: HeaderMap,
    body: Bytes,
) -> ConnectorResult<Response> {
    require_api_v3(&headers)?;
    let request: SaveItemsRequest = parse_json(&body)?;
    if request.items.is_empty() {
        return Err(ConnectorError::bad_request("ITEMS_NOT_PROVIDED"));
    }
    state
        .reserve_session(&request.session_id, SaveAction::Items)
        .await?;
    // saveItems carries no target, so the capture lands in whichever collection
    // the popup last selected. The popup only calls updateSession when the user
    // changes something, so this cannot wait until then.
    let collection = state.selected_target().await;
    let mapped = request
        .items
        .into_iter()
        .map(|mut item| {
            // Out of band from the item's own `tags`, which are the
            // translator's scraped keywords and are ignored.
            item.collections = collection.clone().into_iter().collect();
            map_item(item)
        })
        .collect::<LantaiResult<Vec<_>>>()
        .map_err(|_| ConnectorError::bad_request("INVALID_ITEM"));
    let mapped = match mapped {
        Ok(mapped) => mapped,
        Err(error) => {
            state.remove_session(&request.session_id).await;
            return Err(error);
        }
    };
    let mut keys = HashSet::new();
    if mapped
        .iter()
        .any(|item| !keys.insert(item.connector_id.clone()))
    {
        state.remove_session(&request.session_id).await;
        return Err(ConnectorError::bad_request("DUPLICATE_ITEM_ID"));
    }

    let _guard = state.inner.mutation.lock().await;
    let before = before_hook(&state);
    let store = state.store();
    let created = run_blocking(move || {
        let mut created = Vec::new();
        for mapped in mapped {
            match store.add_item(mapped.item) {
                Ok(item) => created.push((mapped.connector_id, item.uuid)),
                Err(error) => {
                    for (_, uuid) in &created {
                        let _ = store.remove_item(&uuid.to_string());
                    }
                    return Err(error);
                }
            }
        }
        Ok(created)
    })
    .await;
    let created = match created {
        Ok(created) => created,
        Err(_) => {
            state.remove_session(&request.session_id).await;
            return Err(ConnectorError::internal("SAVE_FAILED"));
        }
    };
    let affected = created.iter().map(|(_, uuid)| *uuid).collect();
    state
        .finish_session(
            &request.session_id,
            created.into_iter().collect(),
            collection.into_iter().collect(),
        )
        .await;
    let prepared = state.inner.hook.prepare(
        before,
        HookOperation::ItemCreate,
        HookOrigin::Connector,
        HookItems::Uuids(affected),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(empty_response(StatusCode::CREATED, "application/json"))
}

async fn save_attachment(
    State(state): State<ConnectorState>,
    Query(query): Query<SessionQuery>,
    headers: HeaderMap,
    body: Body,
) -> ConnectorResult<Response> {
    let metadata = attachment_metadata(&headers)?;
    let session_id = metadata
        .session_id
        .as_deref()
        .or(query.session_id.as_deref())
        .ok_or_else(|| ConnectorError::bad_request("SESSION_ID_NOT_PROVIDED"))?;
    let session = state.session(session_id).await?;
    let parent_id = metadata
        .parent_item_id
        .as_deref()
        .ok_or_else(|| ConnectorError::bad_request("PARENT_ITEM_ID_NOT_PROVIDED"))?;
    let parent = session
        .items
        .get(parent_id)
        .copied()
        .ok_or_else(|| ConnectorError::bad_request("ITEM_NOT_FOUND"))?;
    let temporary = stream_upload(body, &headers, state.inner.attachment_limit_bytes).await?;
    let content_type = request_content_type(&headers, metadata.content_type.as_deref());
    let title = decode_rfc2047_q(&metadata.title);
    let source_name = attachment_source_name(&metadata.url, &title, &content_type);
    let limit = state.inner.attachment_limit_bytes;
    let store = state.store();
    let _guard = state.inner.mutation.lock().await;
    let before = before_hook(&state);
    run_blocking(move || {
        store.attach_file_named(
            &parent.to_string(),
            temporary.path(),
            &source_name,
            Some(&title),
            Some(&content_type),
            limit,
        )
    })
    .await
    .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    let prepared = state.inner.hook.prepare(
        before,
        HookOperation::AttachmentCreate,
        HookOrigin::Connector,
        HookItems::Uuids(vec![parent]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(StatusCode::CREATED.into_response())
}

async fn save_standalone_attachment(
    State(state): State<ConnectorState>,
    Query(query): Query<SessionQuery>,
    headers: HeaderMap,
    body: Body,
) -> ConnectorResult<Response> {
    let metadata = attachment_metadata(&headers)?;
    let session_id = metadata
        .session_id
        .as_deref()
        .or(query.session_id.as_deref())
        .ok_or_else(|| ConnectorError::bad_request("SESSION_ID_NOT_PROVIDED"))?;
    state
        .reserve_session(session_id, SaveAction::StandaloneAttachment)
        .await?;
    let temporary = match stream_upload(body, &headers, state.inner.attachment_limit_bytes).await {
        Ok(temporary) => temporary,
        Err(error) => {
            state.remove_session(session_id).await;
            return Err(error);
        }
    };
    let content_type = request_content_type(&headers, metadata.content_type.as_deref());
    let title = decode_rfc2047_q(&metadata.title);
    let title = if title.trim().is_empty() {
        metadata.url.clone()
    } else {
        title
    };
    let source_name = attachment_source_name(&metadata.url, &title, &content_type);
    let collection = state.selected_target().await;
    let mut fields = [
        ("title".to_owned(), title.clone()),
        ("url".to_owned(), metadata.url.clone()),
        ("zotero-item-type".to_owned(), "attachment".to_owned()),
    ]
    .into_iter()
    .filter(|(_, value)| !value.is_empty())
    .collect::<Vec<_>>();
    if let Some(collection) = &collection {
        fields.push(("keywords".to_owned(), collection.clone()));
    }
    let limit = state.inner.attachment_limit_bytes;
    let store = state.store();
    let _guard = state.inner.mutation.lock().await;
    let before = before_hook(&state);
    let created = run_blocking(move || {
        let item = store.add_item(NewItem {
            entry_type: "misc".to_owned(),
            citation_key: None,
            fields,
        })?;
        if let Err(error) = store.attach_file_named(
            &item.uuid.to_string(),
            temporary.path(),
            &source_name,
            Some(&title),
            Some(&content_type),
            limit,
        ) {
            let _ = store.remove_item(&item.uuid.to_string());
            return Err(error);
        }
        Ok(item.uuid)
    })
    .await;
    let item_uuid = match created {
        Ok(uuid) => uuid,
        Err(_) => {
            state.remove_session(session_id).await;
            return Err(ConnectorError::internal("ATTACHMENT_SAVE_FAILED"));
        }
    };
    let mut items = HashMap::new();
    items.insert(metadata.url.clone(), item_uuid);
    if let Some(id) = metadata.id {
        items.insert(id, item_uuid);
    }
    state
        .finish_session(session_id, items, collection.into_iter().collect())
        .await;
    let prepared = state.inner.hook.prepare(
        before,
        HookOperation::ItemCreate,
        HookOrigin::Connector,
        HookItems::Uuids(vec![item_uuid]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok((StatusCode::CREATED, Json(json!({ "canRecognize": false }))).into_response())
}

async fn save_snapshot(
    State(state): State<ConnectorState>,
    headers: HeaderMap,
    body: Bytes,
) -> ConnectorResult<Response> {
    require_api_v3(&headers)?;
    let request: SaveSnapshotRequest = parse_json(&body)?;
    state
        .reserve_session(&request.session_id, SaveAction::Snapshot)
        .await?;
    let title = if request.title.trim().is_empty() {
        request.url.clone()
    } else {
        request.title
    };
    let url = request.url.clone();
    let collection = state.selected_target().await;
    let mut fields = vec![
        ("title".to_owned(), title),
        ("url".to_owned(), url),
        ("zotero-item-type".to_owned(), "webpage".to_owned()),
    ];
    if let Some(collection) = &collection {
        fields.push(("keywords".to_owned(), collection.clone()));
    }
    let store = state.store();
    let _guard = state.inner.mutation.lock().await;
    let before = before_hook(&state);
    let created = run_blocking(move || {
        store.add_item(NewItem {
            entry_type: "online".to_owned(),
            citation_key: None,
            fields,
        })
    })
    .await;
    let created = match created {
        Ok(created) => created,
        Err(_) => {
            state.remove_session(&request.session_id).await;
            return Err(ConnectorError::internal("SAVE_FAILED"));
        }
    };
    state
        .finish_session(
            &request.session_id,
            HashMap::from([(request.url, created.uuid)]),
            collection.into_iter().collect(),
        )
        .await;
    let prepared = state.inner.hook.prepare(
        before,
        HookOperation::ItemCreate,
        HookOrigin::Connector,
        HookItems::Uuids(vec![created.uuid]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(empty_response(StatusCode::CREATED, "application/json"))
}

async fn save_single_file(
    State(state): State<ConnectorState>,
    headers: HeaderMap,
    body: Body,
) -> ConnectorResult<Response> {
    let limit = state.inner.attachment_limit_bytes;
    let envelope_limit = limit.saturating_add(1024 * 1024);
    let encoded = stream_upload(body, &headers, envelope_limit).await?;
    let upload = tokio::task::spawn_blocking(move || parse_single_file_upload(encoded, limit))
        .await
        .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))??;
    let request = upload.request;
    let session = state.session(&request.session_id).await?;
    if upload.size == 0 {
        return Ok(StatusCode::CREATED.into_response());
    }
    let connector_key = if session.action == SaveAction::Snapshot {
        request.url.as_str()
    } else {
        request
            .items
            .first()
            .map(|item| item.id.as_str())
            .ok_or_else(|| ConnectorError::bad_request("PARENT_ITEM_ID_NOT_PROVIDED"))?
    };
    let parent = session
        .items
        .get(connector_key)
        .copied()
        .ok_or_else(|| ConnectorError::bad_request("ITEM_NOT_FOUND"))?;
    let title = if request.title.trim().is_empty() {
        "Snapshot".to_owned()
    } else {
        request.title
    };
    let source_name = format!(
        "{}.html",
        crate::attachments::sanitize_filename(Path::new(&title))
    );
    let temporary = upload.snapshot;
    let store = state.store();
    let _guard = state.inner.mutation.lock().await;
    let before = before_hook(&state);
    run_blocking(move || {
        store.attach_file_named(
            &parent.to_string(),
            temporary.path(),
            &source_name,
            Some(&title),
            Some("text/html"),
            limit,
        )
    })
    .await
    .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    let prepared = state.inner.hook.prepare(
        before,
        HookOperation::AttachmentCreate,
        HookOrigin::Connector,
        HookItems::Uuids(vec![parent]),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(StatusCode::CREATED.into_response())
}

async fn get_selected_collection(
    State(state): State<ConnectorState>,
) -> ConnectorResult<Json<JsonValue>> {
    let library = state.library_collections().await?;
    let selected = state.selected_target().await;
    let tree = collections::tree(library.iter().cloned());

    // The popup preselects this row and offers recent targets first.
    let current = selected
        .as_deref()
        .and_then(|path| tree.iter().find(|target| target.path == path));
    let mut targets = vec![json!({
        "id": collections::LIBRARY_TARGET,
        "name": LIBRARY_NAME,
        "filesEditable": true,
        "level": 0,
    })];
    targets.extend(tree.iter().map(|target| {
        let mut row = json!({
            "id": target.id,
            "name": target.name,
            "filesEditable": true,
            "level": target.level,
        });
        if current.is_some_and(|current| current.id == target.id) {
            row["recent"] = JsonValue::Bool(true);
        }
        row
    }));

    // `tag` and `tags` are Zotero's names on this wire, not Lantai's: the popup
    // autocompletes a flat name list, which for Lantai is the collection set.
    let tags = library
        .into_iter()
        .map(|collection| json!({ "tag": collection, "type": 0 }))
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "libraryID": 1,
        "libraryName": LIBRARY_NAME,
        "libraryEditable": true,
        "filesEditable": true,
        "editable": true,
        "id": current.map(|target| target.id.clone()),
        "name": current.map_or(LIBRARY_NAME, |target| target.name.as_str()),
        "targets": targets,
        // Keyed by library: the popup resolves a collection to its root target
        // before looking up the name list for autocomplete.
        "tags": {collections::LIBRARY_TARGET: tags}
    })))
}

async fn update_session(
    State(state): State<ConnectorState>,
    body: Bytes,
) -> ConnectorResult<Json<JsonValue>> {
    let request: UpdateSessionRequest = parse_json(&body)?;
    if !request.note.trim().is_empty() {
        return Err(ConnectorError::bad_request("NOTES_NOT_SUPPORTED"));
    }
    // The library root files an item under no collection at all; any other
    // target names a collection, which the popup's own entries then join.
    let collection = if request.target == collections::LIBRARY_TARGET {
        None
    } else {
        let library = state.library_collections().await?;
        Some(
            collections::resolve(library, &request.target)
                .ok_or_else(|| ConnectorError::bad_request("TARGET_NOT_FOUND"))?,
        )
    };
    let mut replacement = normalize_session_tags(request.tags);
    if let Some(collection) = &collection
        && !replacement
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(collection))
    {
        replacement.push(collection.clone());
    }
    let session = state.session(&request.session_id).await?;
    let previous = session.current_user_collections;
    let item_ids = session.items.values().copied().collect::<BTreeSet<_>>();
    let affected = item_ids.iter().copied().collect::<Vec<_>>();
    let store = state.store();
    let replacement_for_write = replacement.clone();
    let _guard = state.inner.mutation.lock().await;
    let before = before_hook(&state);
    run_blocking(move || {
        for item_id in item_ids {
            store.rebase_collections(&item_id.to_string(), &previous, &replacement_for_write)?;
        }
        Ok(())
    })
    .await
    .map_err(|_| ConnectorError::internal("SESSION_UPDATE_FAILED"))?;
    if let Some(session) = state
        .inner
        .sessions
        .lock()
        .await
        .get_mut(&request.session_id)
    {
        session.current_user_collections = replacement;
    }
    state.set_selected_target(collection).await;
    let prepared = state.inner.hook.prepare(
        before,
        HookOperation::ItemUpdate,
        HookOrigin::Connector,
        HookItems::Uuids(affected),
        Vec::new(),
    );
    drop(_guard);
    run_prepared_hook(prepared).await;
    Ok(Json(json!({})))
}

async fn has_attachment_resolvers() -> Json<bool> {
    Json(false)
}

async fn delay_sync() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn empty_compatibility_list() -> Json<Vec<JsonValue>> {
    Json(Vec::new())
}

fn require_api_v3(headers: &HeaderMap) -> ConnectorResult<()> {
    let version = headers
        .get("X-Zotero-Connector-API-Version")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    if version >= 3 {
        Ok(())
    } else {
        Err(ConnectorError::bad_request("CONNECTOR_VERSION_OUTDATED"))
    }
}

fn attachment_metadata(headers: &HeaderMap) -> ConnectorResult<AttachmentMetadata> {
    let value = headers
        .get("X-Metadata")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ConnectorError::bad_request("METADATA_NOT_PROVIDED"))?;
    serde_json::from_str(value).map_err(|_| ConnectorError::bad_request("INVALID_METADATA"))
}

async fn stream_upload(
    body: Body,
    headers: &HeaderMap,
    limit: u64,
) -> ConnectorResult<NamedTempFile> {
    if headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit)
    {
        return Err(ConnectorError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "ATTACHMENT_TOO_LARGE",
        ));
    }
    let mut temporary =
        NamedTempFile::new().map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    let mut size = 0_u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ConnectorError::bad_request("INVALID_UPLOAD"))?;
        size = size.saturating_add(chunk.len() as u64);
        if size > limit {
            return Err(ConnectorError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "ATTACHMENT_TOO_LARGE",
            ));
        }
        temporary
            .write_all(&chunk)
            .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    }
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    Ok(temporary)
}

fn request_content_type(headers: &HeaderMap, metadata: Option<&str>) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .or(metadata)
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .trim()
        .to_owned()
}

fn attachment_source_name(url: &str, title: &str, content_type: &str) -> String {
    let candidate = url
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(title);
    let mut candidate = crate::attachments::sanitize_filename(Path::new(candidate));
    if Path::new(&candidate).extension().is_none()
        && let Some(extension) = mime_guess::get_mime_extensions_str(content_type)
            .and_then(|extensions| extensions.first())
    {
        candidate.push('.');
        candidate.push_str(extension);
    }
    candidate
}

fn normalize_session_tags(tags: SessionTags) -> Vec<String> {
    let tags = match tags {
        SessionTags::Empty => Vec::new(),
        SessionTags::List(tags) => tags,
        SessionTags::CommaSeparated(tags) => tags.split(',').map(str::to_owned).collect(),
    };
    let mut seen = HashSet::new();
    let mut tags = tags
        .into_iter()
        .map(|tag| tag.trim().to_owned())
        .filter(|tag| !tag.is_empty())
        .filter(|tag| seen.insert(tag.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    tags.sort_by_key(|tag| tag.to_ascii_lowercase());
    tags
}

fn decode_rfc2047_q(value: &str) -> String {
    let Some(encoded) = value
        .strip_prefix("=?UTF-8?Q?")
        .or_else(|| value.strip_prefix("=?utf-8?q?"))
        .and_then(|value| value.strip_suffix("?="))
    else {
        return value.to_owned();
    };
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'_' {
            decoded.push(b' ');
            index += 1;
        } else if bytes[index] == b'=' && index + 2 < bytes.len() {
            let pair = &encoded[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(pair, 16) {
                decoded.push(byte);
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| value.to_owned())
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> ConnectorResult<T> {
    serde_json::from_slice(body).map_err(|_| ConnectorError::bad_request("INVALID_JSON"))
}

fn parse_single_file_upload(
    encoded: NamedTempFile,
    limit: u64,
) -> ConnectorResult<SingleFileUpload> {
    // SAFETY: the completed temporary file is not mutated while the read-only mapping exists.
    let mapping = unsafe { MmapOptions::new().map(encoded.as_file()) }
        .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    let raw: RawSaveSingleFileRequest<'_> = serde_json::from_slice(&mapping)
        .map_err(|_| ConnectorError::bad_request("INVALID_JSON"))?;
    let mut snapshot =
        NamedTempFile::new().map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    let size = match raw.snapshot_content {
        Some(value) => decode_json_string(value.get(), snapshot.as_file_mut(), limit)?,
        None => 0,
    };
    snapshot
        .as_file_mut()
        .sync_all()
        .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))?;
    Ok(SingleFileUpload {
        request: SaveSingleFileRequest {
            session_id: raw.session_id,
            url: raw.url,
            title: raw.title,
            items: raw.items,
        },
        snapshot,
        size,
    })
}

fn decode_json_string(raw: &str, writer: &mut impl Write, limit: u64) -> ConnectorResult<u64> {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return Err(ConnectorError::bad_request("INVALID_JSON"));
    }
    let mut index = 1;
    let end = bytes.len() - 1;
    let mut written = 0_u64;
    while index < end {
        if bytes[index] != b'\\' {
            let start = index;
            while index < end && bytes[index] != b'\\' {
                index += 1;
            }
            write_snapshot_bytes(writer, &bytes[start..index], &mut written, limit)?;
            continue;
        }

        index += 1;
        if index >= end {
            return Err(ConnectorError::bad_request("INVALID_JSON"));
        }
        let escaped = bytes[index];
        index += 1;
        match escaped {
            b'"' | b'\\' | b'/' => {
                write_snapshot_bytes(writer, &[escaped], &mut written, limit)?;
            }
            b'b' => write_snapshot_bytes(writer, &[8], &mut written, limit)?,
            b'f' => write_snapshot_bytes(writer, &[12], &mut written, limit)?,
            b'n' => write_snapshot_bytes(writer, b"\n", &mut written, limit)?,
            b'r' => write_snapshot_bytes(writer, b"\r", &mut written, limit)?,
            b't' => write_snapshot_bytes(writer, b"\t", &mut written, limit)?,
            b'u' => {
                let first = decode_hex_quad(bytes, &mut index, end)?;
                let scalar = if (0xD800..=0xDBFF).contains(&first) {
                    if index + 6 > end || bytes[index] != b'\\' || bytes[index + 1] != b'u' {
                        return Err(ConnectorError::bad_request("INVALID_JSON"));
                    }
                    index += 2;
                    let second = decode_hex_quad(bytes, &mut index, end)?;
                    if !(0xDC00..=0xDFFF).contains(&second) {
                        return Err(ConnectorError::bad_request("INVALID_JSON"));
                    }
                    0x1_0000 + (((first - 0xD800) as u32) << 10) + (second - 0xDC00) as u32
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(ConnectorError::bad_request("INVALID_JSON"));
                } else {
                    first as u32
                };
                let character = char::from_u32(scalar)
                    .ok_or_else(|| ConnectorError::bad_request("INVALID_JSON"))?;
                let mut buffer = [0_u8; 4];
                write_snapshot_bytes(
                    writer,
                    character.encode_utf8(&mut buffer).as_bytes(),
                    &mut written,
                    limit,
                )?;
            }
            _ => return Err(ConnectorError::bad_request("INVALID_JSON")),
        }
    }
    Ok(written)
}

fn decode_hex_quad(bytes: &[u8], index: &mut usize, end: usize) -> ConnectorResult<u16> {
    if *index + 4 > end {
        return Err(ConnectorError::bad_request("INVALID_JSON"));
    }
    let mut value = 0_u16;
    for byte in &bytes[*index..*index + 4] {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(ConnectorError::bad_request("INVALID_JSON")),
        };
        value = (value << 4) | u16::from(digit);
    }
    *index += 4;
    Ok(value)
}

fn write_snapshot_bytes(
    writer: &mut impl Write,
    bytes: &[u8],
    written: &mut u64,
    limit: u64,
) -> ConnectorResult<()> {
    *written = written.checked_add(bytes.len() as u64).ok_or_else(|| {
        ConnectorError::new(StatusCode::PAYLOAD_TOO_LARGE, "ATTACHMENT_TOO_LARGE")
    })?;
    if *written > limit {
        return Err(ConnectorError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "ATTACHMENT_TOO_LARGE",
        ));
    }
    writer
        .write_all(bytes)
        .map_err(|_| ConnectorError::internal("ATTACHMENT_SAVE_FAILED"))
}

async fn run_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> LantaiResult<T> + Send + 'static,
) -> LantaiResult<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| Error::Daemon {
            address: CONNECTOR_ADDRESS.to_string(),
            message: error.to_string(),
        })?
}

fn before_hook(state: &ConnectorState) -> Option<String> {
    match state.inner.hook.revision_before_save() {
        Ok(revision) => revision,
        Err(error) => {
            eprintln!("warning: could not prepare post-save hook: {error}");
            None
        }
    }
}

async fn run_prepared_hook(prepared: Option<PreparedPostSaveHook>) {
    if let Some(prepared) = prepared
        && let Err(error) = tokio::task::spawn_blocking(move || prepared.run()).await
    {
        eprintln!("warning: post-save hook task failed: {error}");
    }
}

fn gc_sessions(sessions: &mut HashMap<String, SaveSession>) {
    let ttl = if sessions.len() >= 10 {
        BUSY_SESSION_TTL
    } else {
        SESSION_TTL
    };
    sessions.retain(|_, session| session.created.elapsed() < ttl);
}

fn valid_connector_host(host: &str) -> bool {
    for allowed in ["127.0.0.1", "localhost", "[::1]"] {
        if host == allowed {
            return true;
        }
        if let Some(port) = host
            .strip_prefix(allowed)
            .and_then(|value| value.strip_prefix(':'))
            && !port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

fn connector_headers(mut response: Response) -> Response {
    response.headers_mut().insert(
        "X-Zotero-Version",
        HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    response.headers_mut().insert(
        "X-Zotero-Connector-API-Version",
        HeaderValue::from_static(CONNECTOR_API_VERSION),
    );
    response
}

fn connector_cors(mut response: Response, origin: Option<&str>) -> Response {
    if origin == Some("https://www.zotero.org") {
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("https://www.zotero.org"),
        );
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(
                "Content-Type, X-Zotero-Connector-API-Version, X-Zotero-Version, X-Metadata",
            ),
        );
    }
    response
}

fn empty_response(status: StatusCode, content_type: &'static str) -> Response {
    let mut response = status.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

impl ConnectorError {
    const fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    const fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }

    const fn internal(code: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code)
    }
}

impl IntoResponse for ConnectorError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{HttpBody, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn connector_save_runs_one_batched_hook_event() {
        let directory = tempfile::tempdir().unwrap();
        let layout = LibraryLayout::new(directory.path().join("references.bib")).unwrap();
        layout.initialize().unwrap();
        let event_path = directory.path().join("event.json");
        let config = crate::config::PostSaveHookConfig {
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "cat > \"$1\"".to_owned(),
                "lantai-hook".to_owned(),
                event_path.display().to_string(),
            ],
            timeout_seconds: 30,
        };
        let hook = PostSaveHook::new(
            Some(&config),
            &directory.path().join("config.toml"),
            layout.clone(),
        );
        let state = ConnectorState::new_with_hook(1024 * 1024, layout, hook);
        let app = connector_router(state);
        let body = json!({
            "sessionID": "hook-session",
            "items": [
                {"id": "one", "itemType": "book", "title": "One"},
                {"id": "two", "itemType": "book", "title": "Two"}
            ]
        });

        let response = app
            .oneshot(request(
                "POST",
                "/connector/saveItems",
                "application/json",
                body.to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let event: JsonValue =
            serde_json::from_str(&std::fs::read_to_string(event_path).unwrap()).unwrap();
        assert_eq!(event["origin"], "connector");
        assert_eq!(event["operation"], "item.create");
        assert_eq!(event["items"].as_array().unwrap().len(), 2);
    }

    fn test_state() -> (tempfile::TempDir, ConnectorState) {
        let directory = tempfile::tempdir().unwrap();
        let layout = LibraryLayout::new(directory.path().join("references.bib")).unwrap();
        layout.initialize().unwrap();
        (directory, ConnectorState::new(1024 * 1024, layout))
    }

    fn request(
        method: &str,
        uri: &str,
        content_type: &str,
        body: impl Into<Body>,
    ) -> Request<Body> {
        let body = body.into();
        let length = body.size_hint().exact().unwrap_or(0);
        Request::builder()
            .method(method)
            .uri(uri)
            .header(HOST, "127.0.0.1:23119")
            .header("X-Zotero-Version", "9.0.0")
            .header("X-Zotero-Connector-API-Version", "3")
            .header(CONTENT_TYPE, content_type)
            .header(CONTENT_LENGTH, length)
            .body(body)
            .unwrap()
    }

    #[test]
    fn single_file_json_is_disk_backed_and_decodes_escapes_with_a_hard_limit() {
        let body = concat!(
            r#"{"sessionID":"session","url":"https://example.com","#,
            r#""snapshotContent":"<p title=\"x\">line\n\u00e9 \uD83D\uDE00</p>","#,
            r#""items":[{"id":"parent"}]}"#
        );
        let mut encoded = NamedTempFile::new().unwrap();
        encoded.write_all(body.as_bytes()).unwrap();
        encoded.as_file_mut().sync_all().unwrap();
        let expected = "<p title=\"x\">line\né 😀</p>";

        let upload = parse_single_file_upload(encoded, expected.len() as u64).unwrap();

        assert_eq!(upload.request.session_id, "session");
        assert_eq!(upload.request.items[0].id, "parent");
        assert_eq!(upload.size, expected.len() as u64);
        assert_eq!(
            std::fs::read(upload.snapshot.path()).unwrap(),
            expected.as_bytes()
        );

        let mut encoded = NamedTempFile::new().unwrap();
        encoded.write_all(body.as_bytes()).unwrap();
        encoded.as_file_mut().sync_all().unwrap();
        let error = match parse_single_file_upload(encoded, expected.len() as u64 - 1) {
            Ok(_) => panic!("oversized decoded snapshot was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn ping_enforces_loopback_browser_filter_and_response_headers() {
        let (_directory, state) = test_state();
        let app = connector_router(state);

        let navigation = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/connector/ping")
                    .header(HOST, "localhost:23119")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(navigation.status(), StatusCode::OK);
        assert_eq!(navigation.headers()["X-Zotero-Connector-API-Version"], "3");

        let rejected = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/connector/ping")
                    .header(HOST, "127.0.0.1:23119")
                    .header(CONTENT_LENGTH, "2")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

        let rebound = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/connector/ping")
                    .header(HOST, "attacker.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rebound.status(), StatusCode::BAD_REQUEST);

        let ping = app
            .oneshot(request("POST", "/connector/ping", "application/json", "{}"))
            .await
            .unwrap();
        assert_eq!(ping.status(), StatusCode::OK);
        let body = to_bytes(ping.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["prefs"]["supportsAttachmentUpload"], true);
        assert_eq!(body["prefs"]["canUserAddNote"], false);
    }

    #[tokio::test]
    async fn translated_item_attachment_snapshot_and_collections_follow_one_session() {
        let (_directory, state) = test_state();
        let app = connector_router(state.clone());
        let save = json!({
            "sessionID": "session-1",
            "uri": "https://example.com/article",
            "items": [{
                "id": "parent-1",
                "itemType": "journalArticle",
                "title": "A Sketch",
                "creators": [{
                    "firstName": "Ada",
                    "lastName": "Lovelace",
                    "creatorType": "author"
                }],
                "date": "1843",
                "url": "https://example.com/article",
                "tags": [{"tag": "automatic", "type": 1}],
                "attachments": []
            }]
        })
        .to_string();
        let saved = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/saveItems",
                "application/json",
                save,
            ))
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::CREATED);

        let metadata = json!({
            "id": "attachment-1",
            "url": "https://example.com/paper.pdf",
            "contentType": "application/pdf",
            "parentItemID": "parent-1",
            "title": "=?UTF-8?Q?M=C3=A9moire?="
        })
        .to_string();
        let mut attachment = request(
            "POST",
            "/connector/saveAttachment?sessionID=session-1",
            "application/pdf",
            Body::from("PDF bytes"),
        );
        attachment
            .headers_mut()
            .insert("X-Metadata", metadata.parse().unwrap());
        let attached = app.clone().oneshot(attachment).await.unwrap();
        assert_eq!(attached.status(), StatusCode::CREATED);

        let single_file = json!({
            "sessionID": "session-1",
            "url": "https://example.com/article",
            "title": "A Sketch",
            "snapshotContent": "<!doctype html><title>A Sketch</title>",
            "items": [{"id": "parent-1"}]
        })
        .to_string();
        let snapshot = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/saveSingleFile",
                "application/json",
                single_file,
            ))
            .await
            .unwrap();
        assert_eq!(snapshot.status(), StatusCode::CREATED);

        let update = json!({
            "sessionID": "session-1",
            "target": "L1",
            "tags": ["manual"],
            "note": ""
        })
        .to_string();
        let updated = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/updateSession",
                "application/json",
                update,
            ))
            .await
            .unwrap();
        assert_eq!(updated.status(), StatusCode::OK);

        let source = state.inner.layout.read_utf8().unwrap();
        let catalog = Catalog::parse(&state.inner.layout.bibliography, &source).unwrap();
        let item = catalog.find("lovelace1843sketch").unwrap();
        // The translator's automatic keyword is dropped; only what the user
        // typed in the popup becomes a collection.
        assert_eq!(item.collections, vec!["manual"]);
        assert_eq!(item.attachments.len(), 2);
        assert_eq!(item.attachments[0].title, "Mémoire");
        assert_eq!(
            std::fs::read(
                state
                    .inner
                    .layout
                    .bibliography
                    .parent()
                    .unwrap()
                    .join(&item.attachments[0].path)
            )
            .unwrap(),
            b"PDF bytes"
        );

        let target = app
            .oneshot(request(
                "POST",
                "/connector/getSelectedCollection",
                "application/json",
                "{}",
            ))
            .await
            .unwrap();
        let body = to_bytes(target.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["targets"][0]["id"], "L1");
    }

    /// A library whose tags form a nested collection tree.
    fn filed_state() -> (tempfile::TempDir, ConnectorState) {
        let (directory, state) = test_state();
        std::fs::write(
            &state.inner.layout.bibliography,
            concat!(
                "@book{seed,\n",
                "  title = {Seed},\n",
                "  keywords = {Inbox, Projects/IfT, ResearchTopics/Subtyping/Semantic},\n",
                "  lantaiid = {cc9e50c4-55ee-4471-b17c-c41684f64bf9}\n",
                "}\n"
            ),
        )
        .unwrap();
        (directory, state)
    }

    async fn selected_collection(app: &Router) -> JsonValue {
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/getSelectedCollection",
                "application/json",
                "{}",
            ))
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn save_one(app: &Router, session: &str, title: &str) -> StatusCode {
        let body = json!({
            "sessionID": session,
            "items": [{"id": "one", "itemType": "book", "title": title}]
        })
        .to_string();
        app.clone()
            .oneshot(request(
                "POST",
                "/connector/saveItems",
                "application/json",
                body,
            ))
            .await
            .unwrap()
            .status()
    }

    async fn retarget(app: &Router, session: &str, target: &str, tags: JsonValue) -> StatusCode {
        let body = json!({
            "sessionID": session,
            "target": target,
            "tags": tags,
            "note": ""
        })
        .to_string();
        app.clone()
            .oneshot(request(
                "POST",
                "/connector/updateSession",
                "application/json",
                body,
            ))
            .await
            .unwrap()
            .status()
    }

    fn collections_of(state: &ConnectorState, title: &str) -> Vec<String> {
        let source = state.inner.layout.read_utf8().unwrap();
        let catalog = Catalog::parse(&state.inner.layout.bibliography, &source).unwrap();
        catalog
            .views()
            .find(|item| item.title.as_deref() == Some(title))
            .unwrap_or_else(|| panic!("no item titled {title}"))
            .collections
    }

    #[tokio::test]
    async fn collections_are_offered_as_a_nested_tree() {
        let (_directory, state) = filed_state();
        let app = connector_router(state);

        let body = selected_collection(&app).await;
        let rows = body["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["name"].as_str().unwrap(),
                    row["level"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("Lantai", 0),
                ("Inbox", 1),
                // No item belongs to either parent directly; both are
                // synthesized so the popup can find each row's parent by
                // scanning back one level.
                ("Projects", 1),
                ("IfT", 2),
                ("ResearchTopics", 1),
                ("Subtyping", 2),
                ("Semantic", 3),
            ]
        );
        assert!(
            body["targets"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["filesEditable"] == true),
            "the popup drops targets without filesEditable"
        );
        assert_eq!(body["id"], JsonValue::Null);
        assert_eq!(body["name"], "Lantai");
        // `tags`/`tag` are Zotero's protocol names for the flat autocomplete
        // list, which for Lantai is the collection set.
        assert_eq!(body["tags"]["L1"][0]["tag"], "Inbox");
    }

    #[tokio::test]
    async fn choosing_a_collection_files_the_session_and_retargeting_moves_it() {
        let (_directory, state) = filed_state();
        let app = connector_router(state.clone());
        let targets = selected_collection(&app).await;
        let id = |name: &str| {
            targets["targets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == name)
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };

        assert_eq!(save_one(&app, "s1", "Filed").await, StatusCode::CREATED);
        assert_eq!(
            retarget(&app, "s1", &id("IfT"), json!(["manual"])).await,
            StatusCode::OK
        );
        assert_eq!(collections_of(&state, "Filed"), ["manual", "Projects/IfT"]);

        // Switching targets moves the item rather than accumulating memberships.
        assert_eq!(
            retarget(&app, "s1", &id("Semantic"), json!(["manual"])).await,
            StatusCode::OK
        );
        assert_eq!(
            collections_of(&state, "Filed"),
            ["manual", "ResearchTopics/Subtyping/Semantic"]
        );

        // The library root files the item under no collection.
        assert_eq!(
            retarget(&app, "s1", "L1", json!(["manual"])).await,
            StatusCode::OK
        );
        assert_eq!(collections_of(&state, "Filed"), ["manual"]);
    }

    #[tokio::test]
    async fn the_chosen_collection_is_remembered_for_the_next_capture() {
        let (_directory, state) = filed_state();
        let app = connector_router(state.clone());
        let targets = selected_collection(&app).await;
        let inbox = targets["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "Inbox")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        assert_eq!(save_one(&app, "s1", "First").await, StatusCode::CREATED);
        assert_eq!(
            retarget(&app, "s1", &inbox, json!([])).await,
            StatusCode::OK
        );

        // The popup now preselects Inbox and marks it recent.
        let body = selected_collection(&app).await;
        assert_eq!(body["id"], JsonValue::String(inbox.clone()));
        assert_eq!(body["name"], "Inbox");
        let inbox_row = body["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == JsonValue::String(inbox.clone()))
            .unwrap()
            .clone();
        assert_eq!(inbox_row["recent"], true);

        // A later capture lands there without any popup interaction, which is
        // the only chance to apply it: updateSession fires only on user edits.
        assert_eq!(save_one(&app, "s2", "Second").await, StatusCode::CREATED);
        assert_eq!(collections_of(&state, "Second"), ["Inbox"]);

        // A snapshot save takes the remembered target too.
        let page = json!({
            "sessionID": "s3",
            "url": "https://example.com/page",
            "title": "Page"
        })
        .to_string();
        let saved = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/saveSnapshot",
                "application/json",
                page,
            ))
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::CREATED);
        assert_eq!(collections_of(&state, "Page"), ["Inbox"]);
    }

    #[tokio::test]
    async fn an_unknown_target_is_rejected() {
        let (_directory, state) = filed_state();
        let app = connector_router(state.clone());
        assert_eq!(save_one(&app, "s1", "Filed").await, StatusCode::CREATED);

        assert_eq!(
            retarget(&app, "s1", "C1", json!([])).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            retarget(&app, "s1", "not-a-target", json!([])).await,
            StatusCode::BAD_REQUEST
        );
        assert!(collections_of(&state, "Filed").is_empty());
    }

    #[tokio::test]
    async fn webpage_and_standalone_attachment_workflows_use_distinct_sessions() {
        let (_directory, state) = test_state();
        let app = connector_router(state.clone());
        let save_page = json!({
            "sessionID": "page-session",
            "url": "https://example.com/page",
            "title": "Example page"
        })
        .to_string();
        let saved = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/saveSnapshot",
                "application/json",
                save_page.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::CREATED);

        let duplicate = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/saveSnapshot",
                "application/json",
                save_page,
            ))
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let body = to_bytes(duplicate.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "SESSION_EXISTS");

        let single_file = json!({
            "sessionID": "page-session",
            "url": "https://example.com/page",
            "title": "Example page",
            "snapshotContent": "<!doctype html><title>Example page</title>"
        })
        .to_string();
        let attached = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/saveSingleFile",
                "application/json",
                single_file,
            ))
            .await
            .unwrap();
        assert_eq!(attached.status(), StatusCode::CREATED);

        let metadata = json!({
            "url": "https://example.com/direct.epub",
            "contentType": "application/epub+zip",
            "title": "Direct EPUB"
        })
        .to_string();
        let mut standalone = request(
            "POST",
            "/connector/saveStandaloneAttachment?sessionID=file-session",
            "application/epub+zip",
            Body::from("EPUB bytes"),
        );
        standalone
            .headers_mut()
            .insert("X-Metadata", metadata.parse().unwrap());
        let standalone = app.clone().oneshot(standalone).await.unwrap();
        assert_eq!(standalone.status(), StatusCode::CREATED);
        let body = to_bytes(standalone.into_body(), usize::MAX).await.unwrap();
        let body: JsonValue = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["canRecognize"], false);

        let nonempty_note = json!({
            "sessionID": "file-session",
            "target": "L1",
            "tags": "reading, books",
            "note": "unsupported"
        })
        .to_string();
        let rejected = app
            .clone()
            .oneshot(request(
                "POST",
                "/connector/updateSession",
                "application/json",
                nonempty_note,
            ))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        let source = state.inner.layout.read_utf8().unwrap();
        let catalog = Catalog::parse(&state.inner.layout.bibliography, &source).unwrap();
        let items = catalog.items().collect::<Vec<_>>();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.attachments.len() == 1));
        assert!(items.iter().any(|item| item.entry_type == "online"));
        assert!(items.iter().any(|item| {
            item.entry_type == "misc"
                && item
                    .fields
                    .iter()
                    .any(|field| field.name == "zotero-item-type" && field.value == "attachment")
        }));

        for endpoint in [
            "/connector/getClientHostnames",
            "/connector/proxies",
            "/connector/hasAttachmentResolvers",
        ] {
            let response = app
                .clone()
                .oneshot(request("POST", endpoint, "application/json", "{}"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }
}
