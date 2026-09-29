#!/usr/bin/env python3
"""Ingest sample_data/ into the STAC multi-tenant catalogs API.

The folder layout mirrors the desired hierarchy:

    sample_data/catalogs/
        <root-catalog>/                 must contain catalog.json
            catalog.json                -> POST /catalogs
            catalogs/<sub-catalog>/
                catalog.json            -> POST /catalogs/{id}/catalogs
                collections/<collection>/
                    collection.json     -> POST /catalogs/{id}/collections
                    items/*.json        -> POST /catalogs/{id}/collections/{cid}/items

Resource ids come from each JSON document's "id" field; folder names are
organizational (keep them matching the doc id for readability).

Requires the transaction extension (ENABLE_TRANSACTIONS_EXTENSIONS=true) —
all endpoints used here are writes.

Usage:
    python3 scripts/ingest_sample_data.py [--api http://localhost:3000]
    STAC_API_URL=http://localhost:3000 python3 scripts/ingest_sample_data.py
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

FAILED = 0


def post(api: str, path: str, doc: dict) -> int:
    req = urllib.request.Request(
        f"{api}{path}",
        data=json.dumps(doc).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return resp.status
    except urllib.error.HTTPError as e:
        return e.code
    except urllib.error.URLError:
        return -1


def emit(status: int, kind: str, doc_id: str, indent: int) -> bool:
    global FAILED
    # 409 = doc already exists — idempotent re-ingest is fine
    ok = status in (200, 201, 409)
    if not ok:
        FAILED += 1
    mark = {409: "exists"}.get(status) or ("ok" if ok else f"FAIL {status}")
    print(f"{'  ' * indent}[{mark}] {kind}: {doc_id}")
    if status == 405:
        print(
            "        ^ 405 — is ENABLE_TRANSACTIONS_EXTENSIONS set? "
            "Write routes are disabled without it."
        )
    if status == -1:
        print("        ^ connection refused — is the API running?")
    return ok


def load(path: Path) -> dict:
    with open(path) as f:
        return json.load(f)


def ingest_collection(api: str, catalog_id: str, col_dir: Path, indent: int) -> None:
    collection = load(col_dir / "collection.json")
    col_id = collection["id"]
    status = post(api, f"/catalogs/{catalog_id}/collections", collection)
    emit(status, "collection", col_id, indent)

    items_dir = col_dir / "items"
    if items_dir.is_dir():
        for item_path in sorted(items_dir.glob("*.json")):
            item = load(item_path)
            status = post(
                api,
                f"/catalogs/{catalog_id}/collections/{col_id}/items",
                item,
            )
            emit(status, "item", item["id"], indent + 1)


def ingest_sub_catalog(api: str, parent_id: str, cat_dir: Path, indent: int) -> None:
    sub = load(cat_dir / "catalog.json")
    sub_id = sub["id"]
    status = post(api, f"/catalogs/{parent_id}/catalogs", sub)
    emit(status, "catalog", sub_id, indent)

    collections_dir = cat_dir / "collections"
    if collections_dir.is_dir():
        for col_dir in sorted(p for p in collections_dir.iterdir() if p.is_dir()):
            ingest_collection(api, sub_id, col_dir, indent + 1)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--api", default=os.environ.get("STAC_API_URL", "http://localhost:3000"))
    ap.add_argument("--data", type=Path, default=Path(__file__).resolve().parent.parent / "sample_data")
    args = ap.parse_args()

    catalogs_root = args.data / "catalogs"
    if not catalogs_root.is_dir():
        print(f"no {catalogs_root} found", file=sys.stderr)
        return 1

    print(f"Ingesting {args.data} -> {args.api}\n")

    for cat_dir in sorted(p for p in catalogs_root.iterdir() if p.is_dir()):
        catalog = load(cat_dir / "catalog.json")
        cat_id = catalog["id"]
        status = post(args.api, "/catalogs", catalog)
        if not emit(status, "catalog", cat_id, 0):
            continue  # skip subtree if the root failed

        subs_dir = cat_dir / "catalogs"
        if subs_dir.is_dir():
            for sub_dir in sorted(p for p in subs_dir.iterdir() if p.is_dir()):
                ingest_sub_catalog(args.api, cat_id, sub_dir, 1)

        collections_dir = cat_dir / "collections"
        if collections_dir.is_dir():
            for col_dir in sorted(p for p in collections_dir.iterdir() if p.is_dir()):
                ingest_collection(args.api, cat_id, col_dir, 1)

    print(f"\n{'Done.' if FAILED == 0 else f'{FAILED} request(s) failed.'}")
    return 0 if FAILED == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
