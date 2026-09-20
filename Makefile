# Native runtime development and checks. Foundation lifecycle: hibana platform.
ifneq (,$(wildcard .env))
include .env
export
endif
BASE_URL ?= http://127.0.0.1:8080
SMOKE_TENANT_SLUG ?= smoke
SMOKE_TENANT_NAME ?= Smoke Tenant
SMOKE_EMAIL ?= admin@example.com
SMOKE_OIDC_SUBJECT ?=
.DEFAULT_GOAL := help
.PHONY: help setup up down migrate run-cp run-worker bootstrap login test rls-lint logs psql
help:
	@echo "setup | up | down | migrate | run-cp | run-worker | bootstrap | login | test | rls-lint | logs | psql"
setup:
	rustup target add wasm32-wasip2
	npm ci --prefix sdk
up:
	docker compose up -d --wait postgres redis minio
	docker compose run --rm minio-setup
down:
	docker compose down
migrate:
	cargo run -p hibana-control-plane --bin hibana-control-plane -- --migrate-only
run-cp:
	cargo run --release -p hibana-control-plane --bin hibana-control-plane
run-worker:
	cargo run --release -p hibana-worker --bin hibana-worker
bootstrap: ## M3a: テナント + 最初の admin ユーザを 1 回で作成（POST /admin/tenants, §9 bootstrap）
	@set -e; \
	echo "==> POST /admin/tenants (slug=$(SMOKE_TENANT_SLUG), admin=$(SMOKE_EMAIL))"; \
	BODY_FILE=$$(mktemp); \
	trap 'rm -f "$$BODY_FILE"' EXIT; \
	CODE=$$(curl -sS -o "$$BODY_FILE" -w '%{http_code}' -X POST "$(BASE_URL)/admin/tenants" \
		-H "Authorization: Bearer $(BOOTSTRAP_ADMIN_TOKEN)" \
		-H "Content-Type: application/json" \
		-d '{"slug":"$(SMOKE_TENANT_SLUG)","name":"$(SMOKE_TENANT_NAME)","admin_email":"$(SMOKE_EMAIL)","admin_oidc_subject":"$(SMOKE_OIDC_SUBJECT)"}'); \
	TBODY=$$(cat "$$BODY_FILE"); \
	echo "    [$$CODE] $$TBODY"; \
	if [ "$$CODE" = "409" ] || printf '%s' "$$TBODY" | grep -q 'tenant slug already exists'; then \
		echo "Tenant already exists; verify its OIDC identity with make login."; exit 0; \
	fi; \
	test "$$CODE" = "201" || { echo "ERROR: bootstrap に失敗しました (HTTP $$CODE)" >&2; exit 1; }; \
	TID=$$(printf '%s' "$$TBODY" | sed -n 's/.*"tenant_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'); \
	test -n "$$TID" || { echo "ERROR: bootstrap 応答から tenant_id を取得できませんでした。" >&2; exit 1; }; \
	echo "OK: tenant_id=$$TID + admin ユーザ($(SMOKE_EMAIL)) を作成しました。'make login' でOIDCログインできます。"

login: ## OIDC browser login; saves the connection in the CLI profile
	node sdk/src/cli.mjs login --url "$(BASE_URL)" --tenant "$(SMOKE_TENANT_SLUG)"

test:
	python3 scripts/check-architecture.py
	cargo test --workspace
	npm test --prefix sdk
	python3 scripts/check-kubernetes.py
	python3 sdk/platform/test_kubernetes.py
	python3 scripts/test_resilience.py
rls-lint:
	bash scripts/rls-lint.sh
logs:
	docker compose logs -f
psql:
	docker compose exec postgres psql -U faas -d faas
