// tests/collections.rs — root /collections* read routes (core STAC).
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn test_list_collections() {
    let Some((app, _)) = test_app(true).await else { return };
    let cat = uniq("cl-cat");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections"),
            Some(collection(&uniq(&format!("cl-col-{i}")))),
        )
        .await;
    }
    let (s, body) = call(&app, "GET", "/collections", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["numberMatched"], 3);
    assert_eq!(body["collections"].as_array().unwrap().len(), 3);
    for col in body["collections"].as_array().unwrap() {
        let rels = link_rels(col);
        for rel in ["self", "root", "parent"] {
            assert!(rels.contains(&rel.to_string()), "missing {rel}");
        }
        // canonical self link — the global route, not a scoped path
        let selfs = links_by_rel(col, "self");
        assert!(selfs[0]["href"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/collections/{}", col["id"].as_str().unwrap())));
    }
}

#[tokio::test]
async fn test_list_collections_pagination() {
    let Some((app, _)) = test_app(true).await else { return };
    let cat = uniq("clp-cat");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections"),
            Some(collection(&uniq(&format!("clp-col-{i}")))),
        )
        .await;
    }
    let (s, page1) = call(&app, "GET", "/collections?limit=2", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page1["collections"].as_array().unwrap().len(), 2);
    assert_eq!(page1["numberMatched"], 3);
    let next = links_by_rel(&page1, "next").pop().expect("missing next link");
    let token = next["href"]
        .as_str()
        .unwrap()
        .split("token=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let (s, page2) = call(
        &app,
        "GET",
        &format!("/collections?limit=2&token={token}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page2["collections"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_get_collection_global_links() {
    // Poly-hierarchy collection read via canonical route: parent link per
    // parent catalog, duplicate links to each scoped path.
    let Some((app, _)) = test_app(true).await else { return };
    let (p1, p2) = (uniq("gl-p1"), uniq("gl-p2"));
    let col = uniq("gl-col");
    for p in [&p1, &p2] {
        call(&app, "POST", "/catalogs", Some(catalog(p))).await;
    }
    call(
        &app,
        "POST",
        &format!("/catalogs/{p1}/collections"),
        Some(collection(&col)),
    )
    .await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{p2}/collections"),
        Some(json!({"id": col})),
    )
    .await;

    let (s, body) = call(&app, "GET", &format!("/collections/{col}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["id"], col);
    let selfs = links_by_rel(&body, "self");
    assert!(selfs[0]["href"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/collections/{col}")));
    let parents = links_by_rel(&body, "parent");
    let hrefs: Vec<&str> = parents.iter().filter_map(|l| l["href"].as_str()).collect();
    assert!(hrefs.iter().any(|h| h.ends_with(&format!("/catalogs/{p1}"))));
    assert!(hrefs.iter().any(|h| h.ends_with(&format!("/catalogs/{p2}"))));
    let dupes = links_by_rel(&body, "duplicate");
    assert_eq!(dupes.len(), 2);
}

#[tokio::test]
async fn test_get_collection_not_found() {
    let Some((app, _)) = test_app(true).await else { return };
    let (s, _) = call(&app, "GET", "/collections/nonexistent", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_collection_items_global_route() {
    let Some((app, _)) = test_app(true).await else { return };
    let cat = uniq("ci-cat");
    let col = uniq("ci-col");
    let it = uniq("ci-item");
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
        Some(item(&it, &col)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, body) = call(&app, "GET", &format!("/collections/{col}/items"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["type"], "FeatureCollection");
    assert_eq!(body["numberMatched"], 1);
    assert_eq!(body["features"][0]["id"], it);

    let (s, body) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items/{it}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["id"], it);
}

#[tokio::test]
async fn test_collection_items_404s() {
    let Some((app, _)) = test_app(true).await else { return };
    let (s, _) = call(&app, "GET", "/collections/nope/items", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "GET", "/collections/nope/items/x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_collection_items_pagination() {
    let Some((app, _)) = test_app(true).await else { return };
    let cat = uniq("ip-cat");
    let col = uniq("ip-col");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections/{col}/items"),
            Some(item(&uniq(&format!("ip-item-{i}")), &col)),
        )
        .await;
    }
    let (s, page1) = call(&app, "GET", &format!("/collections/{col}/items?limit=2"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page1["numberReturned"], 2);
    assert_eq!(page1["numberMatched"], 3);
    let next = links_by_rel(&page1, "next").pop().expect("missing next link");
    let href = next["href"].as_str().unwrap();
    assert!(href.contains("token="));
    let token = href.split("token=").nth(1).unwrap().split('&').next().unwrap();
    let (s, page2) = call(
        &app,
        "GET",
        &format!("/collections/{col}/items?limit=2&token={token}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page2["numberReturned"], 1);
}

#[tokio::test]
async fn test_scoped_items_pagination() {
    let Some((app, _)) = test_app(true).await else { return };
    let cat = uniq("sip-cat");
    let col = uniq("sip-col");
    call(&app, "POST", "/catalogs", Some(catalog(&cat))).await;
    call(
        &app,
        "POST",
        &format!("/catalogs/{cat}/collections"),
        Some(collection(&col)),
    )
    .await;
    for i in 0..3 {
        call(
            &app,
            "POST",
            &format!("/catalogs/{cat}/collections/{col}/items"),
            Some(item(&uniq(&format!("sip-item-{i}")), &col)),
        )
        .await;
    }
    let (s, page1) = call(
        &app,
        "GET",
        &format!("/catalogs/{cat}/collections/{col}/items?limit=2"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page1["numberReturned"], 2);
    assert_eq!(page1["numberMatched"], 3);
}

#[tokio::test]
async fn test_sortables_endpoint() {
    let Some((app, _)) = test_app(true).await else { return };
    let (s, body) = call(&app, "GET", "/sortables", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["type"], "object");
    assert_eq!(body["additionalProperties"], true);
    let props = body["properties"].as_object().unwrap();
    assert!(props.contains_key("id"));
    assert!(props.contains_key("collection"));
    assert!(props.contains_key("datetime"));
}

#[tokio::test]
async fn test_landing_advertises_sort_and_sortables() {
    let Some((app, _)) = test_app(true).await else { return };
    let (s, body) = call(&app, "GET", "/", None).await;
    assert_eq!(s, StatusCode::OK);
    let conforms: Vec<&str> = body["conformsTo"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u.as_str())
        .collect();
    for uri in [
        "https://api.stacspec.org/v1.0.0/item-search",
        "https://api.stacspec.org/v1.1.0/item-search#sort",
        "https://api.stacspec.org/v1.1.0/item-search#sortables",
    ] {
        assert!(conforms.contains(&uri), "missing {uri}");
    }
    let sortables = links_by_rel(&body, "http://www.opengis.net/def/rel/ogc/1.0/sortables");
    assert_eq!(sortables.len(), 1);
    assert!(sortables[0]["href"].as_str().unwrap().ends_with("/sortables"));
}
