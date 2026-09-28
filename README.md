# stac-rust-catalogs-server

![StacLabs](https://github.com/StacLabs/.github/raw/main/profile/staclabs-orange-banner.png)

A STAC API Opensearch server built with Rust

## Contents

- [What is this?](#what-is-this)
- [API Routes](#api-routes)
- [Getting Started (coming from Python?)](#getting-started-coming-from-python)
- [Sample Data](#sample-data)
- [TODO](#todo)

## What is this?

A **hybrid** of the STAC API core spec and the [multi-tenant-catalogs extension](https://github.com/StacLabs/multi-tenant-catalogs): catalogs form a poly-hierarchy (a DAG — a catalog or collection can live under multiple parents), and every STAC capability is exposed *scoped* under `/catalogs/{catalog_id}` rather than only at the API root. See [API Routes](#api-routes) for the full surface.

The catalog DAG is stored in OpenSearch as one `{kind, parents}` document per node (`stac-hierarchy` index) — children are derived via `term` queries on `parents`, so linking a shared resource is a single-document write, and orphans are automatically adopted under `root`.

**Beyond the spec:** this project also exposes full STAC *transaction* capabilities inside the `/catalogs` scope (create/update/delete catalogs, collections, and items). That goes beyond what the multi-tenant-catalogs extension currently specifies — we're prototyping it here with the intent of feeding it back into the spec. All write endpoints are gated behind a flag:

```bash
ENABLE_TRANSACTIONS_EXTENSIONS=true   # unsets → every mutating route returns 405
```

When unset, only the read surface is mounted and the landing page `conformsTo` omits the transaction conformance URIs.

## API Routes

### Read surface — always mounted

| Method | Path | Description |
|---|---|---|
| GET | `/` | Landing page (`conformsTo`, links) |
| GET | `/catalogs` | List top-level catalogs |
| GET | `/catalogs/{catalog_id}` | Fetch a catalog |
| GET | `/catalogs/{catalog_id}/children` | List children (`?type=Catalog\|Collection`) |
| GET | `/catalogs/{catalog_id}/collections` | List collections in scope |
| GET | `/catalogs/{catalog_id}/collections/{collection_id}` | Fetch a scoped collection |
| GET | `/catalogs/{catalog_id}/collections/{collection_id}/items` | List items in a scoped collection |
| GET | `/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}` | Fetch a scoped item |
| GET/POST | `/catalogs/search` | Search across the whole catalogs registry (scope = `root`) |
| GET/POST | `/catalogs/{catalog_id}/search` | Scoped search (`Search` body intersected with descendants) |

### Transactions — require `ENABLE_TRANSACTIONS_EXTENSIONS`

| Method | Path | Description |
|---|---|---|
| POST | `/catalogs` | Create a root-level catalog |
| PUT | `/catalogs/{catalog_id}` | Update a catalog |
| DELETE | `/catalogs/{catalog_id}` | Disband (children adopted by `root`) |
| POST | `/catalogs/{catalog_id}/catalogs` | Create (Mode A) or link (Mode B `{"id": ...}`) a sub-catalog |
| DELETE | `/catalogs/{catalog_id}/catalogs/{sub_id}` | Unlink a sub-catalog |
| POST | `/catalogs/{catalog_id}/collections` | Create (Mode A) or link (Mode B) a collection |
| PUT | `/catalogs/{catalog_id}/collections/{collection_id}` | Update a collection |
| DELETE | `/catalogs/{catalog_id}/collections/{collection_id}` | Unlink a collection |
| POST | `/catalogs/{catalog_id}/collections/{collection_id}/items` | Create an item |
| PUT | `/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}` | Update an item |
| DELETE | `/catalogs/{catalog_id}/collections/{collection_id}/items/{item_id}` | Delete an item |

Scoped reads 404 when the target collection isn't inside the catalog's DAG. Item payloads whose `collection` field contradicts the path get a 400.

## Getting Started (coming from Python?)

Rust projects don't use `pip` or virtualenvs — dependencies live in `Cargo.toml` (like `pyproject.toml`) and are fetched automatically by `cargo`, the Rust build/package tool.

**1. Install the Rust toolchain** (gives you `cargo`, think "pip + interpreter in one"):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
cargo --version   # verify install
```

**2. Start the stack** — `compose.yml` runs the API, a single-node OpenSearch dev cluster, and Dashboards:

```bash
docker compose up -d          # API :3000, OpenSearch :9200, Dashboards :5601
```

**3. Or run the API locally** — iterate on Rust code while OpenSearch stays in Docker:

```bash
docker compose up -d opensearch
cargo run     # API on http://localhost:3000
```

Config via env vars: `OPENSEARCH_URL` (default `http://localhost:9200`), `ENABLE_TRANSACTIONS_EXTENSIONS` (enables all write endpoints; set in `compose.yml` by default).

Other handy commands: `cargo check` (fast type-check, no binary), `cargo test` (unit + integration tests — integration tests need OpenSearch running, and skip automatically if it's not), `cargo add <crate>` (add a dependency).

Or use the **Makefile**, which handles the Docker dependency for you (`make test` / `make test-integration` start OpenSearch and wait for it to be healthy before running tests): `make up`, `make test`, `make test-integration`, `make ingest`, `make reset` (wipes data volumes), `make down`.

## Sample Data

`sample_data/` contains a demo hierarchy — folder structure mirrors the DAG:

```
earth-observation/          # root catalog
├── landsat/                # sub-catalog
│   └── landsat-c2-l2/      # collection + 2 items
└── sentinel/
    └── sentinel-2-l2a/     # collection + 2 items
climate-research/           # root catalog
└── era5/
    └── era5-daily/         # collection + 2 items
```

Load it (requires `ENABLE_TRANSACTIONS_EXTENSIONS`, on by default in `compose.yml`):

```bash
python3 scripts/ingest_sample_data.py        # STAC_API_URL to override :3000
```

Then try scoped search: `POST /catalogs/earth-observation/search` returns its 4 satellite items; `POST /catalogs/search` searches the whole registry.

## TODO

This is a prototype — the endpoints exist but several are shallow. Known gaps:

**Search**
- `collections`, `ids`, `bbox`, `intersects`, `datetime`, and `limit` are translated to OpenSearch queries — `sortby`, `fields`, and `query` are parsed but ignored
- `datetime` bounds must be full RFC3339 datetimes — date-only inputs (`2023-06-01`) are rejected at validation
- No CQL2 / filter extension support
- No pagination (`limit` caps page size at 10k; `page`/`token`/next-page links don't exist)

**Core STAC routes** (links already point here — currently dead until implemented)
- `GET/POST /search`, `/collections`, `/collections/{id}`, `/collections/{id}/items` at the root
- Item links: items return `links: []` (catalog/collection docs already get DAG-derived links)
- No `/conformance` page or `/queryables` endpoints

**Hierarchy & data**
- Children/descendants capped at 10k per level — real pagination needed on the DAG itself
- No optimistic concurrency: concurrent link/unlink on the same node can lost-update (needs `_seq_no`/`_primary_term` or scripted upserts)
- Reserved IDs: a catalog named `search` collides with the static route — should 400 on create
- Orphaned docs: a hierarchy node whose document is missing is silently skipped in children listings — needs a consistency check

**Ops**
- Integration tests write `it-*` fixtures into the same indices as dev data — needs index isolation or cleanup
- OpenSearch runs single-node with the security plugin disabled — dev only, harden before anything else
- No auth or per-tenant authorization — catalog scope is organizational only, not a security boundary
- Mode B link allows linking *any* existing resource id with no existence validation on the parent
