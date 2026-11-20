# Writing plugins

Stubs, scenarios, templates and callbacks cover most providers. When a provider needs real logic (identifiers that
later requests refer to, reports that depend on earlier messages, its own signature scheme), write a plugin in Rust.

A plugin is a type that implements `fastmock_core::Plugin`. It is compiled into a custom binary that registers it
and then runs the regular FastMock command line, so `serve`, `migrate`, `hash-token`, `validate` and `plugins`
behave exactly as in the release binary. The release binary itself contains no plugins.

- [What a plugin gets](#what-a-plugin-gets)
- [A minimal plugin](#a-minimal-plugin)
- [The custom binary](#the-custom-binary)
- [Settings](#settings)
- [Callbacks, state and secrets](#callbacks-state-and-secrets)
- [Testing](#testing)
- [Container image](#container-image)

## What a plugin gets

| Item | Where |
|---|---|
| Data-plane routes | `router()` is mounted on the data port under `/<name>` |
| Admin routes (optional) | `admin_router()` is mounted under `/__admin/plugins/<name>/`, behind the same tokens and roles |
| Settings | `ctx.settings`, read from `FASTMOCK_PLUGIN_<NAME>_<KEY>` |
| Shared state | `ctx.stores`: the same stores the server uses (`memory` or PostgreSQL + Valkey) |
| Callbacks | `ctx.schedule_callback(job)`: the durable outbox with retries, HMAC and tracing |
| Secrets | `ctx.secret(name)`: values from `FASTMOCK_SECRETS_DIR` |
| Templates | `ctx.templates`: the sandboxed Handlebars engine |

Plugin traffic goes to the request journal and metrics with `source="<name>"` instead of `source="stubs"`.
`GET /__admin/plugins` and `fastmock plugins` list the compiled-in plugins; `FASTMOCK_PLUGINS` selects which are
active.

## A minimal plugin

```toml
# Cargo.toml of the plugin crate
[dependencies]
anyhow = "1"
axum = "0.8"
fastmock-core = { git = "<fastmock repository URL>", tag = "<version>" }
serde_json = "1"
```

```rust
use axum::{Json, Router, routing::get};
use fastmock_core::{Plugin, PluginContext};
use serde_json::json;

pub struct Echo;

impl Plugin for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "GET /hello answers with the greeting setting"
    }

    fn router(&self, ctx: PluginContext) -> anyhow::Result<Router> {
        let greeting = ctx.settings.get_or("greeting", "hello");
        Ok(Router::new().route("/hello", get(move || async move { greeting })))
    }

    fn admin_router(&self, ctx: PluginContext) -> Option<Router> {
        let name = ctx.name.clone();
        Some(Router::new().route("/info", get(move || async move { Json(json!({"plugin": name})) })))
    }
}

pub fn plugins() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(Echo)]
}
```

`router()` returning an error stops the server at startup with the plugin name in the message; validate settings
there rather than on the first request.

## The custom binary

```toml
[dependencies]
fastmock-server = { git = "<fastmock repository URL>", tag = "<version>" }
my-plugins = { path = "../my-plugins" }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fastmock_server::register_plugins(my_plugins::plugins);
    fastmock_server::cli::run().await
}
```

`register_plugins` must run before `cli::run()`, and only the first call counts. Pin `fastmock-core` and
`fastmock-server` to the same tag, and copy `Cargo.lock` from that tag to start from the tested dependency set.

```sh
FASTMOCK_PLUGIN_ECHO_GREETING=hi ./target/release/my-fastmock serve
curl -s localhost:8080/echo/hello                      # hi
curl -s -H "Authorization: Bearer $FASTMOCK_TOKEN" localhost:9090/__admin/plugins/echo/info
```

## Settings

`FASTMOCK_PLUGIN_<PLUGIN>_<KEY>`: plugin and key are upper-cased and `-` becomes `_`; the plugin sees the key in
lower case. `ctx.settings.get(key)`, `get_or(key, default)` and `parse_or(key, default)` read them. The Helm chart
and the `MockServer` resource pass plugin settings as ordinary environment variables.

Never put credentials into settings: name a secret in a setting and read it with `ctx.secret(name)`.

## Callbacks, state and secrets

- **Callbacks.** Build a `fastmock_core::CallbackJob` and pass it to `ctx.schedule_callback`. The job is stored in
  the outbox first, so it survives restarts, follows the egress policy and is retried like stub callbacks. Set
  `due_at` in the future instead of sleeping in the handler. A job gets the [space](../operations/access-control.md#spaces) of the stub
  being answered or, on a plugin admin route, the space that owns the request's path or host (when exactly one
  space does), so it is listed and cleared with that space's callbacks.
- **State.** `ctx.stores` gives the data, hot and journal stores. Use your plugin name as the namespace so data from
  different plugins does not collide. Everything stored there is shared between replicas in `postgres` mode.
- **Correlation.** Insert `fastmock_core::Correlation(id)` into the response extensions to make the journal entry
  searchable by `correlation=<id>`.

## Testing

Unit-test the router directly with `tower::ServiceExt::oneshot` and a `PluginContext` built over
`fastmock_core::memory::memory_stores`. For an end-to-end test, call `fastmock_server::register_plugins` and then
`fastmock_server::build(config, prometheus)`, which returns the data and admin routers without opening ports.

## Container image

Build the custom binary the same way as the release image (see the `Dockerfile` at the repository root): build the
UI, then `cargo build --release` of your binary crate, and copy the result into a distroless image. Keep the user
non-root and the entrypoint the binary itself, so the Helm chart and the operator work unchanged with your image.
