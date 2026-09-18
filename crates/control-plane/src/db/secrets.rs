//! Secrets persistence.
use hibana_database::prelude::*;

// ---------------------------------------------------------------------------
// M7c: Secrets Manager（function_secrets / function_secret_versions）
//
// **平文も暗号文もこの層より上へ素で流さない**。値は `secrets.rs` の封筒 [`crate::secrets::Envelope`]
// としてのみ受け渡す。版台帳は追記専用（faas_app に UPDATE/DELETE が無い）なので、値の更新
// （rotate）も KEK 再ラップ（rekey）も「新しい version 行の INSERT」で表現する。
// すべて set_tenant_guc 済み tx で呼ぶこと。
// ---------------------------------------------------------------------------

/// `function_secrets` のメタデータ 1 行（**値は含まない**）。
#[derive(Debug, Clone, FromQueryResult)]
pub struct SecretMetaRow {
    pub id: String,
    pub component_id: String,
    pub name: String,
    pub current_version: i32,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// secret メタ行を作成する（版行は別途 `insert_secret_version` で INSERT する）。
///
/// 生存行の同名重複は部分 UNIQUE index 違反（23505）。呼び出し側が捕捉して
/// 「既存 → rotate」へ倒す。
pub async fn insert_secret_meta(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    secret_id: &str,
    component_id: &str,
    name: &str,
    current_version: i32,
) -> Result<(), DbErr> {
    function_secrets::Entity::insert(function_secrets::ActiveModel {
        id: Set(secret_id.into()),
        tenant_id: Set(tenant_id.into()),
        component_id: Set(component_id.into()),
        name: Set(name.into()),
        current_version: Set(current_version),
        ..Default::default()
    })
    .exec(executor)
    .await?;
    Ok(())
}

/// 生存している secret を名前で引く（**メタのみ**）。
pub async fn find_live_secret_by_name(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    name: &str,
) -> Result<Option<SecretMetaRow>, DbErr> {
    function_secrets::Entity::find()
        .select_only()
        .columns([
            function_secrets::Column::Id,
            function_secrets::Column::ComponentId,
            function_secrets::Column::Name,
            function_secrets::Column::CurrentVersion,
            function_secrets::Column::UpdatedAt,
        ])
        .filter(function_secrets::Column::TenantId.eq(tenant_id))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .filter(function_secrets::Column::ComponentId.eq(component_id))
        .filter(function_secrets::Column::Name.eq(name))
        .into_model::<SecretMetaRow>()
        .one(executor)
        .await
}

/// component の生存 secret を全件引く（**メタのみ**。値も value_len も kek_kid も返さない）。
pub async fn list_secrets_meta(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
) -> Result<Vec<SecretMetaRow>, DbErr> {
    function_secrets::Entity::find()
        .select_only()
        .columns([
            function_secrets::Column::Id,
            function_secrets::Column::ComponentId,
            function_secrets::Column::Name,
            function_secrets::Column::CurrentVersion,
            function_secrets::Column::UpdatedAt,
        ])
        .filter(function_secrets::Column::TenantId.eq(tenant_id))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .filter(function_secrets::Column::ComponentId.eq(component_id))
        .order_by_asc(function_secrets::Column::Name)
        .into_model::<SecretMetaRow>()
        .all(executor)
        .await
}

/// 版台帳へ 1 行 INSERT する（追記専用）。`reason` は `'create' | 'rotate' | 'rekey'`。
/// 呼び出し側は component の排他ロックを取得済みであること。
pub async fn insert_secret_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
    env: &crate::secrets::Envelope,
    reason: &str,
    created_by: Option<&str>,
) -> Result<(), DbErr> {
    // now() is the transaction start, possibly before an earlier HTTP admission
    // released the parent lock. Stamp the generation after acquiring that lock
    // so delayed Worker lookups cannot select a value published after admission.
    let created_at: chrono::DateTime<chrono::Utc> = executor
        .query_one(
            &Query::select()
                .expr_as(Func::cust("clock_timestamp"), "created_at")
                .to_owned(),
        )
        .await?
        .ok_or_else(|| DbErr::Custom("database clock unavailable".into()))?
        .try_get("", "created_at")?;
    function_secret_versions::Entity::insert(function_secret_versions::ActiveModel {
        tenant_id: Set(tenant_id.into()),
        secret_id: Set(secret_id.into()),
        version: Set(version),
        kek_kid: Set(env.kek_kid.clone()),
        wrapped_dek: Set(env.wrapped_dek.clone()),
        dek_nonce: Set(env.dek_nonce.clone()),
        nonce: Set(env.nonce.clone()),
        ciphertext: Set(env.ciphertext.clone()),
        value_len: Set(env.value_len),
        reason: Set(reason.into()),
        created_by: Set(created_by.map(str::to_owned)),
        created_at: Set(created_at),
    })
    .exec(executor)
    .await?;
    Ok(())
}

