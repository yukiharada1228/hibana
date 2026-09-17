use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "component_versions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub component_id: String,
    pub tenant_id: String,
    pub version: String,
    pub storage_uri: String,
    pub wasm_sha256: String,
    pub resource_limits: Json,
    pub status: String,
    pub created_at: DateTimeUtc,
    pub size_bytes: i64,
    pub capabilities: Json,
    pub build_metadata: Option<Json>,
    pub deleted_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
