use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub tenant_id: String,
    pub email: String,
    pub role: String,
    pub created_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
    pub oidc_issuer: Option<String>,
    pub oidc_subject: Option<String>,
    pub auth_version: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
