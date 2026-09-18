//! PostgreSQL-specific security operations. Application CRUD belongs to entities.
use sea_orm::{
    sea_query::{Expr, Func, Query},
    *,
};
use std::time::Duration;

pub fn now() -> sea_query::SimpleExpr {
    Func::cust("now").into()
}

pub async fn connect(url: &str, max: u32, min: u32) -> Result<DatabaseConnection, DbErr> {
    let mut options = ConnectOptions::new(url);
    options
        .max_connections(max)
        .min_connections(std::cmp::min(min, max))
        .acquire_timeout(Duration::from_secs(10))
        .sqlx_logging(false)
        .map_sqlx_postgres_opts(|opts| {
            opts.options([
                ("statement_timeout", "10000"),
                ("lock_timeout", "3000"),
                ("idle_in_transaction_session_timeout", "15000"),
                ("tcp_keepalives_idle", "10"),
                ("tcp_keepalives_interval", "3"),
                ("tcp_keepalives_count", "3"),
                ("tcp_user_timeout", "10000"),
            ])
        });
    Database::connect(options).await
}

/// The transaction type prevents accidentally setting a tenant on a pooled connection.
pub async fn set_tenant_guc(tx: &DatabaseTransaction, tenant: &str) -> Result<(), DbErr> {
    tx.query_one(
        &Query::select()
            .expr(Func::cust("set_config").args([
                Expr::val("app.tenant_id"),
                Expr::val(tenant),
                Expr::val(true),
            ]))
            .to_owned(),
    )
    .await?;
    Ok(())
}

/// Call a fixed SECURITY DEFINER function using bound arguments. Names are
/// supplied only by source code; request data is always represented as values.
pub async fn function_rows(
    db: &impl ConnectionTrait,
    name: &'static str,
    args: Vec<Value>,
) -> Result<Vec<QueryResult>, DbErr> {
    db.query_all(
        &Query::select()
            .column(sea_query::Asterisk)
            .from_function(
                Func::cust(name).args(args.into_iter().map(Expr::val)),
                "result",
            )
            .to_owned(),
    )
    .await
}

pub async fn assert_runtime_role(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let row = db
        .query_one(
            &Query::select()
                .columns(["rolname", "rolsuper", "rolbypassrls"])
                .from("pg_roles")
                .and_where(Expr::col("rolname").eq(Expr::cust("current_user")))
                .to_owned(),
        )
        .await?
        .ok_or_else(|| DbErr::Custom("runtime role missing".into()))?;
    if row.try_get::<bool>("", "rolsuper")? || row.try_get::<bool>("", "rolbypassrls")? {
        return Err(DbErr::Custom(
            "runtime DATABASE_URL must use the non-privileged faas_app role (NOBYPASSRLS)".into(),
        ));
    }
    Ok(())
}

/// Require the schema used by this version, including additive ORM migrations.
/// This check is read-only, including when automatic migrations are disabled.
pub async fn assert_runtime_schema(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let version = db
        .query_one(
            &Query::select()
                .column("version")
                .from(("public", "seaql_migrations"))
                .and_where(Expr::col("version").eq("m20260918_000006_secret_key_retention"))
                .to_owned(),
        )
        .await;
    match version {
        Ok(Some(_)) => Ok(()),
        _ => Err(DbErr::Custom("Hibana database migrations are required before starting this version. Legacy databases need a separate switch to the ORM baseline.".into())),
    }
}

pub async fn lock_tenant_admission(tx: &DatabaseTransaction, tenant: &str) -> Result<(), DbErr> {
    tx.query_one(
        &Query::select()
            .expr(
                Func::cust("pg_advisory_xact_lock").arg(
                    Func::cust("hashtextextended").args([Expr::val(tenant), Expr::val(0_i64)]),
                ),
            )
            .to_owned(),
    )
    .await?;
    Ok(())
}
