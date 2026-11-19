use std::collections::HashMap;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::LogFormat;

pub struct Telemetry {
    pub prometheus: PrometheusHandle,
    pub otel_enabled: bool,
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
}

fn otel_requested() -> bool {
    let disabled = std::env::var("OTEL_SDK_DISABLED")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    !disabled
        && [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        ]
        .iter()
        .any(|k| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false))
}

pub fn prometheus() -> anyhow::Result<PrometheusHandle> {
    let buckets = [
        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
        300.0,
    ];
    Ok(PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), &buckets)?
        .install_recorder()?)
}

pub fn init(format: LogFormat) -> anyhow::Result<Telemetry> {
    let prometheus = prometheus()?;
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let filter = || {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,sqlx=warn,hyper=warn,h2=warn,reqwest=warn,opentelemetry=warn")
        })
    };
    let fmt_layer = match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_filter(filter())
            .boxed(),
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_filter(filter())
            .boxed(),
    };
    let (tracer, logger) = if otel_requested() {
        let resource = Resource::builder()
            .with_service_name(
                std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "fastmock".into()),
            )
            .build();
        let span_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;
        let tracer = SdkTracerProvider::builder()
            .with_batch_exporter(span_exporter)
            .with_resource(resource.clone())
            .build();
        let log_exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .build()?;
        let logger = SdkLoggerProvider::builder()
            .with_batch_exporter(log_exporter)
            .with_resource(resource)
            .build();
        opentelemetry::global::set_tracer_provider(tracer.clone());
        (Some(tracer), Some(logger))
    } else {
        (None, None)
    };
    let otel_trace_layer = tracer.as_ref().map(|t| {
        tracing_opentelemetry::layer()
            .with_tracer(t.tracer("fastmock"))
            .with_filter(filter())
    });
    let otel_log_layer = logger.as_ref().map(|l| {
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(l)
            .with_filter(filter())
    });
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .try_init()?;
    metrics::gauge!("fastmock_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
    Ok(Telemetry {
        prometheus,
        otel_enabled: tracer.is_some(),
        tracer,
        logger,
    })
}

impl Telemetry {
    pub fn shutdown(&self) {
        if let Some(t) = &self.tracer {
            let _ = t.shutdown();
        }
        if let Some(l) = &self.logger {
            let _ = l.shutdown();
        }
    }
}

struct HeaderExtract<'a>(&'a http::HeaderMap);

impl Extractor for HeaderExtract<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

struct MapInject<'a>(&'a mut HashMap<String, String>);

impl Injector for MapInject<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

struct MapExtract<'a>(&'a HashMap<String, String>);

impl Extractor for MapExtract<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceIds {
    pub trace_id: String,
    pub span_id: String,
}

impl TraceIds {
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }
}

fn random_hex(bytes: usize) -> String {
    loop {
        let v: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
        if v.iter().any(|b| *b != 0) {
            return hex::encode(v);
        }
    }
}

pub fn parse_traceparent(tp: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = tp.trim().split('-').collect();
    if parts.len() < 4 || parts[0].len() != 2 || parts[0] == "ff" {
        return None;
    }
    let (trace, span) = (parts[1], parts[2]);
    let hex_ok = |s: &str, n: usize| {
        s.len() == n
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && s.bytes().any(|b| b != b'0')
    };
    (hex_ok(trace, 32) && hex_ok(span, 16) && parts[3].len() == 2)
        .then(|| (trace.to_string(), span.to_string()))
}

pub fn attach_parent(span: &tracing::Span, headers: &http::HeaderMap) {
    let cx = opentelemetry::global::get_text_map_propagator(|p| p.extract(&HeaderExtract(headers)));
    let _ = span.set_parent(cx);
}

pub fn attach_parent_traceparent(span: &tracing::Span, traceparent: Option<&str>) {
    if let Some(tp) = traceparent {
        let map = HashMap::from([("traceparent".to_string(), tp.to_string())]);
        let cx = opentelemetry::global::get_text_map_propagator(|p| p.extract(&MapExtract(&map)));
        let _ = span.set_parent(cx);
    }
}

pub fn ids_for(span: &tracing::Span, incoming: Option<&str>) -> TraceIds {
    let cx = span.context();
    let sc = cx.span().span_context().clone();
    if sc.is_valid() {
        return TraceIds {
            trace_id: sc.trace_id().to_string(),
            span_id: sc.span_id().to_string(),
        };
    }
    let trace_id = incoming
        .and_then(parse_traceparent)
        .map(|(t, _)| t)
        .unwrap_or_else(|| random_hex(16));
    TraceIds {
        trace_id,
        span_id: random_hex(8),
    }
}

pub fn outgoing_traceparent(span: &tracing::Span, parent: Option<&str>) -> Option<String> {
    let mut map = HashMap::new();
    let cx = span.context();
    if cx.span().span_context().is_valid() {
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut MapInject(&mut map))
        });
        return map.remove("traceparent");
    }
    parent.and_then(parse_traceparent).map(|(t, _)| {
        TraceIds {
            trace_id: t,
            span_id: random_hex(8),
        }
        .traceparent()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_parsing() {
        let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        assert_eq!(
            parse_traceparent(tp).unwrap().0,
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-b7ad6b7169203331-01").is_none()
        );
        assert!(
            parse_traceparent("ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01").is_none()
        );
        assert!(
            parse_traceparent("00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01").is_none()
        );
        assert!(parse_traceparent("garbage").is_none());
    }

    #[test]
    fn fallback_ids_keep_incoming_trace() {
        let span = tracing::Span::none();
        let ids = ids_for(
            &span,
            Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
        );
        assert_eq!(ids.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(ids.span_id.len(), 16);
        let fresh = ids_for(&span, None);
        assert_eq!(fresh.trace_id.len(), 32);
        let child = outgoing_traceparent(&span, Some(&ids.traceparent())).unwrap();
        assert!(child.starts_with("00-0af7651916cd43dd8448eb211c80319c-"));
    }
}
