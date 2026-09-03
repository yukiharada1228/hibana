# WASM FaaS Platform — M2 (Component ライフサイクルとストレージ) 開発用 Makefile
# 仕様書 §15 M2。すべてローカル / 隔離環境専用（露出ガード §15）。
#
# 代表的な一連の流れ:
#   make setup            # ツールチェイン/target/wasm-tools 等の前提を確認・導入補助
#   make up               # docker compose で postgres + nats(JetStream) + minio 起動
#   make migrate          # migrations/*.sql を適用（default テナント seed 込み）
#   make build-component  # echo を wasm32-wasip2 でビルド（アップロード対象のローカル成果物）
#   make run-cp           # control-plane 起動（別ターミナル）
#   make run-worker       # worker 起動（別ターミナル）
#   make bootstrap        # スモーク用テナント + admin ユーザ作成（初回のみ）
#   make deploy           # echo を登録しアップロード（POST /components → /components/{id}/versions）
#   make invoke           # echo を end-to-end で呼ぶ（deploy → POST /invoke → GET /executions）

# .env があれば読み込む。無ければ .env.example の既定値を使う。
ifneq (,$(wildcard .env))
include .env
export
endif

# --- 既定値（.env / .env.example と一致）。.env で上書き可。 ---
# M3b(§3.2): ランタイムは非特権 faas_app（NOBYPASSRLS）。マイグレーションだけ特権ロール faas。
DATABASE_URL          ?= postgres://faas_app:faas_app@localhost:5432/faas
MIGRATION_DATABASE_URL ?= postgres://faas:faas@localhost:5432/faas
NATS_URL              ?= nats://localhost:4222
# M3a: 固定 AUTH_TOKEN は廃止。system-admin の bootstrap トークンで POST /admin/tenants を gate。
BOOTSTRAP_ADMIN_TOKEN ?= dev-bootstrap-admin-token
COMPONENTS_DIR        ?= ./components-dist
BIND_ADDR             ?= 0.0.0.0:8080

# --- M3a: スモークテスト用のテナント/ユーザ資格情報（make bootstrap が作成） ---
SMOKE_TENANT_SLUG     ?= smoke
SMOKE_TENANT_NAME     ?= Smoke Tenant
SMOKE_EMAIL           ?= admin@example.com
SMOKE_PASSWORD        ?= dev-password

# --- Object Storage（M2: MinIO, S3 互換, path-style。§3.4） ---
S3_ENDPOINT           ?= http://localhost:9000
S3_REGION             ?= us-east-1
S3_BUCKET             ?= faas-components
S3_ACCESS_KEY         ?= minioadmin
S3_SECRET_KEY         ?= minioadmin

# --- アップロード/検証パイプライン（M2: §6.2） ---
MAX_WASM_UPLOAD_BYTES ?= 33554432
PRESIGN_TTL_SECS      ?= 300

# --- Worker: 取得した wasm 本体のローカルキャッシュ先（M2: §3.6） ---
WASM_CACHE_DIR        ?= ./worker-cache

# --- ジョブ署名トークン（M3c: 結果の出所認証。§3.3）。CP だけが鍵を持つ。 ---
# 開発用の決定的ダミー seed（32 バイト base64url）。本番は高エントロピーに置換すること（露出ガード §15）。
JOB_SIGNING_KEY       ?= AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8
JOB_SIGNING_KID       ?= k1
# TTL 結合: token exp = iat + ACK_WAIT_SECS*MAX_DELIVER + 壁時計上限 + TOKEN_MARGIN_SECS。
# CP と worker で同一値にすること（worker の consumer も ACK_WAIT_SECS/MAX_DELIVER を読む）。
ACK_WAIT_SECS         ?= 30
MAX_DELIVER           ?= 5
TOKEN_MARGIN_SECS     ?= 60

