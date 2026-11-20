use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::model::CallbackJob;
use crate::storage::{SecretProvider, Stores};
use crate::template::Templates;

tokio::task_local! {
    pub static TRACEPARENT: Option<String>;
    /// The space of the stub or of the owned path the current data-plane request belongs to.
    pub static SPACE: Option<String>;
}

pub fn current_traceparent() -> Option<String> {
    TRACEPARENT.try_with(|t| t.clone()).ok().flatten()
}

pub fn current_space() -> Option<String> {
    SPACE.try_with(|s| s.clone()).ok().flatten()
}

#[derive(Clone, Debug)]
pub struct Correlation(pub String);

#[derive(Clone, Default, Debug)]
pub struct PluginSettings(Arc<BTreeMap<String, String>>);

impl PluginSettings {
    pub fn new(map: BTreeMap<String, String>) -> Self {
        Self(Arc::new(map))
    }

    pub fn from_env(plugin: &str) -> Self {
        let prefix = format!(
            "FASTMOCK_PLUGIN_{}_",
            plugin.to_ascii_uppercase().replace('-', "_")
        );
        let map = std::env::vars()
            .filter_map(|(k, v)| {
                k.strip_prefix(&prefix)
                    .map(|rest| (rest.to_ascii_lowercase(), v))
            })
            .collect();
        Self::new(map)
    }

    pub fn merged(&self, extra: &BTreeMap<String, String>) -> Self {
        let mut m = (*self.0).clone();
        for (k, v) in extra {
            m.insert(k.to_ascii_lowercase(), v.clone());
        }
        Self::new(m)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(&key.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }

    pub fn parse_or<T: FromStr>(&self, key: &str, default: T) -> anyhow::Result<T>
    where
        T::Err: std::fmt::Display,
    {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v
                .parse::<T>()
                .map_err(|e| anyhow::anyhow!("invalid value for {key}: {e}")),
        }
    }
}

#[derive(Clone)]
pub struct PluginContext {
    pub name: String,
    pub stores: Stores,
    pub settings: PluginSettings,
    pub templates: Arc<Templates>,
    pub secrets: Arc<dyn SecretProvider>,
}

impl PluginContext {
    pub async fn schedule_callback(&self, mut job: CallbackJob) -> anyhow::Result<()> {
        if job.traceparent.is_none() {
            job.traceparent = current_traceparent();
        }
        if job.space.is_none() {
            job.space = current_space();
        }
        self.stores.outbox.enqueue(&job).await
    }

    pub fn secret(&self, name: &str) -> Option<String> {
        self.secrets.get(name)
    }
}

pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn router(&self, ctx: PluginContext) -> anyhow::Result<axum::Router>;
    fn admin_router(&self, _ctx: PluginContext) -> Option<axum::Router> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn callbacks_inherit_the_space_of_the_request() {
        let stores = crate::memory::memory_stores(10);
        let ctx = PluginContext {
            name: "t".into(),
            stores: stores.clone(),
            settings: PluginSettings::default(),
            templates: Arc::new(Templates::new()),
            secrets: Arc::new(crate::storage::NoSecrets),
        };
        let job = |url: &str| {
            CallbackJob::new(
                "t",
                url.to_string(),
                "POST".into(),
                BTreeMap::new(),
                String::new(),
                chrono::Utc::now(),
                1,
            )
        };
        SPACE
            .scope(
                Some("pay".into()),
                ctx.schedule_callback(job("http://198.51.100.1/in")),
            )
            .await
            .unwrap();
        ctx.schedule_callback(job("http://198.51.100.1/out"))
            .await
            .unwrap();
        let mut seen: Vec<(String, Option<String>)> = stores
            .outbox
            .list(&crate::CallbackFilter::default())
            .await
            .unwrap()
            .into_iter()
            .map(|j| (j.url, j.space))
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            [
                (
                    "http://198.51.100.1/in".to_string(),
                    Some("pay".to_string())
                ),
                ("http://198.51.100.1/out".to_string(), None)
            ]
        );
    }
}
