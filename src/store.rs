// src/store.rs
use opensearch::{
    http::{
        transport::{SingleNodeConnectionPool, TransportBuilder},
        StatusCode, Url,
    },
    indices::{
        IndicesCreateParts, IndicesDeleteParts, IndicesExistsParts, IndicesPutMappingParts,
    },
    DeleteParts, GetParts, IndexParts, MgetParts, OpenSearch, SearchParts,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use stac::Bbox;
use stac_api::Search;
use std::collections::{HashMap, HashSet};

pub const ROOT_CATALOG_ID: &str = "root";

/// Logical index names — `Store` prepends its index prefix
/// (`{prefix}-{logical}`). The default `stac` prefix maps onto the
/// historical `stac-catalogs` / `stac-items` / ... index names.
pub const HIERARCHY_INDEX: &str = "hierarchy";
pub const CATALOGS_INDEX: &str = "catalogs";
pub const COLLECTIONS_INDEX: &str = "collections";
pub const ITEMS_INDEX: &str = "items";

/// Safety cap on DAG depth when resolving descendants.
const MAX_DESCENDANT_DEPTH: usize = 25;
/// Cap on children fetched per query (proper pagination is future work).
const MAX_CHILDREN: usize = 10_000;
/// Default / max page size for item search.
pub const DEFAULT_SEARCH_LIMIT: u64 = 100;
pub const MAX_SEARCH_LIMIT: u64 = 10_000;

/// STAC node type tracked in the hierarchy DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Catalog,
    Collection,
}

/// One document per node in `stac-hierarchy`, keyed by resource id.
/// Children are derived via `term` queries on `parents`, so link/unlink
/// only ever touches the child's document — no materialized DAG updates.
#[derive(Debug, Serialize, Deserialize)]
struct HierarchyNode {
    kind: NodeKind,
    #[serde(default)]
    parents: Vec<String>,
}

/// A hierarchy node as returned by children queries (id comes from `_id`).
#[derive(Debug)]
pub struct ChildNode {
    pub id: String,
    pub kind: NodeKind,
    pub parents: Vec<String>,
}

#[derive(Clone)]
pub struct Store {
    client: OpenSearch,
    prefix: String,
}

impl Store {
    pub fn connect(url: &str) -> Result<Self, opensearch::Error> {
        let url = Url::parse(url).expect("Invalid OPENSEARCH_URL");
        let pool = SingleNodeConnectionPool::new(url);
        let transport = TransportBuilder::new(pool).build()?;
        Ok(Self {
            client: OpenSearch::new(transport),
            prefix: "stac".to_string(),
        })
    }

    /// Use `{prefix}-{logical}` index names instead of `stac-*` — tests
    /// get fully isolated indices.
    pub fn with_index_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The physical index name for a logical index constant.
    fn idx(&self, logical: &str) -> String {
        format!("{}-{}", self.prefix, logical)
    }

    /// Delete every `{prefix}-*` index — test teardown helper.
    pub async fn drop_indices(&self) -> Result<(), opensearch::Error> {
        let pattern = format!("{}-*", self.prefix);
        self.client
            .indices()
            .delete(IndicesDeleteParts::Index(&[&pattern]))
            .send()
            .await?;
        Ok(())
    }

