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
    // Validation is synchronous and memory bounded. Do not allocate Tokio worker
    // threads or initialize telemetry before applying the child's address-space limit.
    if std::env::args().any(|a| a == validation::VALIDATE_STDIN_FLAG) {
        return validation::run_validate_stdin();
    }
    if std::env::args().any(|a| a == "--verify-backup-secrets") {
        return backup::run();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(bootstrap::run())
}
