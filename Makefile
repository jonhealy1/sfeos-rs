OPENSEARCH_URL ?= http://localhost:9200
STAC_API_URL ?= http://localhost:3000
COMPOSE := docker compose

.PHONY: up down reset opensearch test test-integration test-unit ingest check fmt

up: ## Start the full stack (api + opensearch + dashboards)
	$(COMPOSE) up -d --build --wait

opensearch: ## Start just OpenSearch and wait for it to be healthy
	$(COMPOSE) up -d --wait opensearch

down: ## Stop all containers (keeps data volumes)
	$(COMPOSE) down

reset: ## Stop, wipe OpenSearch data, restart clean
	$(COMPOSE) down -v
	$(COMPOSE) up -d --build --wait

test: opensearch ## Unit + integration tests (starts OpenSearch first)
	OPENSEARCH_URL=$(OPENSEARCH_URL) cargo test

test-integration: opensearch ## Integration tests only
	OPENSEARCH_URL=$(OPENSEARCH_URL) cargo test --test integration

test-unit: ## Unit tests only (no OpenSearch needed)
	cargo test --lib

ingest: ## Load sample_data into the running API
	STAC_API_URL=$(STAC_API_URL) python3 scripts/ingest_sample_data.py

check: ## cargo fmt check + clippy + test-compile
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test --no-run

fmt: ## Auto-format
	cargo fmt
