use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use fastmock_core::CallbackJob;
use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio::sync::{Semaphore, watch};
use tracing::Instrument;

use crate::state::AppState;
use crate::telemetry;

/// A callback job header that marks `body` as base64 of binary content (mail webhooks with binary
/// attachments); the outbox decodes the body and never sends the header.
pub const BODY_BASE64_HEADER: &str = "x-fastmock-body-base64";

#[derive(Clone)]
pub struct EgressPolicy {
    pub allowed_hosts: Vec<String>,
    pub denied: Vec<IpNet>,
}

impl EgressPolicy {
    pub fn host_allowed(&self, host: &str) -> bool {
        if self.allowed_hosts.is_empty() {
            return true;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.allowed_hosts.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            match a.strip_prefix("*.") {
                Some(suffix) => host.ends_with(&format!(".{suffix}")),
                None => host == a,
            }
        })
    }

    pub fn ip_allowed(&self, ip: IpAddr) -> bool {
        if ip.is_unspecified() || ip.is_multicast() {
            return false;
        }
        let mut candidates = vec![ip];
        if let IpAddr::V6(v6) = ip {
            let seg = v6.segments();
            let o = v6.octets();
            if let Some(v4) = v6.to_ipv4_mapped() {
                candidates.push(IpAddr::V4(v4));
            }
            if seg[..6] == [0, 0, 0, 0, 0, 0] {
                candidates.push(IpAddr::V4(std::net::Ipv4Addr::new(
                    o[12], o[13], o[14], o[15],
                )));
            }
            if seg[0] == 0x64 && seg[1] == 0xff9b {
                candidates.push(IpAddr::V4(std::net::Ipv4Addr::new(
                    o[12], o[13], o[14], o[15],
                )));
            }
            if seg[0] == 0x2002 {
                candidates.push(IpAddr::V4(std::net::Ipv4Addr::new(o[2], o[3], o[4], o[5])));
            }
        }
        candidates
            .iter()
            .all(|c| !c.is_unspecified() && !self.denied.iter().any(|n| n.contains(c)))
    }

    pub fn check_url(&self, url: &reqwest::Url) -> Result<(), String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("scheme {} is not allowed", url.scheme()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("credentials in callback URLs are not allowed".into());
        }
        let host = url.host_str().ok_or("callback URL has no host")?;
        if !self.host_allowed(host) {
            return Err(format!(
                "host {host} is not in FASTMOCK_CALLBACK_ALLOWED_HOSTS"
            ));
        }
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = literal.parse::<IpAddr>()
            && !self.ip_allowed(ip)
        {
            return Err(format!(
                "address {ip} is denied by FASTMOCK_CALLBACK_DENIED_CIDRS"
            ));
        }
        Ok(())
    }
}

struct GuardedResolver(Arc<EgressPolicy>);

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.0.clone();
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|a| policy.ip_allowed(a.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(format!(
                    "all addresses of {host} are denied by FASTMOCK_CALLBACK_DENIED_CIDRS"
                )
                .into());
            }
            let it: Addrs = Box::new(allowed.into_iter());
            Ok(it)
        })
    }
}

pub fn load_outbound_ca(
    path: Option<&std::path::Path>,
) -> anyhow::Result<Vec<reqwest::Certificate>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let pem = std::fs::read(path).map_err(|e| {
        anyhow::anyhow!("reading FASTMOCK_OUTBOUND_CA_FILE {}: {e}", path.display())
    })?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
        anyhow::anyhow!("parsing FASTMOCK_OUTBOUND_CA_FILE {}: {e}", path.display())
    })?;
    if certs.is_empty() {
        anyhow::bail!(
            "FASTMOCK_OUTBOUND_CA_FILE {} contains no PEM certificates",
            path.display()
        );
    }
    Ok(certs)
}

pub fn http_client(
    policy: Arc<EgressPolicy>,
    timeout: Duration,
    extra_roots: &[reqwest::Certificate],
) -> anyhow::Result<reqwest::Client> {
    Ok(client_builder(policy, timeout, extra_roots).build()?)
}

pub fn client_builder(
    policy: Arc<EgressPolicy>,
    timeout: Duration,
    extra_roots: &[reqwest::Certificate],
) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder();
    for c in extra_roots {
        builder = builder.add_root_certificate(c.clone());
    }
    builder
        .dns_resolver(Arc::new(GuardedResolver(policy)))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(5)))
        .user_agent(concat!("FastMock/", env!("CARGO_PKG_VERSION")))
}

