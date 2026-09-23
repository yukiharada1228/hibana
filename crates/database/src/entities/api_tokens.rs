use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "api_tokens")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub name: Option<String>,
    pub created_at: DateTimeUtc,
    pub expires_at: DateTimeUtc,
    pub revoked_at: Option<DateTimeUtc>,
    pub auth_method: String,
    pub user_auth_version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
