mod admission;
mod auth;
mod authz;
mod backup;
mod completion;
mod config;
mod crypto;
mod db;
mod deployment;
mod direct_http;
mod dispatch;
mod error;
mod extract;
mod handlers;
mod handlers_secrets;
#[cfg(test)]
mod http_tests;
mod ingress;
mod job_auth;
mod login;
mod maintenance;
mod metrics;
mod preparation;
mod reaper;
mod secrets;
mod signing;
mod state;
mod storage;
mod store;
mod validation;

mod bootstrap;
mod migrations;
mod routes;

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--verify-backup-secrets") {
        return backup::run();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(bootstrap::run())
}
