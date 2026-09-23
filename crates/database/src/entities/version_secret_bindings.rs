use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "version_secret_bindings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: String,
    pub component_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub version_id: String,
    pub secret_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