pub fn backoff(attempt: u32) -> Duration {
    let ms = 500u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(10));
    Duration::from_millis(ms.min(60_000))
}

pub async fn run(app: AppState, policy: Arc<EgressPolicy>, mut shutdown: watch::Receiver<bool>) {
    let sem = Arc::new(Semaphore::new(app.cfg.callback_workers));
    let lease = app.cfg.callback_timeout * 2 + Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(app.cfg.callback_poll) => {}
        }
        let free = sem.available_permits().min(app.cfg.callback_batch as usize) as i64;
        if free == 0 {
            continue;
        }
        let jobs = match app.stores.outbox.claim_due(Utc::now(), free, lease).await {
            Ok(j) => j,
            Err(e) => {
                metrics::counter!("fastmock_store_errors_total", "store" => "outbox").increment(1);
                tracing::error!(error = %e, "claiming callbacks failed");
                continue;
            }
        };
        for job in jobs {
            let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
            let app = app.clone();
            let policy = policy.clone();
            tokio::spawn(async move {
                deliver(&app, &policy, job).await;
                drop(permit);
            });
        }
    }
    let _ = sem.acquire_many(app.cfg.callback_workers as u32).await;
}

async fn deliver(app: &AppState, policy: &EgressPolicy, job: CallbackJob) {
    let span = tracing::info_span!(
        "fastmock.callback",
        otel.name = %format!("{} callback", job.source),
        otel.kind = "client",
        http.request.method = %job.method,
        url.full = tracing::field::Empty,
        fastmock.source = %job.source,
        fastmock.callback_id = %job.id,
        fastmock.attempt = job.attempts,
        http.response.status_code = tracing::field::Empty,
    );
    telemetry::attach_parent_traceparent(&span, job.traceparent.as_deref());
    async {
        let start = Instant::now();
        let source = job.source.clone();
        let outcome = send(app, policy, &job).await;
        metrics::histogram!("fastmock_callback_duration_seconds", "source" => source.clone()).record(start.elapsed().as_secs_f64());
        let result = match outcome {
            Ok(status) => {
                tracing::Span::current().record("http.response.status_code", status);
                match app.stores.outbox.mark_sent(job.id, job.attempts, status).await {
                    Ok(true) => {}
                    Ok(false) => {
                        metrics::counter!("fastmock_callbacks_stale_results_total").increment(1);
                        tracing::warn!(callback_id = %job.id, "delivery result ignored: the lease expired and another attempt owns the callback");
                    }
                    Err(e) => tracing::error!(error = %e, "marking callback sent failed"),
                }
                tracing::info!(callback_id = %job.id, status, "callback delivered");
                "sent"
            }
            Err(Failure { message, status, retryable }) => {
                let retry_at = (retryable && job.attempts < job.max_attempts)
                    .then(|| Utc::now() + chrono::Duration::from_std(backoff(job.attempts)).unwrap_or_default());
                match app.stores.outbox.mark_attempt_failed(job.id, job.attempts, &message, status, retry_at).await {
                    Ok(true) => {}
                    Ok(false) => metrics::counter!("fastmock_callbacks_stale_results_total").increment(1),
                    Err(e) => tracing::error!(error = %e, "marking callback failure failed"),
                }
                tracing::warn!(callback_id = %job.id, attempt = job.attempts, error = %message, retry = retry_at.is_some(), "callback attempt failed");
                if !retryable && message.starts_with("blocked") {
                    "blocked"
                } else if retry_at.is_some() {
                    "retry"
                } else {
                    "failed"
                }
            }
        };
        metrics::counter!("fastmock_callbacks_total", "source" => source, "result" => result).increment(1);
    }
    .instrument(span)
    .await
}

struct Failure {
    message: String,
    status: Option<u16>,
    retryable: bool,
}

