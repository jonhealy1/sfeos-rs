// src/handlers.rs
use crate::dto::CreateOrLinkPayload;
use crate::links::LinkEngine;
use crate::store::{
    search_offset, NodeKind, Store, CATALOGS_INDEX, COLLECTIONS_INDEX, DEFAULT_SEARCH_LIMIT,
    ITEMS_INDEX, MAX_SEARCH_LIMIT, ROOT_CATALOG_ID,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use stac::{Catalog, Collection, Item, Link};
use stac_api::{GetSearch, ItemCollection, Search};
use std::collections::HashMap;
use std::sync::Arc;

pub struct AppState {
    pub base_url: String,
    pub store: Store,
    pub links: LinkEngine,
    pub enable_transactions: bool,
}

#[derive(Deserialize)]
pub struct ChildrenQuery {
    pub r#type: Option<String>, // Filter by "Catalog" or "Collection"
}

fn parse_kind(raw: Option<&str>) -> Option<NodeKind> {
    match raw {
        Some("Catalog") => Some(NodeKind::Catalog),
        Some("Collection") => Some(NodeKind::Collection),
        _ => None,
    }
}

/// 404 unless `collection_id` lives somewhere inside `catalog_id`'s DAG.
async fn require_scoped_collection(
    state: &AppState,
    catalog_id: &str,
    collection_id: &str,
) -> Result<(), ApiError> {
    let allowed = state.store.get_descendant_collections(catalog_id).await?;
    if !allowed.contains(collection_id) {
        return Err(ApiError::NotFound(collection_id.to_string()));
    }
    Ok(())
}

/// STAC links for a catalog: `parent` points at a real parent when one
/// exists (never the implicit root), extra parents become `related`.
/// Links are derived from the DAG at read time — never stored.
fn catalog_links(state: &AppState, catalog_id: &str, parents: &[String]) -> Vec<Link> {
    let primary = parents
        .iter()
        .find(|p| p.as_str() != ROOT_CATALOG_ID)
        .or(parents.first());
    let alts: Vec<String> = parents
        .iter()
        .filter(|p| Some(*p) != primary && p.as_str() != ROOT_CATALOG_ID)
        .cloned()
        .collect();
    state
        .links
        .format_sub_catalog_links(catalog_id, primary.map(String::as_str), &alts)
}

fn collection_links(state: &AppState, collection_id: &str, scoped_catalog_id: &str, parents: &[String]) -> Vec<Link> {
    state.links.format_scoped_collection_links(
        collection_id,
        scoped_catalog_id,
        parents,
    )
}

// --- Discovery Handlers ---

pub async fn root_landing_page(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut conforms_to = vec![
        "https://api.stacspec.org/v1.0.0/core",
        "https://api.stacspec.org/v1.0.0/multi-tenant-catalogs",
        "https://api.stacspec.org/v1.0.0/multi-tenant-catalogs/search",
        "https://api.stacspec.org/v1.0.0/children",
        "https://api.stacspec.org/v1.0.0/children#type-filter",
    ];
    if state.enable_transactions {
        conforms_to.push("https://api.stacspec.org/v1.0.0/multi-tenant-catalogs/transaction");
        conforms_to.push("https://api.stacspec.org/v1.0.0/ogcapi-features/extensions/transaction");
    }
    Json(json!({
        "stac_version": "1.0.0",
        "type": "Catalog",
        "id": "stac-multi-tenant-root",
        "title": "STAC API with Multi-Tenant Catalogs",
        "conformsTo": conforms_to,
        "links": [
            { "rel": "self", "type": "application/json", "href": "http://localhost:3000/" },
            { "rel": "data", "type": "application/json", "href": "http://localhost:3000/collections" },
            { "rel": "catalogs", "type": "application/json", "href": "http://localhost:3000/catalogs", "title": "Multi-Tenant Catalogs Registry" }
        ]
    }))
}


pub async fn list_catalogs(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let ids = state
        .store
        .get_children_by_kind(ROOT_CATALOG_ID, NodeKind::Catalog)
        .await?;
    let parents_map = state.store.get_parents_many(&ids).await?;
    let mut catalogs = state.store.get_documents(CATALOGS_INDEX, &ids).await?;
    for doc in catalogs.iter_mut() {
        if let Some(id) = doc["id"].as_str() {
            let parents = parents_map.get(id).cloned().unwrap_or_default();
            doc["links"] = json!(catalog_links(&state, id, &parents));
        }
    }
    Ok(Json(json!({
        "catalogs": catalogs,
        "links": [
            { "rel": "self", "href": format!("{}/catalogs", state.base_url) },
            { "rel": "root", "href": state.base_url }
        ]
    })))
}

pub async fn create_root_catalog(
    State(state): State<Arc<AppState>>,
    Json(catalog): Json<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .store
        .set_parents(&catalog.id, vec![ROOT_CATALOG_ID.to_string()], NodeKind::Catalog)
        .await?;
    state
        .store
        .index_document(CATALOGS_INDEX, &catalog.id, &catalog)
        .await?;
    Ok((StatusCode::CREATED, Json(json!(catalog))))
}

pub async fn get_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    match state.store.get_document(CATALOGS_INDEX, &catalog_id).await? {
        Some(mut doc) => {
            let parents = state.store.get_parents(&catalog_id).await?;
            doc["links"] = json!(catalog_links(&state, &catalog_id, &parents));
            Ok(Json(doc))
        }
        None => Err(ApiError::NotFound(catalog_id)),
    }
}