# --- 共有 admission ストア / クォータ（M3d: §8）。全 Axum インスタンスで共有するカウンタ。 ---
REDIS_URL                       ?= redis://localhost:6379
QUOTA_INVOKE_RATE_PER_SEC       ?= 50
QUOTA_INVOKE_BURST              ?= 500
QUOTA_MAX_CONCURRENT_EXECUTIONS ?= 20
INFLIGHT_TTL_SECS               ?= 3600
REAPER_INTERVAL_SECS            ?= 30
# login 失敗ロックアウト（§6.0）: (tenant,email) + IP 両キー、fail-closed。
LOGIN_LOCKOUT_THRESHOLD         ?= 10
LOGIN_LOCKOUT_WINDOW_SECS       ?= 900
TRUST_PROXY_HEADERS             ?= false
# 大 I/O アップロード（§3.4/§5.2）: presigned PUT URL の TTL（秒）。
UPLOAD_PRESIGN_TTL_SECS         ?= 300

# --- M6: 同期 Invoke / Cron スケジューラ（§15） ---
# INSTANCE_ID は未設定なら CP 起動時に inst_{uuid} を採番する（Axum×N では一意な値を与えること）。
INSTANCE_ID                     ?=
SYNC_REPLY_TIMEOUT_MS           ?= 5000
CRON_POLL_INTERVAL_SECS         ?= 10

# invoke / deploy ターゲット用。BIND_ADDR の 0.0.0.0 は curl 先として localhost に読み替える。
BASE_URL ?= http://localhost:$(lastword $(subst :, ,$(BIND_ADDR)))

# deploy するバージョン（semver）。
VERSION ?= 0.1.0

# M4 chaos_d 用 slow component のリソース上限。ゲストが tight loop で回り続けるため、
# epoch interruption（max_wall_time）と tokio timeout（max_execution_time）の両方が
# 短時間で発火する値にする（既定の 1s / 5s だとテストの待ち窓に収まらないことがある）。
SLOW_LIMITS ?= {"max_wall_time_ms":1000,"max_execution_time_ms":2000}

# echo component の wasm32-wasip2 ビルド成果物パス（アップロード対象のローカル成果物）。
ECHO_WASM := target/wasm32-wasip2/release/echo.wasm

.DEFAULT_GOAL := help

.PHONY: recreate-stream deploy-chaos-components component-id traffic canary promote rollback approve-env set-secret secrets rekey help setup up down migrate minio-bucket build-component run-cp run-worker bootstrap login deploy invoke logs psql clean rls-lint

help: ## 利用可能なターゲット一覧を表示
	@echo "WASM FaaS Platform — M2 Makefile"
	@echo
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

setup: ## 前提ツール（rustup / wasm32-wasip2 target / wasm-tools）の導入を確認・補助
	@command -v rustup >/dev/null 2>&1 || { \
		echo "ERROR: rustup が見つかりません。https://rustup.rs から導入してください。"; exit 1; }
	@echo "==> rust-toolchain.toml に従い toolchain / target を導入..."
	rustup show
	rustup target add wasm32-wasip2
	@command -v wasm-tools >/dev/null 2>&1 || { \
		echo "==> wasm-tools をインストール..."; cargo install wasm-tools; }
	@echo "==> COMPONENTS_DIR を作成: $(COMPONENTS_DIR)"
	mkdir -p "$(COMPONENTS_DIR)"
	@echo "==> WASM_CACHE_DIR を作成: $(WASM_CACHE_DIR)"
	mkdir -p "$(WASM_CACHE_DIR)"
	@test -f .env || { cp .env.example .env && echo "==> .env を .env.example から作成しました"; }
	@echo "OK: setup 完了。次は 'make up' → 'make migrate' → 'make build-component'。"

up: ## docker compose で postgres + nats(JetStream) + redis + minio を起動（healthy 待ち）→ bucket 作成
	# 長寿命サービスのみ healthy 待ちする。ワンショットの minio-setup を --wait に含めると
	# 正常終了(exit 0)でも「終了した」と見なされ docker compose --wait が失敗するため除外する。
	# redis は M3d 共有 admission ストア（§8）。
	docker compose up -d --wait postgres nats redis minio
	@$(MAKE) --no-print-directory minio-bucket

down: ## docker compose を停止（volume は保持）
	docker compose down

