mod controller;
mod crd;
mod manifests;
mod policy;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use kube::{Client, CustomResourceExt};
use tracing_subscriber::EnvFilter;

fn crds_yaml() -> anyhow::Result<String> {
    let mut out = String::new();
    for crd in [crd::FastMock::crd(), crd::MockApi::crd()] {
        out.push_str("---\n");
        out.push_str(&serde_yaml_ng::to_string(&crd)?);
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("crd") {
        print!("{}", crds_yaml()?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,kube=warn,hyper=warn")),
        )
        .init();
    let prom = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;
    let client = Client::try_default().await?;
    let ctx = Arc::new(controller::Ctx {
        client,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        admin_url_template: std::env::var("FASTMOCK_OPERATOR_ADMIN_URL_TEMPLATE")
            .unwrap_or_else(|_| "http://{name}-admin.{namespace}.svc:9090".into()),
        images: policy::ImagePolicy::new(
            &std::env::var("FASTMOCK_OPERATOR_SERVER_IMAGE")
                .unwrap_or_else(|_| concat!("fastmock:", env!("CARGO_PKG_VERSION")).into()),
            &std::env::var("FASTMOCK_OPERATOR_ALLOWED_IMAGE_PREFIXES").unwrap_or_default(),
        ),
        failures: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let namespace = std::env::var("FASTMOCK_OPERATOR_NAMESPACE")
        .ok()
        .filter(|s| !s.is_empty());
    let listen: std::net::SocketAddr = std::env::var("FASTMOCK_OPERATOR_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8081".into())
        .parse()?;
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(move || {
                let p = prom.clone();
                async move { p.render() }
            }),
        );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tracing::info!(
        namespace = ?namespace,
        %listen,
        default_image = %ctx.images.default_image,
        allowed_image_prefixes = ?ctx.images.allowed_prefixes,
        "fastmock-operator started"
    );
    controller::run(ctx, namespace).await;
    tracing::info!("fastmock-operator stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn crds_render() {
        let y = super::crds_yaml().unwrap();
        assert!(y.contains("kind: CustomResourceDefinition"));
        assert!(y.contains("fastmocks.fastmock.franshub.io"));
        assert!(y.contains("mockapis.fastmock.franshub.io"));
        assert!(y.contains("x-kubernetes-preserve-unknown-fields: true"));
        assert!(!y.contains("mode: ''"));
    }

    #[test]
    fn spec_defaults_are_usable() {
        let spec: crate::crd::FastMockSpec =
            serde_json::from_value(serde_json::json!({"adminTokensSecret": {"name": "t"}}))
                .unwrap();
        assert_eq!(spec.storage.mode, "memory");
        assert_eq!(spec.admin_tokens_secret.unwrap().key, "tokens");
        let crd =
            serde_json::to_value(<crate::crd::FastMock as kube::CustomResourceExt>::crd()).unwrap();
        let storage = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["storage"];
        assert_eq!(storage["default"]["mode"], "memory");
    }
}
