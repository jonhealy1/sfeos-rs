// src/lib.rs
pub mod dto;
pub mod handlers;
pub mod links;
pub mod store;

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use handlers::*;
use std::sync::Arc;

/// Build the API router. Mutating routes are only mounted when
/// `state.enable_transactions` is set (ENABLE_TRANSACTIONS_EXTENSIONS).
pub fn build_app(state: Arc<AppState>) -> Router {
    // Read-only surface — always registered
    let mut app = Router::new()
        // --- Global Landing Page ---
        .route("/", get(root_landing_page))
        .route("/conformance", get(conformance))
        .route("/api", get(api))
        .route("/sortables", get(sortables))
        // --- Core STAC item search (registry-wide; same as /catalogs/search) ---
        .route(
            "/search",
            get(catalogs_search_get).post(catalogs_search_post),
        )
        // --- Core STAC collections routes (global namespace) ---
        .route("/collections", get(list_collections))
        .route("/collections/{collection_id}", get(get_collection))
        .route(
            "/collections/{collection_id}/sortables",
            get(collection_sortables),
        )
        .route(
            "/collections/{collection_id}/items",
            get(list_collection_items),
        )
        .route(
            "/collections/{collection_id}/items/{item_id}",
            get(get_collection_item),
        )
        // --- Registry & Management Plane ---
        .route("/catalogs", get(list_catalogs))
        .route("/catalogs/{catalog_id}", get(get_catalog))
        .route("/catalogs/{catalog_id}/catalogs", get(list_sub_catalogs))
        .route(
            "/catalogs/{catalog_id}/conformance",
            get(catalog_conformance),
        )
        // --- Children & Sub-Resources ---
        .route("/catalogs/{catalog_id}/children", get(get_catalog_children))
        // --- Scoped Collections & Items ---
        .route(
            "/catalogs/{catalog_id}/collections",
            get(list_scoped_collections),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}",
            get(get_scoped_collection),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}/items",
            get(list_scoped_items),
        )
        .route(
            "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
            get(get_scoped_item),
        )
        // --- Scoped Search Engine ---
        // /catalogs/search = whole-registry scope (everything under root)
        .route(
            "/catalogs/search",
            get(catalogs_search_get).post(catalogs_search_post),
        )
        .route(
            "/catalogs/{catalog_id}/search",
            get(scoped_search_get).post(scoped_search_post),
        );

    // Transaction surface — hybrid extension beyond the multi-tenant-catalogs
    // spec; only mounted when ENABLE_TRANSACTIONS_EXTENSIONS is set.
    if state.enable_transactions {
        app = app.merge(
            Router::new()
                // Core transaction routes (STAC Transaction extension /
                // OGC API Features Part-4 style) on the canonical surface
                .route("/collections", post(create_root_collection))
                .route(
                    "/collections/{collection_id}",
                    put(update_root_collection).delete(delete_root_collection),
                )
                .route("/collections/{collection_id}/items", post(create_root_item))
                .route(
                    "/collections/{collection_id}/items/{item_id}",
                    put(update_root_item).delete(delete_root_item),
                )
                // Catalog-extension transaction routes (scoped surface)
                .route("/catalogs", post(create_root_catalog))
                .route(
                    "/catalogs/{catalog_id}",
                    put(update_catalog).delete(disband_catalog),
                )
                .route(
                    "/catalogs/{catalog_id}/catalogs",
                    post(link_or_create_sub_catalog),
                )
                .route(
                    "/catalogs/{catalog_id}/catalogs/{sub_id}",
                    delete(unlink_sub_catalog),
                )
                .route(
                    "/catalogs/{catalog_id}/collections",
                    post(link_or_create_scoped_collection),
                )
                .route(
                    "/catalogs/{catalog_id}/collections/{collection_id}",
                    put(update_scoped_collection).delete(unlink_scoped_collection),
                )
                .route(
                    "/catalogs/{catalog_id}/collections/{collection_id}/items",
                    post(create_scoped_item),
                )
                .route(
                    "/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}",
                    put(update_scoped_item).delete(delete_scoped_item),
                ),
        );
    }

    app.with_state(state)
}