migrate: ## sqlx migrator で migrations を冪等適用（baseline + pending を起動時と同一ロジックで処理）
	@echo "==> sqlx migrator (--migrate-only) を実行..."
	@# 起動時マイグレーションと同じ run_migrations() を呼ぶため、再実行しても already exists で落ちない。
	@# 0001/0002 が手動適用済みなら baseline 行を入れて skip し、0003 以降だけが適用される。
	@# 0004_rls.sql はテーブル所有者 + CREATEROLE 権限を要するため、MIGRATION_DATABASE_URL は
	@# 所有者ロール（既定: faas）にしておくこと（faas_app は NOBYPASSRLS で適用不可）。
	@MIGRATION_DATABASE_URL="$(MIGRATION_DATABASE_URL)" \
	 DATABASE_URL="$(DATABASE_URL)" \
		cargo run --quiet -p faas-control-plane -- --migrate-only
	@echo "OK: migration 適用完了（冪等。再実行しても安全）。"

minio-bucket: ## MinIO に bucket $(S3_BUCKET) を冪等に作成（compose の minio-setup と等価の手動版）
	@echo "==> bucket $(S3_BUCKET) を作成（既存なら no-op）..."
	docker compose run --rm --entrypoint /bin/sh minio-setup -c "\
		mc alias set local http://minio:9000 $(S3_ACCESS_KEY) $(S3_SECRET_KEY) && \
		mc mb --ignore-existing local/$(S3_BUCKET)"
	@echo "OK: bucket $(S3_BUCKET) を確認しました。"

build-component: ## echo を wasm32-wasip2 でビルドし COMPONENTS_DIR/echo.wasm へ配置（アップロード対象）
	cargo build -p echo --target wasm32-wasip2 --release
	mkdir -p "$(COMPONENTS_DIR)"
	cp "$(ECHO_WASM)" "$(COMPONENTS_DIR)/echo.wasm"
	@echo "OK: $(COMPONENTS_DIR)/echo.wasm を配置しました（make deploy でアップロードします）。"

run-cp: ## control-plane を起動（別ターミナルで）
	RUST_LOG=$${RUST_LOG:-info} cargo run -p faas-control-plane

run-worker: ## worker を起動（別ターミナルで。WASM_CACHE_DIR に cwasm をキャッシュ）
	RUST_LOG=$${RUST_LOG:-info} cargo run -p faas-worker

bootstrap: ## M3a: テナント + 最初の admin ユーザを 1 回で作成（POST /admin/tenants, §9 bootstrap）
	@set -e; \
	echo "==> POST /admin/tenants (slug=$(SMOKE_TENANT_SLUG), admin=$(SMOKE_EMAIL))"; \
	BODY_FILE=$$(mktemp); \
	trap 'rm -f "$$BODY_FILE"' EXIT; \
	CODE=$$(curl -sS -o "$$BODY_FILE" -w '%{http_code}' -X POST "$(BASE_URL)/admin/tenants" \
		-H "Authorization: Bearer $(BOOTSTRAP_ADMIN_TOKEN)" \
		-H "Content-Type: application/json" \
		-d '{"slug":"$(SMOKE_TENANT_SLUG)","name":"$(SMOKE_TENANT_NAME)","admin_email":"$(SMOKE_EMAIL)","admin_password":"$(SMOKE_PASSWORD)"}'); \
	TBODY=$$(cat "$$BODY_FILE"); \
	echo "    [$$CODE] $$TBODY"; \
	if [ "$$CODE" = "409" ] || printf '%s' "$$TBODY" | grep -q 'tenant slug already exists'; then \
		if TOKEN=$$($(MAKE) -s login 2>/dev/null) && [ -n "$$TOKEN" ]; then \
			echo "OK: tenant $(SMOKE_TENANT_SLUG) と admin ユーザは作成済みです。"; exit 0; \
		fi; \
		echo "ERROR: tenant slug '$(SMOKE_TENANT_SLUG)' は既に存在しますが、設定した admin 資格情報ではログインできません。" >&2; \
		echo "       別の SMOKE_TENANT_SLUG を指定するか、既存ユーザの資格情報を指定してください。" >&2; exit 1; \
	fi; \
	test "$$CODE" = "201" || { echo "ERROR: bootstrap に失敗しました (HTTP $$CODE)" >&2; exit 1; }; \
	TID=$$(printf '%s' "$$TBODY" | sed -n 's/.*"tenant_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'); \
	test -n "$$TID" || { echo "ERROR: bootstrap 応答から tenant_id を取得できませんでした。" >&2; exit 1; }; \
	echo "OK: tenant_id=$$TID + admin ユーザ($(SMOKE_EMAIL)) を作成しました。'make login' でトークンを取得できます。"

