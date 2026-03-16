use pingora::prelude::*;
use tokio::sync::OnceCell;

// Project modules
mod auth;
mod config;
mod error;
mod gc;
mod orchestrator;
mod proxy;
mod service;

pub use error::HubError;

use crate::{gc::GarbageCollector, orchestrator::Orchestrator, service::AxumService};

pub static ONCE: OnceCell<Orchestrator> = OnceCell::const_new();
async fn orchestrator() -> &'static Orchestrator {
    ONCE.get_or_init(|| async { Orchestrator::new().await })
        .await
}
pub static SIDECAR: &'static str = "ghcr.io/nbhdai/workshop-sidecar:latest";

fn main() {
    tracing_subscriber::fmt::init();
    let config = config::Config::from_env();

    tracing::info!("Config loaded: {:?}", config);

    let gc = GarbageCollector;
    let service = AxumService::new();

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();
    my_server.add_service(background_service("garbage_collector", gc));
    my_server.add_service(service);

    let proxy_logic = proxy::WorkshopProxy;

    let mut lb = http_proxy_service(&my_server.configuration, proxy_logic);

    // Bind to the PUBLIC port
    lb.add_tcp("0.0.0.0:8080");

    my_server.add_service(lb);

    tracing::info!("Pingora Gateway listening on 0.0.0.0:8080");
    my_server.run_forever();
}

#[cfg(test)]
mod tests {
    pub mod config;
    pub mod gc;
    pub mod helpers;
    pub mod integration;
}
