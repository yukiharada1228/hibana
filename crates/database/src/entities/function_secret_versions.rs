use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "function_secret_versions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub secret_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: i32,
    pub kek_kid: String,
    pub wrapped_dek: Vec<u8>,
    pub dek_nonce: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub value_len: i32,
    pub reason: String,
    pub created_at: DateTimeUtc,
    pub created_by: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
