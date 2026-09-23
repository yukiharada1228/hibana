//! ORM models and PostgreSQL connection boundaries for Hibana's platform data.
//! Wasm guests never receive this connection or its credentials.
pub mod entities;
pub mod postgres;
pub mod queries;
pub use sea_orm;

pub mod prelude {
    pub use crate::entities::*;
    pub use crate::postgres::{function_rows, now, set_tenant_guc};
    pub use sea_orm::sea_query::{Expr, ExprTrait, Func, IntoIden, OnConflict, Query};
    pub use sea_orm::{
        ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection,
        DatabaseTransaction, DbErr, EntityTrait, FromQueryResult, IntoActiveModel, JoinType,
        ModelTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Set,
        TransactionSession, TransactionTrait,
    };
}
