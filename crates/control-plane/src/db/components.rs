//! Components persistence.
use hibana_database::prelude::*;

use chrono::DateTime;
use chrono::Utc;
use serde_json::Value;

/// `components` の 1 行 (API 応答に必要な範囲)。
#[derive(Debug, Clone, FromQueryResult)]
pub struct ComponentRow {
    pub id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub previous_active_version_id: Option<String>,
    pub egress_policy: Option<Value>,
}

/// Component メタデータのみを作成する (§6.2 M2)。
///
/// M2: 初期 version は作らない（active_version_id は NULL のまま）。実体は
/// POST /components/{id}/versions（insert_version）で投入する。
pub async fn create_component(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    name: &str,
) -> Result<(), DbErr> {
    components::Entity::insert(components::ActiveModel {
        id: Set(component_id.into()),
        tenant_id: Set(tenant_id.into()),
        name: Set(name.into()),
        ..Default::default()
    })
    .exec(executor)
    .await?;
    Ok(())
}

/// テナント内で id から component を解決する (soft-delete 済みは除外)。
pub async fn find_component_by_id(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<Option<ComponentRow>, DbErr> {
    components::Entity::find()
        .select_only()
        .columns([
            components::Column::Id,
            components::Column::Name,
            components::Column::ActiveVersionId,
            components::Column::PreviousActiveVersionId,
            components::Column::EgressPolicy,
        ])
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Id.eq(component_id))
        .filter(components::Column::DeletedAt.is_null())
        .into_model::<ComponentRow>()
        .one(executor)
        .await
}

/// Serialize publication and deletion on the parent row. Read version state in
/// a subsequent statement, after acquiring this lock, so it sees the winner's commit.
pub async fn lock_component(
    tx: &DatabaseTransaction,
    tenant_id: &str,
    component_id: &str,
) -> Result<bool, DbErr> {
    Ok(components::Entity::find()
        .select_only()
        .column(components::Column::Id)
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Id.eq(component_id))
        .filter(components::Column::DeletedAt.is_null())
        .lock_exclusive()
        .into_tuple::<String>()
        .one(tx)
        .await?
        .is_some())
}

/// Caller holds the parent lock. Keep the Worker's per-version capability
/// document authoritative, including for rollback targets, in this transaction.
pub async fn set_component_egress(
    tx: &DatabaseTransaction,
    tenant_id: &str,
    component_id: &str,
    approved: &[String],
) -> Result<(), DbErr> {
    use sea_orm::sea_query::{extension::postgres::PgExpr, CaseStatement};

    components::Entity::update_many()
        .col_expr(
            components::Column::EgressPolicy,
            Expr::val(serde_json::json!(approved)),
        )
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Id.eq(component_id))
        .filter(components::Column::DeletedAt.is_null())
        .exec(tx)
        .await?;

    let column = component_versions::Column::Capabilities;
    let kind = || Func::cust("jsonb_typeof").arg(Expr::col(column));
    let document = CaseStatement::new()
        .case(Expr::expr(kind()).eq("object"), Expr::col(column))
        .case(
            Expr::expr(kind()).eq("array"),
            Func::cust("jsonb_build_object").args([Expr::val("imports"), Expr::col(column)]),
        )
        .finally(Expr::val(serde_json::json!({})));
    component_versions::Entity::update_many()
        .col_expr(
            column,
            Expr::expr(document).concat(Expr::val(
                serde_json::json!({"net_allow_outbound": approved}),
            )),
        )
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::ComponentId.eq(component_id))
        .filter(component_versions::Column::DeletedAt.is_null())
        .exec(tx)
        .await?;
    Ok(())
}

