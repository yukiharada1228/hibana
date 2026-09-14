//! Management and internal HTTP routing; authentication stays at the route boundary.
use crate::ingress;
use crate::{
    auth, auth::require_scope, direct_http, handlers, handlers_secrets, login, state::AppState,
};
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use hibana_shared::Scope;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
pub(crate) fn build_internal_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/internal/maintenance",
            get(crate::maintenance::status).put(crate::maintenance::set),
        )
        .route(
            "/internal/maintenance/prepare",
            post(crate::maintenance::prepare),
        )
        .route("/internal/direct-job", post(direct_http::redeem))
        .route("/internal/artifact", post(crate::preparation::redeem))
        .route("/internal/direct-result", post(direct_http::complete))
        .route("/internal/job-env", post(handlers_secrets::job_env))
        .with_state(state)
}

/// ルータを組み立てる。
///
/// レイヤリング:
/// - スコープ要件はルート群ごとに `route_layer(require_scope(..))` で被せる
///   （GET=>Read / POST /invoke=>Invoke / component・version 作成=>Deploy /
///   全 DELETE・active-version 切替・user/token/tenant 管理=>Admin）。
/// - `authenticate` は /healthz・/auth/login・POST /admin/tenants を**除く**
///   全ルートに適用し、`Principal` を確立する。
///
/// メソッド単位でスコープが異なる（例 /components の POST=Deploy, GET=Read）ため、
/// スコープ別の小ルータに分割して各々へ route_layer を被せてから merge する。
pub(crate) fn build_router(state: AppState) -> Router {
    // --- Read スコープ（一覧・取得） ---
    let read_routes = Router::new()
        .route("/components", get(handlers::components::list_components))
        .route(
            "/components/{component_id}/versions",
            get(handlers::components::list_versions),
        )
        .route("/executions/{id}", get(handlers::executions::get_execution))
        // GET /components/{id}/versions/{version}/capabilities: 現在の承認 env 名 / egress 先を返す
        // （M11-9。値は返さない。CLI が全置換 PUT 前にマージするための読み取り）。
        .route(
            "/components/{component_id}/versions/{version}/capabilities",
            get(handlers::capabilities::get_capabilities),
        )
        // GET /usage: テナント利用量参照 (M5, §15 / §6.0)。principal.tenant_id を権威化し
        // cross-tenant path を持たない（/tenants/{id}/usage の IDOR 面を作らない）。
        .route("/usage", get(handlers::usage::get_usage))
        .route(
            "/components/{component_id}/secrets",
            get(handlers_secrets::list_secrets),
        )
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Read)));

    // --- Deploy スコープ（component / version の作成） ---
    let deploy_routes = Router::new()
        .route("/components", post(handlers::components::create_component))
        .route(
            "/components/{component_id}/versions",
            // axum の DefaultBodyLimit（既定 2MiB）は multipart body 全体に効くため、
            // それを超える wasm（JS/Hono コンポーネントは数 MiB〜十数 MiB）は
            // ハンドラのストリーミング検査に届く前に弾かれてしまう。上限を
            // MAX_WASM_UPLOAD_BYTES（+ 他フィールド用の余白 1MiB）に引き上げる。
            // ハード上限の強制自体は upload_version 内のストリーミング検査が担う。
            post(handlers::components::upload_version).layer(axum::extract::DefaultBodyLimit::max(
                state.max_wasm_upload_bytes() as usize + 1024 * 1024,
            )),
        )
        // PUT /components/{id}/ingress: 公開 HTTP ingress の opt-in 切り替え (M11, §4.2。
        // component ライフサイクル相当の Deploy スコープ)。
        .route(
            "/components/{component_id}/ingress",
            put(handlers::components::set_component_ingress),
        )
        // --- M7b: per-function 環境変数（平文 config, §15 / §4.4）---
        // 読み書きとも Deploy。GET を Read に置かないのは、config が平文で secret と同じ env
        // 名前空間に混ざるため（資格情報を誤って config へ入れた瞬間、最も広く配られる read
        // スコープが資格情報の読み取り権限になる）。
        .route(
            "/components/{component_id}/config",
            get(handlers::configuration::get_function_config),
        )
        .route(
            "/components/{component_id}/config",
            put(handlers::configuration::put_function_config),
        )
        .route(
            "/components/{component_id}/config/{key}",
            delete(handlers::configuration::delete_function_config),
        )
        .route(
            "/components/{component_id}/rollback",
            post(handlers::components::rollback_version),
        )
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Deploy)));

    // --- Admin スコープ（全 DELETE・active-version 切替・user/token 管理） ---
    let admin_routes = Router::new()
        .route(
            "/components/{component_id}/secrets/{name}/deploy-access",
            put(handlers_secrets::set_secret_deploy_access),
        )
        .route(
            "/components/{component_id}",
            delete(handlers::components::delete_component),
        )
        .route(
            "/components/{component_id}/versions/{version}",
            delete(handlers::components::delete_version),
        )
        .route(
            "/components/{component_id}/active-version",
            put(handlers::components::set_active_version),
        )
        .route(
            "/tenants/{tenant_id}/users",
            post(handlers::identity::create_user),
        )
        .route("/tokens", post(handlers::identity::create_token))
        .route(
            "/tokens/{token_id}",
            delete(handlers::identity::revoke_token),
        )
        // Legacy env mutation returns 409: bindings belong to immutable versions.
        .route(
            "/components/{component_id}/versions/{version}/capabilities",
            put(handlers::capabilities::approve_capability_env),
        )
        // --- M9c: capability の egress allowlist 承認 (§4.4 / §15 M9) ---
        // PUT /components/{id}/versions/{version}/capabilities/egress:
        // 許可する outbound 先（host:port）を承認する。Secret利用許可と同じく admin 専用経路
        // （deploy トークンが自分で外部到達を承認できてはならない）。
        .route(
            "/components/{component_id}/versions/{version}/capabilities/egress",
            put(handlers::capabilities::approve_capability_egress),
        )
        // --- M9a: Component 署名鍵の管理 + 署名必須ポリシー (§6.2 / §15 M9) ---
        // 供給網検証: deploy トークンが漏れても、テナント登録鍵で署名された wasm でなければ
        // active にできない。鍵管理とポリシーは admin 専用（deploy から分離）。
        .route(
            "/admin/signing-keys",
            get(handlers::signing_keys::list_signing_keys),
        )
        .route(
            "/admin/signing-keys/{key_id}",
            put(handlers::signing_keys::register_signing_key)
                .delete(handlers::signing_keys::retire_signing_key),
        )
        .route(
            "/admin/signing-policy",
            put(handlers::signing_keys::set_signing_policy),
        )
        // --- M7c: Secrets Manager (§10 / §15) ---
        // 書き込み系はすべて admin スコープ + require_admin_role の二重ガード
        // （§4.4「付与（承認）は admin スコープを要する (MUST)」に従う）。
        .route(
            "/components/{component_id}/secrets/{name}",
            put(handlers_secrets::put_secret),
        )
        .route(
            "/components/{component_id}/secrets/{name}/rotate",
            post(handlers_secrets::rotate_secret),
        )
        .route(
            "/components/{component_id}/secrets/{name}",
            delete(handlers_secrets::delete_secret),
        )
        // GET /secrets/keys: 運用向け（kek_kid / value_len はここだけ）。
        .route(
            "/components/{component_id}/secrets/keys",
            get(handlers_secrets::list_secret_keys),
        )
        // POST /admin/secrets/rekey: **当該テナントのみ**を現行 KEK で再ラップする (M7c-4)。
        // 応答は件数のみ（kid 別の内訳は他テナントの総数が漏れるので返さない）。
        .route(
            "/admin/secrets/rekey",
            post(handlers_secrets::rekey_secrets),
        )
        .route_layer(axum::middleware::from_fn(require_scope(Scope::Admin)));

    // 認証必須ルート（スコープ別ルータを統合し、authenticate で principal を確立）。
    let protected = read_routes
        .merge(deploy_routes)
        .merge(admin_routes)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ));

    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/readyz", get(handlers::health::readyz))
        .route("/metrics", get(handlers::health::metrics))
        .route("/auth/login", post(login::login))
        .route("/admin/tenants", post(handlers::tenants::create_tenant))
        .route(
            "/admin/components",
            get(handlers::components::admin_list_components),
        )
        .route(
            "/admin/tenants/{tenant_id}/components/{component_id}",
            delete(handlers::components::admin_delete_component),
        )
        // M10 follow-up: テナント status / quotas の platform 管理（bootstrap トークン gate。
        // create_tenant と同じ**非認証グループ**に置き、ハンドラ内で bootstrap トークンを照合する。
        // テナント admin スコープではない —— テナント自身が自分を再有効化 / 増枠できてはならない）。
        .route(
            "/admin/tenants/{tenant_id}/status",
            put(handlers::tenants::set_tenant_status),
        )
        .route(
            "/admin/tenants/{tenant_id}/quotas",
            put(handlers::tenants::set_tenant_quotas),
        )
        .merge(protected)
        // M11 (§4.2): 公開 HTTP ingress gateway。API ルートにマッチしなかったリクエストのうち
        // Host が `<app>.<tenant>.<INGRESS_BASE_DOMAIN>` のものだけを gateway として処理する
        // （それ以外は 404）。deny-by-default（ingress_enabled な component だけ到達可能）。
        .fallback(
            |state: axum::extract::State<AppState>, req: axum::extract::Request| async move {
                if std::env::var("APP_BIND_ADDR").is_ok() {
                    axum::response::IntoResponse::into_response(axum::http::StatusCode::NOT_FOUND)
                } else {
                    ingress::ingress_fallback(state, req).await
                }
            },
        )
        // M10 follow-up (§3.8): HTTP リクエストメトリクスを observe する。TraceLayer より内側に
        // 置くことで、routing 済み（MatchedPath が extensions に載った状態）で method/route/status を
        // 拾える。**path はルートテンプレート**（`/components/{id}/versions`）を使い、生 URI の
        // ID でカーディナリティを爆発させない。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            http_metrics_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::maintenance::public_gate,
        ))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// HTTP リクエストの件数（`faas_http_requests_total`）と処理時間
/// （`faas_http_request_duration_seconds`）を observe する middleware（M4a のメトリクスを配線）。
///
/// カーディナリティ対策として `path` は **MatchedPath**（ルートテンプレート）を使う。ルートに
/// マッチしなかった（404 等）リクエストは 1 つの `<unmatched>` に畳んで、任意 URI による
/// 系列の無限増殖を防ぐ。
async fn http_metrics_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().as_str().to_owned();
    let path = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_owned());
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    state
        .metrics()
        .observe_http(&method, &path, resp.status().as_u16(), start.elapsed());
    resp
}
