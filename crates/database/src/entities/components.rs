use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "components")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub active_version_id: Option<String>,
    pub created_at: DateTimeUtc,
    pub deleted_at: Option<DateTimeUtc>,
    pub previous_active_version_id: Option<String>,
    pub ingress_enabled: bool,
    pub egress_policy: Option<Json>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