/// 検証通過した version を登録する (§6.2)。
///
/// 同一 (component_id, version) の重複は UNIQUE 違反（呼び出し側で 409/422 へ）。
/// `storage_uri` は Object Storage 上のオブジェクトキー。`status` は 'active' を渡す
/// （検証通過後に active 化する。pending→active の遷移は M2 では即時）。
/// `size_bytes` は検証で確定した本体サイズ (§6.2)、`capabilities` は §4.4 strict matching を
/// 通過した **承認済み import 集合**（クライアント宣言値ではない）で、DBの
/// 列に対応する。呼び出し側（handlers）は `Validated::approved_imports` を渡すこと。
#[allow(clippy::too_many_arguments)]
pub async fn insert_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
    version: &str,
    storage_uri: &str,
    wasm_sha256: &str,
    size_bytes: i64,
    capabilities: &Value,
    resource_limits: &Value,
    status: &str,
    build_metadata: Option<&Value>,
) -> Result<(), DbErr> {
    component_versions::Entity::insert(component_versions::ActiveModel {
        id: Set(version_id.into()),
        component_id: Set(component_id.into()),
        tenant_id: Set(tenant_id.into()),
        version: Set(version.into()),
        storage_uri: Set(storage_uri.into()),
        wasm_sha256: Set(wasm_sha256.into()),
        size_bytes: Set(size_bytes),
        capabilities: Set(capabilities.clone()),
        resource_limits: Set(resource_limits.clone()),
        status: Set(status.into()),
        build_metadata: Set(build_metadata.cloned()),
        ..Default::default()
    })
    .exec(executor)
    .await?;
    Ok(())
}

/// Caller holds the component lock; recheck the live version after external preparation.
pub async fn switch_active_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
) -> Result<bool, DbErr> {
    let target = component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::Id)
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::ComponentId.eq(component_id))
        .filter(component_versions::Column::Id.eq(version_id))
        .filter(component_versions::Column::DeletedAt.is_null())
        .into_query();
    Ok(components::Entity::update_many()
        .col_expr(
            components::Column::PreviousActiveVersionId,
            Expr::case(
                Condition::any()
                    .add(components::Column::ActiveVersionId.is_null())
                    .add(components::Column::ActiveVersionId.ne(version_id)),
                Expr::col(components::Column::ActiveVersionId),
            )
            .finally(Expr::col(components::Column::PreviousActiveVersionId))
            .into(),
        )
        .col_expr(components::Column::ActiveVersionId, Expr::val(version_id))
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Id.eq(component_id))
        .filter(components::Column::DeletedAt.is_null())
        .filter(Expr::exists(target))
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

/// ワンクリック rollback。成功時は `(新 active_version_id, 旧 active_version_id)` を返す。
///
/// 0 行（`None`）は component 不在 / previous が NULL / 戻り先が soft delete 済み。理由確定は
/// 呼び出し側が 0 行のときだけ再 SELECT する。
pub async fn rollback_active_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    target_version_id: Option<&str>,
) -> Result<Option<(String, Option<String>)>, DbErr> {
    let Some(current) = find_component_by_id(executor, tenant_id, component_id).await? else {
        return Ok(None);
    };
    let Some(target) = target_version_id.or(current.previous_active_version_id.as_deref()) else {
        return Ok(None);
    };
    if !switch_active_version(executor, tenant_id, component_id, target).await? {
        return Ok(None);
    }
    Ok(find_component_by_id(executor, tenant_id, component_id)
        .await?
        .and_then(|r| {
            r.active_version_id
                .map(|id| (id, r.previous_active_version_id))
        }))
}

/// M11 (§4.2): 公開 ingress gateway 用の component 解決結果。
/// gateway は「到達可否」だけ判定し、実際の起動は名前で通常 invoke に委ねる（id は不要）。
#[derive(FromQueryResult)]
pub struct IngressComponentRow {
    /// 公開 URL から到達を許すか（deny-by-default）。
    pub ingress_enabled: bool,
    /// active な版（無ければ実行不可）。
    pub active_version_id: Option<String>,
}