/// `current_version` を前進させる（rotate / rekey 後の切替）。
pub async fn bump_secret_current_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
) -> Result<bool, DbErr> {
    Ok(function_secrets::Entity::update_many()
        .col_expr(function_secrets::Column::CurrentVersion, Expr::val(version))
        .col_expr(function_secrets::Column::UpdatedAt, now())
        .filter(function_secrets::Column::TenantId.eq(tenant_id))
        .filter(function_secrets::Column::Id.eq(secret_id))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

/// secret を soft delete する。版台帳は残る（追記専用なので消せない ＝ 監査上も残す）。
///
/// 生存行の部分 UNIQUE index から外れるため、**同名で作り直せる**（インシデント対応の基本操作）。
pub async fn soft_delete_secret(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    secret_id: &str,
) -> Result<bool, DbErr> {
    Ok(function_secrets::Entity::update_many()
        .col_expr(function_secrets::Column::DeletedAt, now())
        .col_expr(function_secrets::Column::UpdatedAt, now())
        .filter(function_secrets::Column::TenantId.eq(tenant_id))
        .filter(function_secrets::Column::Id.eq(secret_id))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .exec(executor)
        .await?
        .rows_affected
        > 0)
}

pub async fn version_has_live_secrets(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    component_id: &str,
    version_id: &str,
) -> Result<bool, DbErr> {
    Ok(function_secrets::Entity::find()
        .filter(function_secrets::Column::TenantId.eq(tenant_id))
        .filter(function_secrets::Column::ComponentId.eq(component_id))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .filter(
            function_secrets::Column::Id.in_subquery(
                version_secret_bindings::Entity::find()
                    .select_only()
                    .column(version_secret_bindings::Column::SecretId)
                    .filter(version_secret_bindings::Column::TenantId.eq(tenant_id))
                    .filter(version_secret_bindings::Column::ComponentId.eq(component_id))
                    .filter(version_secret_bindings::Column::VersionId.eq(version_id))
                    .into_query(),
            ),
        )
        .count(executor)
        .await?
        > 0)
}

/// 現行 kid でない current 世代を持つ secret の id を引く（M7c-4: rekey 対象）。
///
/// `secrets_stale_kek(p_tenant, p_active_kid)` は SECURITY DEFINER（faas_app は FORCE RLS 下で
/// 巡回できない）。**テナント引数を取る版**なので GUC 前でも呼べるが、返すのは
/// `(tenant_id, secret_id)` だけで暗号文も名前も返さない（認証前参照の3関数と同じ作法）。
pub async fn secrets_stale_kek(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    active_kid: &str,
) -> Result<Vec<String>, DbErr> {
    function_rows(
        executor,
        "secrets_stale_kek",
        vec![tenant_id.into(), active_kid.into()],
    )
    .await?
    .into_iter()
    .map(|r| r.try_get("", "secret_id"))
    .collect()
}

/// kid 別の必要な世代数。現在世代と未完了の実行が参照する世代を重複なく集計する。
///
/// **HTTP 応答に載せてはならない** (MUST NOT)。全テナント横断の集計であり、テナント管理者へ
/// 返すと他テナントの secret 総数が漏れる。
pub async fn secrets_kek_kid_counts_all(
    executor: &impl ConnectionTrait,
) -> Result<Vec<(String, i64)>, DbErr> {
    function_rows(executor, "secrets_kek_kid_counts_all", vec![])
        .await?
        .into_iter()
        .map(|r| Ok((r.try_get("", "kek_kid")?, r.try_get("", "n")?)))
        .collect()
}

/// secret メタ行を id で引く（rekey が current 世代を解決するのに使う）。
pub async fn find_secret_meta_by_id(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    secret_id: &str,
) -> Result<Option<SecretMetaRow>, DbErr> {
    function_secrets::Entity::find()
        .select_only()
        .columns([
            function_secrets::Column::Id,
            function_secrets::Column::ComponentId,
            function_secrets::Column::Name,
            function_secrets::Column::CurrentVersion,
            function_secrets::Column::UpdatedAt,
        ])
        .filter(function_secrets::Column::TenantId.eq(tenant_id))
        .filter(function_secrets::Column::DeletedAt.is_null())
        .filter(function_secrets::Column::Id.eq(secret_id))
        .into_model::<SecretMetaRow>()
        .one(executor)
        .await
}

/// 指定 secret の指定版の封筒を引く。
pub async fn find_secret_version(
    executor: &impl ConnectionTrait,
    tenant_id: &str,
    secret_id: &str,
    version: i32,
) -> Result<Option<crate::secrets::Envelope>, DbErr> {
    Ok(function_secret_versions::Entity::find_by_id((
        tenant_id.to_owned(),
        secret_id.to_owned(),
        version,
    ))
    .one(executor)
    .await?
    .map(|r| crate::secrets::Envelope {
        kek_kid: r.kek_kid,
        wrapped_dek: r.wrapped_dek,
        dek_nonce: r.dek_nonce,
        nonce: r.nonce,
        ciphertext: r.ciphertext,
        value_len: r.value_len,
    }))
}