    /// Create the STAC indices on startup if they don't already exist.
    pub async fn ensure_indices(&self) -> Result<(), opensearch::Error> {
        let indices = [
            (
                HIERARCHY_INDEX,
                json!({"properties": {
                    "kind": {"type": "keyword"},
                    "parents": {"type": "keyword"}
                }}),
            ),
            (
                CATALOGS_INDEX,
                json!({"properties": {"id": {"type": "keyword"}}}),
            ),
            (
                COLLECTIONS_INDEX,
                json!({"properties": {"id": {"type": "keyword"}}}),
            ),
            (
                ITEMS_INDEX,
                json!({"properties": {
                    "id": {"type": "keyword"},
                    "collection": {"type": "keyword"},
                    "bbox": {"type": "float"},
                    "geometry": {"type": "geo_shape"},
                    "properties": {"properties": {
                        "datetime": {"type": "date"},
                        "start_datetime": {"type": "date"},
                        "end_datetime": {"type": "date"}
                    }}
                }}),
            ),
        ];

        for (logical, mappings) in indices {
            let index = self.idx(logical);
            let exists = self
                .client
                .indices()
                .exists(IndicesExistsParts::Index(&[&index]))
                .send()
                .await?;
            if !exists.status_code().is_success() {
                self.client
                    .indices()
                    .create(IndicesCreateParts::Index(&index))
                    // 0 replicas: dev default is single-node; without this
                    // every index sits yellow with unassigned shards.
                    .body(json!({
                        "settings": {"number_of_replicas": 0},
                        "mappings": mappings
                    }))
                    .send()
                    .await?;
            } else {
                // Index exists: apply additive mapping updates. Type
                // changes to already-mapped fields (e.g. a legacy index
                // where dynamic mapping made `geometry` an object) are
                // rejected by OpenSearch — warn so the dev can drop and
                // recreate the index.
                let resp = self
                    .client
                    .indices()
                    .put_mapping(IndicesPutMappingParts::Index(&[&index]))
                    .body(mappings)
                    .send()
                    .await?;
                if !resp.status_code().is_success() {
                    eprintln!(
                        "warning: could not apply mapping updates to {index} \
                         (field-type conflicts need a reindex — drop the index \
                         and restart to rebuild it)"
                    );
                }
            }
        }
        Ok(())
    }

    // --- Hierarchy DAG ---

    /// Replace a node's parent set (Mode A create / full update).
    /// Orphan safety: an empty parent set is adopted by root.
    pub async fn set_parents(
        &self,
        node_id: &str,
        parent_ids: Vec<String>,
        kind: NodeKind,
    ) -> Result<(), opensearch::Error> {
        let mut parents = parent_ids;
        if parents.is_empty() {
            parents.push(ROOT_CATALOG_ID.to_string());
        }
        self.put_node(node_id, &HierarchyNode { kind, parents })
            .await
    }

    /// Add a single parent to a node (Mode B linking).
    /// Keeps the node's existing kind if it was already registered.
    pub async fn link(
        &self,
        child_id: &str,
        parent_id: &str,
        kind: NodeKind,
    ) -> Result<(), opensearch::Error> {
        let mut node = self.get_node(child_id).await?.unwrap_or(HierarchyNode {
            kind,
            parents: vec![ROOT_CATALOG_ID.to_string()],
        });
        apply_link(&mut node.parents, parent_id);
        self.put_node(child_id, &node).await
    }

    /// Unlink a child from a parent; adopts the child under root if orphaned.
    pub async fn unlink_and_adopt(
        &self,
        child_id: &str,
        parent_id: &str,
    ) -> Result<(), opensearch::Error> {
        if let Some(mut node) = self.get_node(child_id).await? {
            apply_unlink(&mut node.parents, parent_id);
            self.put_node(child_id, &node).await?;
        }
        Ok(())
    }

    /// Delete a node outright (disband). Since children are derived from
    /// `parents` queries, deleting the doc detaches it from every parent.
    /// Callers must unlink the node's own children first.
    pub async fn remove_node(&self, node_id: &str) -> Result<(), opensearch::Error> {
        let index = self.idx(HIERARCHY_INDEX);
        self.client
            .delete(DeleteParts::IndexId(&index, node_id))
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(())
    }

    pub async fn get_parents(&self, node_id: &str) -> Result<Vec<String>, opensearch::Error> {
        Ok(self
            .get_node(node_id)
            .await?
            .map(|n| n.parents)
            .unwrap_or_default())
    }

