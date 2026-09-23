//! Signing keys persistence.
use hibana_database::prelude::*;
use sea_orm::DerivePartialModel;

// ---------------------------------------------------------------------------
// M9a: Component 署名鍵（component_signing_keys）
// ---------------------------------------------------------------------------

/// テナントが登録した署名鍵の 1 行。
#[derive(Debug, Clone, serde::Serialize, DerivePartialModel)]
#[sea_orm(entity = "component_signing_keys::Entity")]
pub struct SigningKey {
    pub key_id: String,
    /// Ed25519 公開鍵（base64url, パディング無し）。
    pub public_key: String,
    /// 'active' | 'retired'。
    pub status: String,
}

/// テナントの署名鍵を全件返す（active + retired）。検証は全鍵を試すので status で絞らない。
pub async fn list_signing_keys(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<Vec<SigningKey>, DbErr> {
    component_signing_keys::Entity::find()
        .filter(component_signing_keys::Column::TenantId.eq(tenant_id))
        .order_by_asc(component_signing_keys::Column::CreatedAt)
        .order_by_asc(component_signing_keys::Column::KeyId)
        .lock_shared()
        .into_partial_model::<SigningKey>()
        .all(executor)
        .await
}

/// 署名鍵を登録する（同一 key_id は公開鍵 / status を上書き = 冪等な再登録）。
pub async fn upsert_signing_key(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    key_id: &str,
    public_key: &str,
    created_by: Option<&str>,
) -> Result<(), DbErr> {
    component_signing_keys::Entity::insert(component_signing_keys::ActiveModel {
        tenant_id: Set(tenant_id.into()),
        key_id: Set(key_id.into()),
        public_key: Set(public_key.into()),
        status: Set("active".into()),
        created_by: Set(created_by.map(str::to_owned)),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            component_signing_keys::Column::TenantId,
            component_signing_keys::Column::KeyId,
        ])
        .update_columns([
            component_signing_keys::Column::PublicKey,
            component_signing_keys::Column::Status,
        ])
        .to_owned(),
    )
    .exec_without_returning(executor)
    .await?;
    Ok(())
}

/// 鍵を retire する（検証は通すが新規署名の推奨から外す）。0 行 = 不在 → 呼び出し側が 404。
pub async fn retire_signing_key(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    key_id: &str,
) -> Result<bool, DbErr> {
    Ok(component_signing_keys::Entity::update_many()
        .col_expr(component_signing_keys::Column::Status, Expr::val("retired"))
        .filter(component_signing_keys::Column::TenantId.eq(tenant_id))
        .filter(component_signing_keys::Column::KeyId.eq(key_id))
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

/// テナントが「署名必須」ポリシーかどうか（`tenants.require_signed_components`）。
pub async fn require_signed_components(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
) -> Result<bool, DbErr> {
    Ok(tenants::Entity::find_by_id(tenant_id)
        .select_only()
        .column(tenants::Column::RequireSignedComponents)
        .into_tuple::<bool>()
        .one(executor)
        .await?
        .unwrap_or(true))
}

/// 署名必須ポリシーを設定する。0 行 = テナント不在 → 呼び出し側が 404。
pub async fn set_require_signed_components(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    require: bool,
) -> Result<bool, DbErr> {
    Ok(tenants::Entity::update_many()
        .col_expr(tenants::Column::RequireSignedComponents, Expr::val(require))
        .filter(tenants::Column::Id.eq(tenant_id))
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}
