// tests/integration.rs — end-to-end tests against a real OpenSearch.
// Requires OpenSearch reachable at $OPENSEARCH_URL (default
// http://localhost:9200) — e.g. `docker compose up -d opensearch`.
// Tests silently pass (skip) when the cluster is unreachable so plain
// `cargo test` still works without Docker.

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use stac_multitenant_server::{build_app, handlers::AppState, links::LinkEngine, store::Store};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn uniq(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

/// None if OpenSearch is unreachable — caller should skip the test.
async fn test_state(enable_transactions: bool) -> Option<Arc<AppState>> {
    let url = std::env::var("OPENSEARCH_URL")
        .unwrap_or_else(|_| "http://localhost:9200".to_string());
    let store = Store::connect(&url).ok()?;
    store.ensure_indices().await.ok()?;
    Some(Arc::new(AppState {
        base_url: "http://test".to_string(),
        store,
        links: LinkEngine::new("http://test"),
        enable_transactions,
    }))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn catalog(id: &str) -> Value {
    json!({"type": "Catalog", "id": id, "description": "test"})
}

fn collection(id: &str) -> Value {
    json!({
        "type": "Collection",
        "id": id,
        "description": "test",
        "license": "MIT",
        "extent": {
            "spatial": {"bbox": [[-180.0, -90.0, 180.0, 90.0]]},
            "temporal": {"interval": [["2020-01-01T00:00:00Z", null]]}
        }
    })
}

fn item(id: &str) -> Value {
    json!({
        "type": "Feature",
        "id": id,
        "geometry": {"type": "Point", "coordinates": [0.0, 0.0]},
        "properties": {"datetime": "2024-01-01T00:00:00Z"}
    })
}

#[tokio::test]
async fn test_catalog_lifecycle() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat = uniq("it-cat");

    let (s, _) = call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, _) = call(&app, "GET", &format!("/catalogs/{cat}"), None).await;
    assert_eq!(s, StatusCode::OK);

    let (s, _) = call(&app, "DELETE", &format!("/catalogs/{cat}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Disband must remove the document too — not just the hierarchy node
    let (s, _) = call(&app, "GET", &format!("/catalogs/{cat}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_poly_hierarchy_link_and_unlink() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat_a = uniq("it-a");
    let cat_b = uniq("it-b");
    let col = uniq("it-col");

    for cat in [&cat_a, &cat_b] {
        let (s, _) = call(&app, "POST", "/catalogs", Some(catalog(cat))).await;
        assert_eq!(s, StatusCode::CREATED);
    }

    // Mode A: create collection under A
    let (s, _) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat_a}/collections"),
        Some(collection(&col)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    // Mode B: link the same collection under B — shared collection, two parents
    let (s, _) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat_b}/collections"),
        Some(json!({"id": col})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Reachable from both scopes
    for cat in [&cat_a, &cat_b] {
        let (s, _) = call(
            &app,
            "GET",
            &format!("/catalogs/{cat}/collections/{col}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    // Unlink from B — must stay reachable via A
    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/catalogs/{cat_b}/collections/{col}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (s, _) = call(
        &app,
        "GET",
        &format!("/catalogs/{cat_b}/collections/{col}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &app,
        "GET",
        &format!("/catalogs/{cat_a}/collections/{col}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn test_disband_orphan_adoption() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat = uniq("it-orphan");
    let col = uniq("it-orphan-col");

    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    call(&app, "DELETE", &format!("/catalogs/{cat}"), None).await;

    // Orphaned collection adopted by root
    let (s, body) = call(&app, "GET", "/catalogs/root/children?type=Collection", None).await;
    assert_eq!(s, StatusCode::OK);
    let children: Vec<&str> = body["children"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(children.contains(&col.as_str()));
}

#[tokio::test]
async fn test_scoped_search_intersection() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat = uniq("it-scope");
    let col = uniq("it-scope-col");
    let it = uniq("it-scope-item");

    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    let (s, _) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections/{col}/items"),
        Some(item(&it)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    // In-scope search finds the item
    let (s, body) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/search"),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], it);

    // Requesting a foreign collection id intersects to empty
    let (s, body) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/search"),
        Some(json!({"collections": ["not-in-scope"]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberReturned"], 0);

    // Registry-wide search also finds it (everything is under root)
    let (s, body) = call(&app, "POST", "/catalogs/search", Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = body["features"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(ids.contains(&it.as_str()));
}

#[tokio::test]
async fn test_search_bbox_datetime_intersects_ids() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat = uniq("it-geo");
    let col = uniq("it-geo-col");
    let inside = uniq("it-geo-in");
    let outside = uniq("it-geo-out");

    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;

    // inside: SF-area point, June; outside: Boston point, August
    let mut in_item = item(&inside);
    in_item["geometry"] = json!({"type": "Point", "coordinates": [-122.0, 37.5]});
    in_item["properties"]["datetime"] = json!("2023-06-15T00:00:00Z");
    let mut out_item = item(&outside);
    out_item["geometry"] = json!({"type": "Point", "coordinates": [-70.0, 42.0]});
    out_item["properties"]["datetime"] = json!("2023-08-01T00:00:00Z");

    for it in [in_item, out_item] {
        let (s, _) = call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections/{col}/items"),
            Some(it),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    let search = |body: Value| {
        let app = app.clone();
        let cat = cat.clone();
        async move {
            call(&app, "POST", &format!("/catalogs/{cat}/search"), Some(body)).await
        }
    };

    // bbox: only the inside item intersects the SF box
    let (s, body) = search(json!({"bbox": [-123.0, 37.0, -121.0, 38.0]})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], inside);

    // bbox covering both
    let (_, body) = search(json!({"bbox": [-180.0, -90.0, 180.0, 90.0]})).await;
    assert_eq!(body["numberMatched"], 2);

    // datetime interval and instant (full RFC3339 — the crate rejects
    // date-only bounds in valid())
    let (_, body) =
        search(json!({"datetime": "2023-06-01T00:00:00Z/2023-07-01T00:00:00Z"})).await;
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], inside);
    let (_, body) = search(json!({"datetime": "2023-08-01T00:00:00Z"})).await;
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], outside);

    // intersects geometry
    let (_, body) =
        search(json!({"intersects": {"type": "Point", "coordinates": [-122.0, 37.5]}})).await;
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], inside);

    // ids filter
    let (_, body) = search(json!({"ids": [outside]})).await;
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], outside);

    // bbox + intersects together is a spec violation -> 400
    let (s, _) = search(json!({
        "bbox": [-180.0, -90.0, 180.0, 90.0],
        "intersects": {"type": "Point", "coordinates": [0.0, 0.0]}
    }))
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_search_pagination() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat = uniq("it-page");
    let col = uniq("it-page-col");

    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    for i in 0..3 {
        let (s, _) = call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections/{col}/items"),
            Some(item(&uniq(&format!("it-page-item-{i}")))),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    let search = |body: Value| {
        let app = app.clone();
        let cat = cat.clone();
        async move {
            call(&app, "POST", &format!("/catalogs/{cat}/search"), Some(body)).await
        }
    };

    // Page 1: 2 of 3, next link carries offset=2 in its POST body
    let (s, body) = search(json!({"limit": 2})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 3);
    assert_eq!(body["numberReturned"], 2);
    let next = body["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "next")
        .expect("missing next link");
    assert_eq!(next["method"], "POST");
    assert_eq!(next["body"]["offset"], 2);
    assert_eq!(next["body"]["limit"], 2); // original search preserved

    // Page 2 via the next link's body: 1 item, prev link back to offset 0
    let (s, body) = search(next["body"].clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 3);
    assert_eq!(body["numberReturned"], 1);
    let links = body["links"].as_array().unwrap();
    assert!(links.iter().any(|l| l["rel"] == "prev"));
    assert!(!links.iter().any(|l| l["rel"] == "next"));
}

#[tokio::test]
async fn test_item_collection_mismatch_is_400() {
    let Some(state) = test_state(true).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);
    let cat = uniq("it-val");
    let col = uniq("it-val-col");

    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;

    let mut bad_item = item("it-val-item");
    bad_item["collection"] = json!("some-other-collection");
    let (s, _) = call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections/{col}/items"),
        Some(bad_item),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_transactions_disabled_405s() {
    // Router-level gating — store only needed to construct state
    let Some(state) = test_state(false).await else {
        eprintln!("skipping: OpenSearch unreachable");
        return;
    };
    let app = build_app(state);

    let (s, _) = call(&app, "POST", "/catalogs", Some(catalog("it-x"))).await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);

    // Reads still work
    let (s, _) = call(&app, "GET", "/", None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, "GET", "/catalogs", None).await;
    assert_eq!(s, StatusCode::OK);
}