    /// Direct child ids of a node (reverse `term` lookup on `parents`).
    pub async fn get_children(&self, node_id: &str) -> Result<Vec<String>, opensearch::Error> {
        Ok(self
            .search_children(node_id, None)
            .await?
            .into_iter()
            .map(|c| c.id)
            .collect())
    }

    /// Direct child ids of a node filtered by kind.
    pub async fn get_children_by_kind(
        &self,
        node_id: &str,
        kind: NodeKind,
    ) -> Result<Vec<String>, opensearch::Error> {
        Ok(self
            .search_children(node_id, Some(kind))
            .await?
            .into_iter()
            .map(|c| c.id)
            .collect())
    }

    /// Direct children with kind + parents — enough to render child links
    /// without a second round trip.
    pub async fn get_child_nodes(
        &self,
        node_id: &str,
        kind: Option<NodeKind>,
    ) -> Result<Vec<ChildNode>, opensearch::Error> {
        self.search_children(node_id, kind).await
    }

    async fn search_children(
        &self,
        node_id: &str,
        kind: Option<NodeKind>,
    ) -> Result<Vec<ChildNode>, opensearch::Error> {
        let mut must = vec![json!({"term": {"parents": node_id}})];
        if let Some(kind) = kind {
            must.push(json!({"term": {"kind": kind}}));
        }
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .search(SearchParts::Index(&[&index]))
            .body(json!({
                "size": MAX_CHILDREN,
                "query": {"bool": {"must": must}}
            }))
            .send()
            .await?;
        let body = resp.json::<Value>().await?;
        Ok(body["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| {
                let node: HierarchyNode =
                    serde_json::from_value(hit["_source"].clone()).ok()?;
                Some(ChildNode {
                    id: hit["_id"].as_str()?.to_string(),
                    kind: node.kind,
                    parents: node.parents,
                })
            })
            .collect())
    }

    /// Parents of many nodes in one mget (for link rendering on lists).
    pub async fn get_parents_many(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>, opensearch::Error> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .mget(MgetParts::Index(&index))
            .body(json!({"ids": ids}))
            .send()
            .await?;
        let body = resp.json::<Value>().await?;
        Ok(body["docs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["found"] == true)
            .filter_map(|d| {
                let id = d["_id"].as_str()?.to_string();
                let node: HierarchyNode = serde_json::from_value(d["_source"].clone()).ok()?;
                Some((id, node.parents))
            })
            .collect())
    }

    /// Resolve descendant COLLECTION ids for scoped search via level-wise
    /// BFS over `parents`. Depth-capped and cycle-safe.
    pub async fn get_descendant_collections(
        &self,
        root_id: &str,
    ) -> Result<HashSet<String>, opensearch::Error> {
        let mut visited: HashSet<String> = [root_id.to_string()].into_iter().collect();
        let mut frontier = vec![root_id.to_string()];
        let mut collections = HashSet::new();

        for _ in 0..MAX_DESCENDANT_DEPTH {
            if frontier.is_empty() {
                break;
            }
            let index = self.idx(HIERARCHY_INDEX);
            let resp = self
                .client
                .search(SearchParts::Index(&[&index]))
                .body(json!({
                    "size": MAX_CHILDREN,
                    "query": {"terms": {"parents": frontier}}
                }))
                .send()
                .await?;
            let body = resp.json::<Value>().await?;
            let mut next = Vec::new();
            if let Some(hits) = body["hits"]["hits"].as_array() {
                for hit in hits {
                    let id = hit["_id"].as_str().unwrap_or_default().to_string();
                    if !visited.insert(id.clone()) {
                        continue;
                    }
                    let kind =
                        serde_json::from_value::<NodeKind>(hit["_source"]["kind"].clone()).ok();
                    if kind == Some(NodeKind::Collection) {
                        collections.insert(id.clone());
                    }
                    next.push(id);
                }
            }
            frontier = next;
        }
        Ok(collections)
    }

    // --- STAC document storage ---

    pub async fn index_document(
        &self,
        index: &str,
        id: &str,
        doc: impl Serialize,
    ) -> Result<(), opensearch::Error> {
        let index = self.idx(index);
        self.client
            .index(IndexParts::IndexId(&index, id))
            .body(doc)
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(())
    }

    pub async fn get_document(
        &self,
        index: &str,
        id: &str,
    ) -> Result<Option<Value>, opensearch::Error> {
        let index = self.idx(index);
        let resp = self
            .client
            .get(GetParts::IndexId(&index, id))
            .send()
            .await?;
        if resp.status_code() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp.json::<Value>().await?;
        Ok(Some(body["_source"].clone()))
    }

    pub async fn delete_document(&self, index: &str, id: &str) -> Result<(), opensearch::Error> {
        let index = self.idx(index);
        self.client
            .delete(DeleteParts::IndexId(&index, id))
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(())
    }

    /// Fetch many documents by id (missing ids are skipped).
    pub async fn get_documents(
        &self,
        index: &str,
        ids: &[String],
    ) -> Result<Vec<Value>, opensearch::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let index = self.idx(index);
        let resp = self
            .client
            .mget(MgetParts::Index(&index))
            .body(json!({"ids": ids}))
            .send()
            .await?;
        let body = resp.json::<Value>().await?;
        Ok(body["docs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["found"] == true)
            .map(|d| d["_source"].clone())
            .collect())
    }

    /// Item search scoped to a set of collections.
    /// Returns (item docs, total_matched) — sources pass through verbatim
    /// since ItemCollection supports the fields extension.
    /// Translates collections/ids/bbox/intersects/datetime/limit plus
    /// `offset` (unmodeled, arrives via `additional_fields`); sortby,
    /// fields and cursor/deep pagination remain future work.
    pub async fn search_items(
        &self,
        collections: &[String],
        search: &Search,
    ) -> Result<(Vec<Map<String, Value>>, u64), opensearch::Error> {
        let limit = search
            .items
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .min(MAX_SEARCH_LIMIT);
        let offset = search_offset(search);

        let mut filter = vec![json!({"terms": {"collection": collections}})];
        if !search.ids.is_empty() {
            // _id == item.id (docs are indexed under their own id)
            filter.push(json!({"ids": {"values": search.ids}}));
        }
        if let Some(bbox) = &search.items.bbox {
            filter.push(bbox_filter(bbox));
        }
        if let Some(geom) = &search.intersects {
            // geojson::Geometry serializes to a GeoJSON shape literal
            filter.push(json!({"geo_shape": {"geometry": {
                "shape": serde_json::to_value(geom).unwrap_or_default(),
                "relation": "intersects"
            }}}));
        }
        if let Some(datetime) = &search.items.datetime {
            filter.push(datetime_filter(datetime));
        }

        // sortby -> OpenSearch `sort` clauses; `_id` is appended as a
        // tiebreaker so paging is deterministic.
        let mut sort: Vec<Value> = search
            .items
            .sortby
            .iter()
            .map(|s| {
                json!({sort_field(&s.field): {
                    "order": s.direction,
                    "missing": "_last"
                }})
            })
            .collect();
        if !sort.is_empty() {
            sort.push(json!({"_id": {"order": "asc"}}));
        }

        let index = self.idx(ITEMS_INDEX);
        let mut body = json!({
            "size": limit,
            "from": offset,
            "track_total_hits": true,
            "query": {"bool": {"filter": filter}}
        });
        if !sort.is_empty() {
            body["sort"] = json!(sort);
        }
        let resp = self
            .client
            .search(SearchParts::Index(&[&index]))
            .body(body)
            .send()
            .await?;
        let body = resp.json::<Value>().await?;
        let total = body["hits"]["total"]["value"].as_u64().unwrap_or(0);
        let items = body["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["_source"].as_object().cloned())
            .collect();
        Ok((items, total))
    }

    // --- node doc helpers ---

    async fn get_node(&self, node_id: &str) -> Result<Option<HierarchyNode>, opensearch::Error> {
        let index = self.idx(HIERARCHY_INDEX);
        let resp = self
            .client
            .get(GetParts::IndexId(&index, node_id))
            .send()
            .await?;
        if resp.status_code() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = resp.json::<Value>().await?;
        Ok(serde_json::from_value(body["_source"].clone()).ok())
    }

    async fn put_node(&self, node_id: &str, node: &HierarchyNode) -> Result<(), opensearch::Error> {
        let index = self.idx(HIERARCHY_INDEX);
        self.client
            .index(IndexParts::IndexId(&index, node_id))
            .body(node)
            .refresh(opensearch::params::Refresh::WaitFor)
            .send()
            .await?;
        Ok(())
    }
}

/// sortby field -> document field. Top-level item fields pass through;
/// anything else is a `properties.*` member (bare `datetime` included).
fn sort_field(field: &str) -> String {
    match field {
        "id" | "collection" | "bbox" | "geometry" | "type" => field.to_string(),
        f if f.contains('.') => f.to_string(),
        f => format!("properties.{f}"),
    }
}

/// Page offset — `offset` isn't a modeled `Search` field, it arrives via
/// `additional_fields` (same trick stac-server's duckdb backend uses).
pub fn search_offset(search: &Search) -> u64 {
    search
        .items
        .additional_fields
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// STAC `bbox` -> geo_shape envelope query (intersects semantics).
/// Envelope corners are [[min_lon, max_lat], [max_lon, min_lat]].
fn bbox_filter(bbox: &Bbox) -> Value {
    json!({"geo_shape": {"geometry": {
        "shape": {"type": "envelope", "coordinates": [
            [bbox.xmin(), bbox.ymax()],
            [bbox.xmax(), bbox.ymin()]
        ]},
        "relation": "intersects"
    }}})
}

/// An interval endpoint: empty / ".." / "null" means open-ended.
fn dt_bound(s: &str) -> Option<&str> {
    match s {
        "" | ".." | "null" => None,
        v => Some(v),
    }
}

/// STAC `datetime` -> temporal filter. Covers both instant items
/// (`properties.datetime`) and ranged items (`datetime: null` +
/// `start_datetime`/`end_datetime`), matching either if it overlaps the
/// query interval. A bare instant is treated as a zero-width interval.
fn datetime_filter(datetime: &str) -> Value {
    let (start, end) = match datetime.split_once('/') {
        Some((s, e)) => (dt_bound(s), dt_bound(e)),
        None => (dt_bound(datetime), dt_bound(datetime)),
    };

    let mut instant_range = Map::new();
    if let Some(s) = start {
        instant_range.insert("gte".into(), json!(s));
    }
    if let Some(e) = end {
        instant_range.insert("lte".into(), json!(e));
    }
    if instant_range.is_empty() {
        return json!({"match_all": {}});
    }

    // Ranged items overlap [start, end] iff start_datetime <= end AND
    // end_datetime >= start (open bounds are simply omitted).
    let mut ranged = Vec::new();
    if let Some(e) = end {
        ranged.push(json!({"range": {"properties.start_datetime": {"lte": e}}}));
    }
    if let Some(s) = start {
        ranged.push(json!({"range": {"properties.end_datetime": {"gte": s}}}));
    }
    ranged.push(
        json!({"bool": {"must_not": {"exists": {"field": "properties.datetime"}}}}),
    );

    json!({"bool": {"should": [
        {"range": {"properties.datetime": instant_range}},
        {"bool": {"filter": ranged}}
    ], "minimum_should_match": 1}})
}

/// Mode B link semantics on a node's parent list: linking to a real
/// catalog drops the implicit root parent; duplicates are ignored.
fn apply_link(parents: &mut Vec<String>, parent_id: &str) {
    if parent_id != ROOT_CATALOG_ID {
        parents.retain(|p| p != ROOT_CATALOG_ID);
    }
    if !parents.iter().any(|p| p == parent_id) {
        parents.push(parent_id.to_string());
    }
}

/// Unlink semantics: removing the last parent triggers root adoption.
fn apply_unlink(parents: &mut Vec<String>, parent_id: &str) {
    parents.retain(|p| p != parent_id);
    if parents.is_empty() {
        parents.push(ROOT_CATALOG_ID.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_link_drops_implicit_root() {
        let mut parents = vec![ROOT_CATALOG_ID.to_string()];
        apply_link(&mut parents, "real-parent");
        assert_eq!(parents, vec!["real-parent"]);
    }

    #[test]
    fn test_link_keeps_explicit_parents() {
        let mut parents = vec!["a".to_string(), ROOT_CATALOG_ID.to_string()];
        apply_link(&mut parents, "b");
        assert_eq!(parents, vec!["a", "b"]);
    }

    #[test]
    fn test_unlink_adopts_orphan_to_root() {
        let mut parents = vec!["parent".to_string()];
        apply_unlink(&mut parents, "parent");
        assert_eq!(parents, vec![ROOT_CATALOG_ID]);
    }

    #[test]
    fn test_unlink_keeps_remaining_parents() {
        let mut parents = vec!["a".to_string(), "b".to_string()];
        apply_unlink(&mut parents, "a");
        assert_eq!(parents, vec!["b"]);
    }

    #[test]
    fn test_sort_field_mapping() {
        assert_eq!(sort_field("id"), "id");
        assert_eq!(sort_field("collection"), "collection");
        assert_eq!(sort_field("datetime"), "properties.datetime");
        assert_eq!(sort_field("properties.created"), "properties.created");
        assert_eq!(sort_field("eo:cloud_cover"), "properties.eo:cloud_cover");
    }

    #[test]
    fn test_bbox_filter_envelope() {
        let q = bbox_filter(&Bbox::new(-122.6, 36.9, -120.3, 38.2));
        let shape = &q["geo_shape"]["geometry"]["shape"];
        assert_eq!(shape["type"], "envelope");
        assert_eq!(
            shape["coordinates"],
            json!([[-122.6, 38.2], [-120.3, 36.9]])
        );
        assert_eq!(q["geo_shape"]["geometry"]["relation"], "intersects");
    }

    #[test]
    fn test_datetime_filter_instant() {
        let q = datetime_filter("2023-06-15T00:00:00Z");
        let should = q["bool"]["should"].as_array().unwrap();
        // instant items: exact instant; ranged items: start<=v<=end
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-15T00:00:00Z", "lte": "2023-06-15T00:00:00Z"})
        );
    }

    #[test]
    fn test_datetime_filter_interval() {
        let q = datetime_filter("2023-06-01/2023-07-01");
        let should = q["bool"]["should"].as_array().unwrap();
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-01", "lte": "2023-07-01"})
        );
        let ranged = should[1]["bool"]["filter"].as_array().unwrap();
        assert_eq!(
            ranged[0]["range"]["properties.start_datetime"],
            json!({"lte": "2023-07-01"})
        );
        assert_eq!(
            ranged[1]["range"]["properties.end_datetime"],
            json!({"gte": "2023-06-01"})
        );
    }

    #[test]
    fn test_datetime_filter_open_interval() {
        // "2023-06-01/.." -> only a lower bound on both branches
        let q = datetime_filter("2023-06-01/..");
        let should = q["bool"]["should"].as_array().unwrap();
        assert_eq!(
            should[0]["range"]["properties.datetime"],
            json!({"gte": "2023-06-01"})
        );
        let ranged = should[1]["bool"]["filter"].as_array().unwrap();
        assert_eq!(ranged.len(), 2); // end_datetime gte + no-datetime guard
        assert_eq!(
            ranged[0]["range"]["properties.end_datetime"],
            json!({"gte": "2023-06-01"})
        );
    }
}
