//! Deployment controls shared by normal startup and maintenance commands.

pub fn run_migrations_on_start() -> anyhow::Result<bool> {
    bool_setting("RUN_MIGRATIONS", true)
}

fn bool_setting(name: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(name) {
        Ok(value) => {
            parse_bool(&value).ok_or_else(|| anyhow::anyhow!("{name} must be true or false"))
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(_) => anyhow::bail!("{name} must be valid UTF-8"),
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// Both HTTP listeners drain on SIGTERM (Kubernetes) and Ctrl-C (local dev).
pub async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = terminate.recv() => {},
        _ = tokio::signal::ctrl_c() => {},
    }
    tracing::info!("shutdown requested; draining HTTP connections");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_booleans_reject_typos() {
        assert_eq!(parse_bool(" FALSE "), Some(false));
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("flase"), None);
        assert_eq!(parse_bool(""), None);
    }
}
