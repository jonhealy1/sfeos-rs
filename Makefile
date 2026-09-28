OPENSEARCH_URL ?= http://localhost:9200
STAC_API_URL ?= http://localhost:3000
COMPOSE := docker compose

.PHONY: up down reset opensearch test test-integration test-unit test-clean ingest check fmt

up: ## Start the full stack (api + opensearch + dashboards)
	$(COMPOSE) up -d --build --wait

opensearch: ## Start just OpenSearch and wait for it to be healthy
	$(COMPOSE) up -d --wait opensearch

down: ## Stop all containers (keeps data volumes)
	$(COMPOSE) down

reset: ## Stop, wipe OpenSearch data, restart clean
	$(COMPOSE) down -v
	$(COMPOSE) up -d --build --wait

# Tests run against isolated it-* indices; the trailing curl sweeps them
# (runs even when the suite fails — status is preserved via $$status).
test: opensearch ## Unit + integration tests (starts OpenSearch first)
	@OPENSEARCH_URL=$(OPENSEARCH_URL) cargo test; status=$$?; \
	curl -sf -XDELETE '$(OPENSEARCH_URL)/it-*' > /dev/null 2>&1 || true; \
	exit $$status

test-integration: opensearch ## Integration tests only
	@OPENSEARCH_URL=$(OPENSEARCH_URL) cargo test --test integration --test catalogs; status=$$?; \
	curl -sf -XDELETE '$(OPENSEARCH_URL)/it-*' > /dev/null 2>&1 || true; \
	exit $$status

test-unit: ## Unit tests only (no OpenSearch needed)
	cargo test --lib

test-clean: ## Delete leftover it-* test indices without running tests
	curl -sf -XDELETE '$(OPENSEARCH_URL)/it-*' || true

ingest: ## Load sample_data into the running API
	STAC_API_URL=$(STAC_API_URL) python3 scripts/ingest_sample_data.py

check: ## cargo fmt check + clippy + test-compile
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test --no-run

fmt: ## Auto-format
	cargo fmt