/// M11 (§4.2): テナント内の component を名前で引き、公開 ingress に必要な列だけ返す。
/// FORCE RLS 下なので呼び出し側は `set_tenant_guc(tenant)` 済みの tx を渡すこと。
pub async fn find_ingress_component(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    name: &str,
) -> Result<Option<IngressComponentRow>, DbErr> {
    components::Entity::find()
        .select_only()
        .columns([
            components::Column::IngressEnabled,
            components::Column::ActiveVersionId,
        ])
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Name.eq(name))
        .filter(components::Column::DeletedAt.is_null())
        .into_model::<IngressComponentRow>()
        .one(executor)
        .await
}

/// M11 (§4.2): component の公開 ingress opt-in フラグを設定する。戻り値は行が在って
/// 更新できたか（存在しない/削除済みは false）。GUC 済み tx を渡すこと（FORCE RLS）。
pub async fn set_component_ingress(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    enabled: bool,
) -> Result<bool, DbErr> {
    Ok(components::Entity::update_many()
        .col_expr(components::Column::IngressEnabled, Expr::val(enabled))
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Id.eq(component_id))
        .filter(components::Column::DeletedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

/// version_id から `ResourceLimits`（JSONB）を解決する (M5, §15)。
///
/// finalize 経路で worker 自己申告の計量を sanity clamp する上限を引くために使う（worker の
/// `get_limits` と同じ `component_versions.resource_limits` を参照する＝同一権威値で clamp する）。
/// 行不在は `None`、JSONB のパース失敗時は `ResourceLimits::default()` にフォールバックする
/// （壁時計上限の解決と同じ `unwrap_or_default` 規約）。
pub async fn find_version_resource_limits(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    version_id: &str,
) -> Result<Option<hibana_shared::ResourceLimits>, DbErr> {
    Ok(component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::ResourceLimits)
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::Id.eq(version_id))
        .into_tuple::<Value>()
        .one(executor)
        .await?
        .map(|value| serde_json::from_value(value).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// Component / Version 管理 (§6.7)
// ---------------------------------------------------------------------------

/// `GET /components` 応答用の 1 行。
#[derive(Debug, Clone, FromQueryResult)]
pub struct ComponentListItem {
    pub component_id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub ingress_enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// テナントの Component 一覧 (soft-delete 済みは除外, §6.7)。新しい順。
pub async fn list_components(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<Vec<ComponentListItem>, DbErr> {
    components::Entity::find()
        .select_only()
        .column_as(components::Column::Id, "component_id")
        .columns([
            components::Column::Name,
            components::Column::ActiveVersionId,
            components::Column::IngressEnabled,
            components::Column::CreatedAt,
        ])
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::DeletedAt.is_null())
        .order_by_desc(components::Column::CreatedAt)
        .into_model::<ComponentListItem>()
        .all(executor)
        .await
}

/// `GET /components/{id}/versions` 応答用の 1 行。
#[derive(Debug, Clone, FromQueryResult)]
pub struct VersionListItem {
    pub version_id: String,
    pub version: String,
    pub status: String,
    pub size_bytes: i64,
    pub wasm_sha256: String,
    pub created_at: DateTime<Utc>,
}

/// 当該 component の version 一覧 (soft-delete 済みは除外, §6.7)。新しい順。
pub async fn list_versions(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<VersionListItem>, DbErr> {
    component_versions::Entity::find()
        .select_only()
        .column_as(component_versions::Column::Id, "version_id")
        .columns([
            component_versions::Column::Version,
            component_versions::Column::Status,
            component_versions::Column::SizeBytes,
            component_versions::Column::WasmSha256,
            component_versions::Column::CreatedAt,
        ])
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::ComponentId.eq(component_id))
        .filter(component_versions::Column::DeletedAt.is_null())
        .order_by_desc(component_versions::Column::CreatedAt)
        .into_model::<VersionListItem>()
        .all(executor)
        .await
}

#[derive(Debug, FromQueryResult)]
pub struct VersionDetails {
    pub version_id: String,
    pub wasm_sha256: String,
    pub build_metadata: Option<Value>,
    pub capabilities: Value,
}

/// Explicit selectors keep legacy names distinct from immutable version IDs.
pub enum VersionRef<'a> {
    Name(&'a str),
    Id(&'a str),
}

impl VersionRef<'_> {
    fn condition(self) -> sea_orm::sea_query::SimpleExpr {
        match self {
            Self::Name(value) => component_versions::Column::Version.eq(value),
            Self::Id(value) => component_versions::Column::Id.eq(value),
        }
    }
}

pub async fn version_details(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version: VersionRef<'_>,
) -> Result<Option<VersionDetails>, DbErr> {
    component_versions::Entity::find()
        .select_only()
        .column_as(component_versions::Column::Id, "version_id")
        .columns([
            component_versions::Column::WasmSha256,
            component_versions::Column::BuildMetadata,
            component_versions::Column::Capabilities,
        ])
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::ComponentId.eq(component_id))
        .filter(version.condition())
        .filter(component_versions::Column::DeletedAt.is_null())
        .into_model::<VersionDetails>()
        .one(executor)
        .await
}

/// component を soft delete する (deleted_at=now(), §6.7)。
///
/// 既に削除済み / 不在の場合は更新 0 行。呼び出し側は事前に存在確認・参照保護を行う。
pub async fn soft_delete_component(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<u64, DbErr> {
    Ok(components::Entity::update_many()
        .col_expr(components::Column::DeletedAt, now())
        .filter(components::Column::TenantId.eq(tenant_id))
        .filter(components::Column::Id.eq(component_id))
        .filter(components::Column::DeletedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected)
}

/// 当該 component の (version 文字列) から未削除の version の `id` (`ver_*`) を解決する (§6.7)。
///
/// soft delete / active-version 切替の対象確認に使う。呼び出し側が必要とするのは
/// version_id のみのため、行全体ではなく id を返す。
pub async fn find_version_id(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version: &str,
) -> Result<Option<String>, DbErr> {
    resolve_version_id(executor, tenant_id, component_id, VersionRef::Name(version)).await
}

pub async fn resolve_version_id(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version: VersionRef<'_>,
) -> Result<Option<String>, DbErr> {
    component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::Id)
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::ComponentId.eq(component_id))
        .filter(version.condition())
        .filter(component_versions::Column::DeletedAt.is_null())
        .into_tuple::<String>()
        .one(executor)
        .await
}

/// version の `capabilities` JSONB を引く（M7b: env 許可リストの解決）。
pub async fn version_capabilities(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    version_id: &str,
) -> Result<Option<Value>, DbErr> {
    component_versions::Entity::find()
        .select_only()
        .column(component_versions::Column::Capabilities)
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::Id.eq(version_id))
        .filter(component_versions::Column::DeletedAt.is_null())
        .into_tuple::<Value>()
        .one(executor)
        .await
}

/// Replace a version's capability document when an administrator changes egress.
/// The caller preserves imports and the environment fixed at deployment.
/// Returns false if the version does not exist or has been deleted.
pub async fn set_version_capabilities(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    version_id: &str,
    capabilities: &Value,
) -> Result<bool, DbErr> {
    Ok(component_versions::Entity::update_many()
        .col_expr(
            component_versions::Column::Capabilities,
            Expr::val(capabilities.clone()),
        )
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::Id.eq(version_id))
        .filter(component_versions::Column::DeletedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

/// version を soft delete する (deleted_at=now(), §6.7)。
///
/// 既に削除済み / 不在の場合は更新 0 行。active version 保護・参照保護は呼び出し側で行う。
pub async fn soft_delete_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
) -> Result<u64, DbErr> {
    Ok(component_versions::Entity::update_many()
        .col_expr(component_versions::Column::DeletedAt, now())
        .filter(component_versions::Column::TenantId.eq(tenant_id))
        .filter(component_versions::Column::Id.eq(version_id))
        .filter(component_versions::Column::ComponentId.eq(component_id))
        .filter(component_versions::Column::DeletedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected)
}
