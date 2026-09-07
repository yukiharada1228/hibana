mod admission;
mod auth;
mod authz;
mod completion;
mod config;
mod crypto;
mod db;
mod deployment;
mod direct_http;
mod error;
mod extract;
mod handlers;
mod handlers_secrets;
#[cfg(test)]
mod http_tests;
mod ingress;
mod job_auth;
mod login;
mod metrics;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    bootstrap::run().await
}