login: ## POST /auth/login でトークンを標準出力に出す（TOKEN=$$(make -s login)）
	@set -e; \
	BODY_FILE=$$(mktemp); \
	trap 'rm -f "$$BODY_FILE"' EXIT; \
	CODE=$$(curl -sS -o "$$BODY_FILE" -w '%{http_code}' -X POST "$(BASE_URL)/auth/login" \
		-H "Content-Type: application/json" \
		-d '{"tenant_slug":"$(SMOKE_TENANT_SLUG)","email":"$(SMOKE_EMAIL)","password":"$(SMOKE_PASSWORD)"}'); \
	BODY=$$(cat "$$BODY_FILE"); \
	TOKEN=$$(printf '%s' "$$BODY" | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'); \
	test -n "$$TOKEN" || { echo "ERROR: ログインに失敗しました (HTTP $$CODE): $$BODY" >&2; exit 1; }; \
	printf '%s\n' "$$TOKEN"

deploy: ## echo を登録しアップロード（冪等。login でトークン取得 → POST /components → POST /components/{id}/versions）
	@set -e; \
	test -f "$(COMPONENTS_DIR)/echo.wasm" || { \
		echo "ERROR: $(COMPONENTS_DIR)/echo.wasm がありません。先に 'make build-component' を実行してください。"; exit 1; }; \
	if ! TOKEN=$$($(MAKE) -s login); then \
		echo "ERROR: 先に 'make bootstrap' でスモーク用テナントとユーザを作成してください。"; exit 1; \
	fi; \
	echo "==> POST /components (name=echo)"; \
	BODY=$$(curl -sS -X POST "$(BASE_URL)/components" \
		-H "Authorization: Bearer $$TOKEN" \
		-H "Content-Type: application/json" \
		-d '{"name":"echo"}'); \
	echo "    $$BODY"; \
	CID=$$(printf '%s' "$$BODY" | sed -n 's/.*"component_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'); \
	if [ -z "$$CID" ]; then \
		echo "    (既存の可能性 → GET /components から echo を解決)"; \
		CID=$$(curl -sS "$(BASE_URL)/components" -H "Authorization: Bearer $$TOKEN" \
			| grep -o '{"component_id":"[^"]*","name":"echo"' \
			| sed -n 's/.*"component_id":"\([^"]*\)".*/\1/p' | head -n1); \
	fi; \
	test -n "$$CID" || { echo "ERROR: component_id を解決できませんでした。"; exit 1; }; \
	echo "    component_id=$$CID"; \
	echo "==> POST /components/$$CID/versions (version=$(VERSION), wasm=echo.wasm)"; \
	CODE=$$(curl -sS -o /tmp/faas_deploy_body -w '%{http_code}' -X POST "$(BASE_URL)/components/$$CID/versions" \
		-H "Authorization: Bearer $$TOKEN" \
		-F "version=$(VERSION)" \
		-F "wasm=@$(COMPONENTS_DIR)/echo.wasm;type=application/wasm"); \
	echo "    [$$CODE] $$(cat /tmp/faas_deploy_body)"; \
	if [ "$$CODE" = "201" ]; then \
		echo "OK: echo $(VERSION) をアップロード・active 化しました。"; \
	elif grep -q "already exists" /tmp/faas_deploy_body; then \
		echo "OK: echo $(VERSION) は既にデプロイ済み（冪等スキップ）。"; \
	else \
		echo "ERROR: アップロード失敗 (HTTP $$CODE)"; exit 1; \
	fi

invoke: ## echo を end-to-end で実行（deploy → POST /invoke → GET /executions をポーリング）
	@$(MAKE) --no-print-directory deploy
	@set -e; \
	if ! TOKEN=$$($(MAKE) -s login); then echo "ERROR: ログインに失敗しました。"; exit 1; fi; \
	echo "==> POST /invoke"; \
	BODY=$$(curl -sS -X POST "$(BASE_URL)/invoke" \
		-H "Authorization: Bearer $$TOKEN" \
		-H "Content-Type: application/json" \
		-d '{"component":"echo","input":{"hello":"world"}}'); \
	echo "    $$BODY"; \
	EID=$$(printf '%s' "$$BODY" | sed -n 's/.*"execution_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'); \
	test -n "$$EID" || { echo "ERROR: execution_id を取得できませんでした。"; exit 1; }; \
	echo "    execution_id=$$EID"; \
	echo "==> GET /executions/$$EID をポーリング（初回は本体取得＋事前コンパイルで数秒かかる）..."; \
	for i in $$(seq 1 15); do \
		sleep 1; \
		R=$$(curl -sS "$(BASE_URL)/executions/$$EID" -H "Authorization: Bearer $$TOKEN"); \
		S=$$(printf '%s' "$$R" | sed -n 's/.*"status"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'); \
		echo "    [$$i] status=$$S"; \
		case "$$S" in succeeded|failed|timeout) echo "$$R"; break;; esac; \
	done