pub async fn update_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(catalog): Json<Catalog>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .store
        .index_document(CATALOGS_INDEX, &catalog_id, &catalog)
        .await?;
    Ok(Json(json!(catalog)))
}

pub async fn get_catalog_children(
    Path(catalog_id): Path<String>,
    Query(query): Query<ChildrenQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let nodes = state
        .store
        .get_child_nodes(&catalog_id, parse_kind(query.r#type.as_deref()))
        .await?;

    // Fetch the child documents (split by kind — catalogs and collections
    // live in different indices) then re-assemble in node order.
    let cat_ids: Vec<String> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Catalog)
        .map(|n| n.id.clone())
        .collect();
    let col_ids: Vec<String> = nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Collection)
        .map(|n| n.id.clone())
        .collect();
    let mut docs = state.store.get_documents(CATALOGS_INDEX, &cat_ids).await?;
    docs.extend(state.store.get_documents(COLLECTIONS_INDEX, &col_ids).await?);
    let mut docs_by_id: HashMap<String, serde_json::Value> = docs
        .into_iter()
        .filter_map(|d| {
            let id = d["id"].as_str()?.to_string();
            Some((id, d))
        })
        .collect();

    let mut children = Vec::with_capacity(nodes.len());
    for node in &nodes {
        let Some(mut doc) = docs_by_id.remove(&node.id) else {
            continue; // hierarchy node exists but doc is missing — skip
        };
        let alts: Vec<String> = node
            .parents
            .iter()
            .filter(|p| **p != catalog_id && p.as_str() != ROOT_CATALOG_ID)
            .cloned()
            .collect();
        doc["links"] = match node.kind {
            // parent link locked to this listing's catalog context
            NodeKind::Catalog => json!(state.links.format_sub_catalog_links(
                &node.id,
                Some(&catalog_id),
                &alts
            )),
            NodeKind::Collection => json!(state.links.format_scoped_collection_links(
                &node.id,
                &catalog_id,
                &node.parents
            )),
        };
        children.push(doc);
    }

    Ok(Json(json!({
        "children": children,
        "links": [
            { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/children", state.base_url) },
            { "rel": "root", "href": state.base_url },
            { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
        ]
    })))
}

// --- Scoped Item Search ---

/// Shared scoped-search pipeline: intersect the request's collections with
/// the scope's descendants, then run the OpenSearch query.
async fn run_scoped_search(
    state: &AppState,
    scope_id: &str,
    mut search: Search,
) -> Result<Json<ItemCollection>, ApiError> {
    // Spec-level validation: bbox/intersects mutual exclusion, bbox
    // ordering, and datetime parsing (full RFC3339 — date-only bounds
    // are rejected by the crate).
    search = search
        .valid()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let empty = || {
        ItemCollection::new(Vec::new()).map_err(|e| ApiError::Internal(e.to_string()))
    };

    // 1. Resolve all descendant collection IDs in the scope's DAG
    let allowed_collections = state.store.get_descendant_collections(scope_id).await?;
    if allowed_collections.is_empty() {
        return Ok(Json(empty()?));
    }

    // 2. Security & Intersection: Enforce scope boundaries on search payload
    if search.collections.is_empty() {
        // No filter provided — restrict to all descendants of this scope
        search.collections = allowed_collections.into_iter().collect();
    } else {
        // Intersect requested collections with allowed descendants
        search.collections.retain(|c| allowed_collections.contains(c));
        if search.collections.is_empty() {
            return Ok(Json(empty()?));
        }
    }

    // 3. Query OpenSearch — collections scope + bbox/datetime/intersects/ids
    let (items, matched) = state.store.search_items(&search.collections, &search).await?;
    let returned = items.len() as u64;
    let mut collection =
        ItemCollection::new(items).map_err(|e| ApiError::Internal(e.to_string()))?;
    collection.number_matched = Some(matched);
    collection.number_returned = Some(returned);

    // 4. Offset pagination links (STAC pagination: method+body on the link).
    let limit = search
        .items
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT);
    let offset = search_offset(&search);
    let href = if scope_id == ROOT_CATALOG_ID {
        format!("{}/catalogs/search", state.base_url)
    } else {
        format!("{}/catalogs/{scope_id}/search", state.base_url)
    };
    if offset + returned < matched {
        collection
            .links
            .push(page_link("next", &href, &search, offset + limit));
    }
    if offset > 0 {
        collection.links.push(page_link(
            "prev",
            &href,
            &search,
            offset.saturating_sub(limit),
        ));
    }
    Ok(Json(collection))
}

/// A STAC pagination link: rel + href + `method: POST` + the original
/// search body with an updated `offset`.
fn page_link(rel: &str, href: &str, search: &Search, offset: u64) -> Link {
    let mut body = serde_json::to_value(search).unwrap_or_default();
    body["offset"] = json!(offset);
    let mut link = Link::new(href, rel);
    link.method = Some("POST".to_string());
    link.r#type = Some("application/geo+json".to_string());
    link.body = body.as_object().cloned();
    link
}

pub async fn scoped_search_post(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(search): Json<Search>,
) -> Result<Json<ItemCollection>, ApiError> {
    run_scoped_search(&state, &catalog_id, search).await
}

pub async fn scoped_search_get(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Query(params): Query<GetSearch>,
) -> Result<Json<ItemCollection>, ApiError> {
    let search = Search::try_from(params).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    run_scoped_search(&state, &catalog_id, search).await
}

/// Search across the whole catalogs registry — equivalent to scoping to
/// `root`, since every orphan is adopted there.
pub async fn catalogs_search_post(
    State(state): State<Arc<AppState>>,
    Json(search): Json<Search>,
) -> Result<Json<ItemCollection>, ApiError> {
    run_scoped_search(&state, ROOT_CATALOG_ID, search).await
}

pub async fn catalogs_search_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<GetSearch>,
) -> Result<Json<ItemCollection>, ApiError> {
    let search = Search::try_from(params).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    run_scoped_search(&state, ROOT_CATALOG_ID, search).await
}

