//! In-Pod adapter for `hibana platform`: credentials stay in the CP environment.
pub(crate) async fn run(args: &[String]) -> anyhow::Result<()> {
    let action = args.first().map(String::as_str).unwrap_or("");
    if action == "check" {
        return crate::dependency_probe::run(&args[1..]).await;
    }
    let owner = args.get(1).cloned().unwrap_or_default();
    let (method, path, body) = match (action, args.len()) {
        ("close" | "open", 2) => (
            reqwest::Method::PUT,
            "/internal/maintenance",
            Some(serde_json::json!({"owner": owner, "closed": action == "close"})),
        ),
        ("status", 1) => (reqwest::Method::GET, "/internal/maintenance", None),
        ("prepare", 3) => {
            let workers: Vec<std::net::IpAddr> = args[2]
                .split(',')
                .map(str::parse)
                .collect::<Result<_, _>>()?;
            (
                reqwest::Method::POST,
                "/internal/maintenance/prepare",
                Some(serde_json::json!({"owner":owner,"workers":workers})),
            )
        }
        _ => anyhow::bail!(
            "Usage: --maintenance close|open OWNER | status | prepare OWNER WORKER_IPS"
        ),
    };
    let mut address: std::net::SocketAddr = std::env::var("INTERNAL_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8081".into())
        .parse()?;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    let token = std::env::var("BOOTSTRAP_ADMIN_TOKEN")?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()?;
    let mut request = client
        .request(method, format!("http://{address}{path}"))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(if action == "prepare" {
            245
        } else {
            15
        }));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Maintenance API transport failed"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "Maintenance API: HTTP {}",
        response.status().as_u16()
    );
    if action == "status" {
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("Invalid maintenance status"))?;
        println!("{}", serde_json::to_string(&data)?);
    }
    Ok(())
}
