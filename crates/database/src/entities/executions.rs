use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "executions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub tenant_id: String,
    pub component_id: String,
    pub version_id: String,
    pub status: String,
    pub input: Option<Json>,
    pub output: Option<Json>,
    pub error: Option<Json>,
    pub application_logs: Option<Json>,
    pub created_at: DateTimeUtc,
    pub started_at: Option<DateTimeUtc>,
    pub finished_at: Option<DateTimeUtc>,
    pub job_token_kid: Option<String>,
    pub input_ref: Option<String>,
    pub output_ref: Option<String>,
    pub cpu_fuel_used: Option<i64>,
    pub wall_time_ms: Option<i64>,
    pub peak_memory_bytes: Option<i64>,
    pub output_bytes: Option<i64>,
    pub invocation_count: Option<i32>,
    pub http_request: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