// --- Transaction Handlers ---

pub async fn link_or_create_sub_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateOrLinkPayload<Catalog>>,
) -> Result<impl IntoResponse, ApiError> {
    match payload {
        CreateOrLinkPayload::LinkReference { id } => {
            // Mode B: Link existing sub-catalog
            state.store.link(&id, &catalog_id, NodeKind::Catalog).await?;
            Ok((
                StatusCode::OK,
                Json(json!({"message": "Catalog linked successfully"})),
            ))
        }
        CreateOrLinkPayload::FullResource(new_catalog) => {
            // Mode A: Create new sub-catalog
            state
                .store
                .set_parents(&new_catalog.id, vec![catalog_id], NodeKind::Catalog)
                .await?;
            state
                .store
                .index_document(CATALOGS_INDEX, &new_catalog.id, &new_catalog)
                .await?;
            Ok((StatusCode::CREATED, Json(json!(new_catalog))))
        }
    }
}

pub async fn unlink_sub_catalog(
    Path((catalog_id, sub_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    // Unlink only; orphans are adopted by root rather than deleted
    state.store.unlink_and_adopt(&sub_id, &catalog_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_scoped_collections(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let ids = state
        .store
        .get_children_by_kind(&catalog_id, NodeKind::Collection)
        .await?;
    let parents_map = state.store.get_parents_many(&ids).await?;
    let mut collections = state.store.get_documents(COLLECTIONS_INDEX, &ids).await?;
    for doc in collections.iter_mut() {
        if let Some(id) = doc["id"].as_str() {
            let parents = parents_map.get(id).cloned().unwrap_or_default();
            doc["links"] = json!(collection_links(&state, id, &catalog_id, &parents));
        }
    }
    Ok(Json(json!({
        "collections": collections,
        "links": [
            { "rel": "self", "href": format!("{}/catalogs/{catalog_id}/collections", state.base_url) },
            { "rel": "root", "href": state.base_url },
            { "rel": "parent", "href": format!("{}/catalogs/{catalog_id}", state.base_url) }
        ]
    })))
}

pub async fn link_or_create_scoped_collection(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateOrLinkPayload<Collection>>,
) -> Result<impl IntoResponse, ApiError> {
    match payload {
        CreateOrLinkPayload::LinkReference { id } => {
            state
                .store
                .link(&id, &catalog_id, NodeKind::Collection)
                .await?;
            Ok((
                StatusCode::OK,
                Json(json!({"message": "Collection linked successfully"})),
            ))
        }
        CreateOrLinkPayload::FullResource(collection) => {
            state
                .store
                .set_parents(&collection.id, vec![catalog_id], NodeKind::Collection)
                .await?;
            state
                .store
                .index_document(COLLECTIONS_INDEX, &collection.id, &collection)
                .await?;
            Ok((StatusCode::CREATED, Json(json!(collection))))
        }
    }
}

pub async fn get_scoped_collection(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state
        .store
        .get_document(COLLECTIONS_INDEX, &collection_id)
        .await?
    {
        Some(mut doc) => {
            let parents = state.store.get_parents(&collection_id).await?;
            doc["links"] =
                json!(collection_links(&state, &collection_id, &catalog_id, &parents));
            Ok(Json(doc))
        }
        None => Err(ApiError::NotFound(collection_id)),
    }
}

pub async fn update_scoped_collection(
    Path((_catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    Json(collection): Json<Collection>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .store
        .index_document(COLLECTIONS_INDEX, &collection_id, &collection)
        .await?;
    Ok(Json(json!(collection)))
}

pub async fn unlink_scoped_collection(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    state
        .store
        .unlink_and_adopt(&collection_id, &catalog_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disband_catalog(
    Path(catalog_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    // Safety Disband: Unlink direct children only and auto-adopt orphans to Root.
    let direct_children = state.store.get_children(&catalog_id).await?;
    for child in direct_children {
        state.store.unlink_and_adopt(&child, &catalog_id).await?;
    }

    // Deleting the node's doc detaches it from all parents (children are
    // derived from `parents` lookups — no reverse cleanup needed)
    state.store.remove_node(&catalog_id).await?;

    // Delete the catalog document itself (never child collections or items)
    state.store.delete_document(CATALOGS_INDEX, &catalog_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Scoped Items (reads: always on; writes: ENABLE_TRANSACTIONS_EXTENSIONS) ---

/// Enforce that the item's declared `collection` matches the path.
fn validate_item_collection(
    item: &mut Item,
    collection_id: &str,
) -> Result<(), ApiError> {
    match &item.collection {
        Some(c) if c != collection_id => Err(ApiError::BadRequest(format!(
            "item collection '{c}' does not match path collection '{collection_id}'"
        ))),
        Some(_) => Ok(()),
        None => {
            item.collection = Some(collection_id.to_string());
            Ok(())
        }
    }
}

pub async fn list_scoped_items(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ItemCollection>, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    let (items, matched) = state
        .store
        .search_items(&[collection_id], &Search::default())
        .await?;
    let returned = items.len() as u64;
    let mut collection =
        ItemCollection::new(items).map_err(|e| ApiError::Internal(e.to_string()))?;
    collection.number_matched = Some(matched);
    collection.number_returned = Some(returned);
    Ok(Json(collection))
}

pub async fn get_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(doc) if doc["collection"] == collection_id => Ok(Json(doc)),
        _ => Err(ApiError::NotFound(item_id)),
    }
}

pub async fn create_scoped_item(
    Path((catalog_id, collection_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
    Json(mut item): Json<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id)?;
    state
        .store
        .index_document(ITEMS_INDEX, &item.id, &item)
        .await?;
    Ok((StatusCode::CREATED, Json(json!(item))))
}

pub async fn update_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
    Json(mut item): Json<Item>,
) -> Result<impl IntoResponse, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    validate_item_collection(&mut item, &collection_id)?;
    state
        .store
        .index_document(ITEMS_INDEX, &item_id, &item)
        .await?;
    Ok(Json(json!(item)))
}

pub async fn delete_scoped_item(
    Path((catalog_id, collection_id, item_id)): Path<(String, String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ApiError> {
    require_scoped_collection(&state, &catalog_id, &collection_id).await?;
    match state.store.get_document(ITEMS_INDEX, &item_id).await? {
        Some(doc) if doc["collection"] == collection_id => {
            state.store.delete_document(ITEMS_INDEX, &item_id).await?;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(ApiError::NotFound(item_id)),
    }
}

// --- Error Handling ---

pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl From<opensearch::Error> for ApiError {
    fn from(e: opensearch::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::NotFound(id) => (StatusCode::NOT_FOUND, format!("Resource '{id}' not found")),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({ "code": status.as_u16(), "description": msg }))).into_response()
    }
}