# --- M7: デプロイ運用（canary / rollback / secret）------------------------
# canary 運用と secret 管理には **admin スコープ**のトークンが要る（細粒度スコープは M7 非スコープ）。
# COMPONENT_ID は `make component-id` で解決するか、明示的に渡す。

component-id: ## echo の component_id を標準出力に出す（CID=$$(make -s component-id)）
	@set -e; \
	TOKEN=$$($(MAKE) -s login); \
	curl -sS "$(BASE_URL)/components" -H "Authorization: Bearer $$TOKEN" \
	  | sed -n 's/.*"component_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1

traffic: ## M7a: canary 配分を表示（GET /components/{id}/traffic）。CID=... で対象指定
	@set -e; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	curl -sS "$(BASE_URL)/components/$$CID/traffic" -H "Authorization: Bearer $$TOKEN"; echo

canary: ## M7a: canary を設定（CANARY_VERSION=0.2.0 WEIGHT=10 [CID=...]）
	@set -e; \
	test -n "$(CANARY_VERSION)" || { echo "ERROR: CANARY_VERSION=... を指定してください（例: make canary CANARY_VERSION=0.2.0 WEIGHT=10）"; exit 1; }; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	curl -sS -X PUT "$(BASE_URL)/components/$$CID/traffic" \
	  -H "Authorization: Bearer $$TOKEN" -H "Content-Type: application/json" \
	  -d "{\"canary_version\":\"$(CANARY_VERSION)\",\"weight\":$${WEIGHT:-10}}"; echo