async fn send(app: &AppState, policy: &EgressPolicy, job: &CallbackJob) -> Result<u16, Failure> {
    let url = reqwest::Url::parse(&job.url).map_err(|e| Failure {
        message: format!("invalid url: {e}"),
        status: None,
        retryable: false,
    })?;
    policy.check_url(&url).map_err(|m| Failure {
        message: format!("blocked by egress policy: {m}"),
        status: None,
        retryable: false,
    })?;
    let mut redacted = url.clone();
    redacted.set_query(None);
    tracing::Span::current().record("url.full", redacted.as_str());
    let method = reqwest::Method::from_bytes(job.method.as_bytes()).map_err(|_| Failure {
        message: "invalid method".into(),
        status: None,
        retryable: false,
    })?;
    let mut rb = app.http.request(method, url);
    let mut base64_body = false;
    for (k, v) in &job.headers {
        if k.eq_ignore_ascii_case(BODY_BASE64_HEADER) {
            base64_body = true;
            continue;
        }
        if fastmock_core::outbound::blocked_outbound_header(k)
            || k.eq_ignore_ascii_case("traceparent")
        {
            continue;
        }
        rb = rb.header(k, v);
    }
    rb = rb.header("x-fastmock-delivery-id", job.id.to_string());
    rb = rb.header("x-fastmock-delivery-attempt", job.attempts.to_string());
    if let Some(tp) =
        telemetry::outgoing_traceparent(&tracing::Span::current(), job.traceparent.as_deref())
    {
        rb = rb.header("traceparent", tp);
    }
    if base64_body {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(job.body.as_bytes())
            .map_err(|_| Failure {
                message: "invalid base64 body".into(),
                status: None,
                retryable: false,
            })?;
        rb = rb.body(bytes);
    } else if !job.body.is_empty() {
        rb = rb.body(job.body.clone());
    }
    let resp = rb.send().await.map_err(|e| Failure {
        message: format!("request failed: {}", without_url(&e)),
        status: None,
        retryable: true,
    })?;
    let status = resp.status().as_u16();
    if resp.status().is_success() {
        Ok(status)
    } else {
        Err(Failure {
            message: format!("unexpected status {status}"),
            status: Some(status),
            retryable: status >= 500 || status == 408 || status == 429,
        })
    }
}

fn without_url(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(u) = e.url() {
        s = s.replace(u.as_str(), "<url>");
    }
    let mut src = std::error::Error::source(e);
    while let Some(inner) = src {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&str]) -> EgressPolicy {
        EgressPolicy {
            allowed_hosts: allowed.iter().map(|s| s.to_string()).collect(),
            denied: crate::config::DEFAULT_DENIED_CIDRS
                .split(',')
                .map(|c| c.parse().unwrap())
                .collect(),
        }
    }

    #[test]
    fn allowlist_and_cidrs() {
        let p = policy(&["*.svc.cluster.local", "hooks.example.com"]);
        assert!(
            p.check_url(&"http://receiver.app.svc.cluster.local/cb".parse().unwrap())
                .is_ok()
        );
        assert!(
            p.check_url(&"https://hooks.example.com/x".parse().unwrap())
                .is_ok()
        );
        assert!(
            p.check_url(&"https://evil.example.com/x".parse().unwrap())
                .is_err()
        );
        assert!(
            p.check_url(&"https://svc.cluster.local.evil.com/x".parse().unwrap())
                .is_err()
        );
        let open = policy(&[]);
        assert!(
            open.check_url(&"http://169.254.169.254/latest/meta-data".parse().unwrap())
                .is_err()
        );
        assert!(
            open.check_url(&"http://[::ffff:169.254.169.254]/".parse().unwrap())
                .is_err()
        );
        assert!(
            open.check_url(&"file:///etc/passwd".parse().unwrap())
                .is_err()
        );
        assert!(
            open.check_url(&"http://user:pw@example.com/".parse().unwrap())
                .is_err()
        );
        assert!(open.check_url(&"http://10.1.2.3/".parse().unwrap()).is_ok());
        for bad in [
            "http://[::]:80/",
            "http://[0:0:0:0:0:0:0:0]/",
            "http://[::127.0.0.1]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[2002:7f00:1::]/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
        ] {
            assert!(open.check_url(&bad.parse().unwrap()).is_err(), "{bad}");
        }
        let lab = EgressPolicy {
            allowed_hosts: vec![],
            denied: vec![],
        };
        assert!(
            lab.check_url(&"http://127.0.0.1:9/".parse().unwrap())
                .is_ok()
        );
        assert!(lab.check_url(&"http://[::]:9/".parse().unwrap()).is_err());
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(30), Duration::from_secs(60));
    }
}
