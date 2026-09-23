use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "usage_rollups")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub period_start: Date,
    #[sea_orm(primary_key, auto_increment = false)]
    pub component_id: String,
    pub invocation_count: i64,
    pub cpu_fuel_used: i64,
    pub wall_time_ms: i64,
    pub peak_memory_bytes_max: i64,
    pub output_bytes: i64,
    pub succeeded_count: i64,
    pub failed_count: i64,
    pub timeout_count: i64,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