promote: ## M7a: canary を stable へ昇格（[CANARY_VERSION=0.2.0 で CAS] [CID=...]）
	@set -e; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	BODY=$${CANARY_VERSION:+{\"version\":\"$(CANARY_VERSION)\"}}; \
	curl -sS -X POST "$(BASE_URL)/components/$$CID/promote" \
	  -H "Authorization: Bearer $$TOKEN" -H "Content-Type: application/json" \
	  -d "$${BODY:-{}}"; echo

rollback: ## M7a: ワンクリック rollback（直前 stable へ戻し canary をクリア）[CID=...]
	@set -e; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	curl -sS -X POST "$(BASE_URL)/components/$$CID/rollback" \
	  -H "Authorization: Bearer $$TOKEN" -H "Content-Type: application/json" -d '{}'; echo

approve-env: ## M7b: 注入を許可する env 名を承認（ENV_NAMES=API_KEY,LOG_LEVEL VERSION=0.1.0 [CID=...]）
	@set -e; \
	test -n "$(ENV_NAMES)" || { echo "ERROR: ENV_NAMES=A,B を指定してください"; exit 1; }; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	JSON=$$(printf '%s' "$(ENV_NAMES)" | awk -F, '{printf "["; for(i=1;i<=NF;i++){printf "%s\"%s\"", (i>1?",":""), $$i}; printf "]"}'); \
	curl -sS -X PUT "$(BASE_URL)/components/$$CID/versions/$(VERSION)/capabilities" \
	  -H "Authorization: Bearer $$TOKEN" -H "Content-Type: application/json" \
	  -d "{\"env\":$$JSON}"; echo

set-secret: ## M7c: secret を設定（NAME=API_KEY VALUE=... [CID=...]）。**値はエコーしない**
	@set -e; \
	test -n "$(NAME)" || { echo "ERROR: NAME=API_KEY を指定してください"; exit 1; }; \
	test -n "$(VALUE)" || { echo "ERROR: VALUE=... を指定してください"; exit 1; }; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	CODE=$$(curl -sS -o /dev/null -w '%{http_code}' -X PUT "$(BASE_URL)/components/$$CID/secrets/$(NAME)" \
	  -H "Authorization: Bearer $$TOKEN" -H "Content-Type: application/json" \
	  -d "{\"value\":\"$(VALUE)\"}"); \
	echo "PUT /components/$$CID/secrets/$(NAME) -> $$CODE (値は出力しません)"

secrets: ## M7c: secret のメタデータ一覧（値は返らない）[CID=...]
	@set -e; \
	TOKEN=$$($(MAKE) -s login); \
	CID=$${CID:-$$($(MAKE) -s component-id)}; \
	curl -sS "$(BASE_URL)/components/$$CID/secrets" -H "Authorization: Bearer $$TOKEN"; echo

rekey: ## M7c: 当該テナントの secret を現行 KEK で再ラップ（**侵害復旧ではない**。README 露出ガード 4）
	@set -e; \
	TOKEN=$$($(MAKE) -s login); \
	curl -sS -X POST "$(BASE_URL)/admin/secrets/rekey" -H "Authorization: Bearer $$TOKEN"; echo

deploy-chaos-components: ## M4 chaos_c/d 用: always-trap / slow をビルドしてアップロード（冪等）
	@set -e; \
	echo "==> always-trap / slow を wasm32-wasip2 でビルド..."; \
	cargo build -p always-trap -p slow --target wasm32-wasip2 --release; \
	TOKEN=$$($(MAKE) -s login); \
	resolve_cid() { \
		curl -sS -o /dev/null -X POST "$(BASE_URL)/components" \
			-H "Authorization: Bearer $$TOKEN" -H "Content-Type: application/json" \
			-d "{\"name\":\"$$1\"}"; \
		curl -sS "$(BASE_URL)/components" -H "Authorization: Bearer $$TOKEN" \
			| tr '}' '\n' | grep "\"name\":\"$$1\"" \
			| sed -n 's/.*"component_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1; \
	}; \
	echo "==> always-trap"; \
	TRAP_CID=$$(resolve_cid always-trap); \
	test -n "$$TRAP_CID" || { echo "ERROR: always-trap の component_id を解決できませんでした"; exit 1; }; \
	curl -sS -X POST "$(BASE_URL)/components/$$TRAP_CID/versions" \
		-H "Authorization: Bearer $$TOKEN" -F "version=$(VERSION)" \
		-F "wasm=@target/wasm32-wasip2/release/always_trap.wasm"; echo; \
	echo "==> slow (resource_limits は SLOW_LIMITS 変数を参照)"; \
	SLOW_CID=$$(resolve_cid slow); \
	test -n "$$SLOW_CID" || { echo "ERROR: slow の component_id を解決できませんでした"; exit 1; }; \
	curl -sS -X POST "$(BASE_URL)/components/$$SLOW_CID/versions" \
		-H "Authorization: Bearer $$TOKEN" -F "version=$(VERSION)" \
		-F 'resource_limits=$(SLOW_LIMITS)' \
		-F "wasm=@target/wasm32-wasip2/release/slow.wasm"; echo; \
	echo "OK: chaos 用 component をデプロイしました（CHAOS_ALWAYS_TRAP=always-trap CHAOS_SLOW=slow）。"

recreate-stream: ## M8-1: invoke stream を WorkQueue retention で作り直す（CP/worker を止めてから実行）
	@echo "==> control-plane / worker が停止していることを確認してください（未消化 0 は本コマンドが検査します）"
	@NATS_URL="$(NATS_URL)" cargo run --quiet -p faas-control-plane -- --recreate-invoke-stream $(FORCE_ARG)
	@echo "OK: stream を再作成しました。control-plane → worker の順に起動してください。"

rls-lint: ## M3b: テナント分離の静的ガード（SET app.tenant_id ハザード / 生 pool 渡し検出）
	@./scripts/rls-lint.sh

logs: ## docker compose のログを追従
	docker compose logs -f

psql: ## postgres に psql で接続
	docker compose exec postgres psql -U faas -d faas

clean: ## docker compose を停止し volume も削除（DB/JetStream/MinIO を初期化）
	docker compose down -v
