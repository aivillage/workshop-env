use std::{collections::HashMap, path::Path};

use serde::Deserialize;
use tracing::{debug, info};

#[derive(Deserialize, Debug, Clone)]
pub struct Workshop {
    /// The public-facing name for this set of workshops.
    pub name: String,
    /// The container image to use for the workshop.
    pub image: String,
    /// The container image to use for the workshop.
    pub description: String,
    /// The port the container is listening on
    pub port: i32,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Top-level configuration loaded from environment variables.
#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    /// The public-facing name for this set of workshops.
    #[serde(default = "default_workshop")]
    pub workshops: Vec<Workshop>,

    /// Namespace where workshop pods and services will be created.
    #[serde(default = "default_workshop_namespace")]
    pub workshop_namespace: String,

    /// Max time-to-live for a pod in seconds, regardless of activity.
    #[serde(default = "default_workshop_ttl")]
    pub workshop_ttl_seconds: i64,

    /// Max idle time in seconds before a pod is cleaned up.
    #[serde(default = "default_workshop_idle")]
    pub workshop_idle_seconds: i64,

    /// The port the sidecar forwards traffic to the workshop on.
    #[serde(default = "default_sidecar_proxy_port")]
    pub sidecar_proxy_port: i32,

    /// The port the sidecar listens on for health reporting.
    #[serde(default = "default_sidecar_health_port")]
    pub sidecar_health_port: i32,

    /// Max number of concurrent workshop pods allowed to run.
    #[serde(default = "default_workshop_pod_limit")]
    pub workshop_pod_limit: usize,

    /// Workshop container CPU request.
    #[serde(default = "default_workshop_cpu_request")]
    pub workshop_cpu_request: String,

    /// Workshop container CPU limit.
    #[serde(default = "default_workshop_cpu_limit")]
    pub workshop_cpu_limit: String,

    /// Workshop container memory request.
    #[serde(default = "default_workshop_mem_request")]
    pub workshop_mem_request: String,

    /// Workshop container memory limit.
    #[serde(default = "default_workshop_mem_limit")]
    pub workshop_mem_limit: String,

    /// Workshop container memory limit.
    #[serde(default = "default_garbage_collection_seconds")]
    pub garbage_collection_seconds: i64,

    #[serde(default = "default_base_domain")]
    pub base_domain: String,

    #[serde(default = "default_sidecar_image")]
    pub sidecar_image: String,
}

impl Config {
    pub fn get_workshop(&self, workshop: &str) -> Option<&Workshop> {
        self.workshops.iter().find(|w| &w.name == workshop)
    }
}

pub(crate) fn default_workshop() -> Vec<Workshop> {
    vec![Workshop {
        name: "workshop".to_string(),
        image: "traefik/whoami".to_string(),
        description: "The host didn't finish setting this up".to_string(),
        port: 80,
        env: HashMap::new(),
    }]
}
fn default_workshop_namespace() -> String {
    "default".to_string()
}
fn default_workshop_ttl() -> i64 {
    8 * 60 * 60
} // 8 hours
fn default_workshop_idle() -> i64 {
    60 * 60
} // 1 hour

fn default_workshop_port() -> i32 {
    80
}
fn default_workshop_pod_limit() -> usize {
    10
}
fn default_sidecar_proxy_port() -> i32 {
    8888
}
fn default_sidecar_health_port() -> i32 {
    9000
}
fn default_workshop_cpu_request() -> String {
    "100m".to_string()
}
fn default_workshop_cpu_limit() -> String {
    "500m".to_string()
}
fn default_workshop_mem_request() -> String {
    "128Mi".to_string()
}
fn default_workshop_mem_limit() -> String {
    "512Mi".to_string()
}
fn default_garbage_collection_seconds() -> i64 {
    300
}
fn default_base_domain() -> String {
    std::env::var("BASE_DOMAIN")
        .expect("BASE_DOMAIN must be specified in workshop.yaml or via environment variable")
}
fn default_sidecar_image() -> String {
    match std::env::var("SIDECAR_IMAGE") {
        Ok(image) if !image.is_empty() => image,
        _ => "ghcr.io/aivillage/workshop-sidecar:latest".to_string(),
    }
}

impl Config {
    /// Loads configuration from environment variables.
    pub fn from_env() -> Self {
        if let Ok(path_str) = std::env::var("WORKSHOP_CONFIG") {
            let path = Path::new(&path_str);
            if path.exists() {
                info!(
                    "Loading workshop config from WORKSHOP_CONFIG path: {:?}",
                    path
                );
                return Self::from_yaml_file(path);
            } else {
                panic!(
                    "WORKSHOP_CONFIG environment variable set to {:?}, but file does not exist.",
                    path
                );
            }
        }

        // 2. Check for sensible default file locations (Container friendly)
        let default_paths = [
            "workshop.yaml",
            "examples/config.yaml",
            "/app/config/workshop.yaml",
            "/etc/workshop/config.yaml",
        ];

        for path_str in default_paths {
            let path = Path::new(path_str);
            if path.exists() {
                info!("Found config file at default location: {:?}", path);
                return Self::from_yaml_file(path);
            }
        }

        panic!(
            "Unable to find workshop config file. Checked WORKSHOP_CONFIG env var and default paths: workshop.yaml, examples/config.yaml, /app/config/workshop.yaml, /etc/workshop/config.yaml"
        );
    }

    pub fn from_yaml_file(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        info!("Loading pyroduct config from YAML file: {:?}", path);

        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read pyroduct config file {:?}: {}", path, e));

        let mut config: Self = serde_yaml::from_str(&contents)
            .unwrap_or_else(|e| panic!("Failed to parse pyroduct YAML config: {}", e));

        if let Ok(env_domain) = std::env::var("BASE_DOMAIN") {
            if !env_domain.is_empty() {
                config.base_domain = env_domain;
            }
        }

        if let Ok(env_image) = std::env::var("SIDECAR_IMAGE") {
            if !env_image.is_empty() {
                config.sidecar_image = env_image;
            }
        }

        debug!("Loaded pyroduct config: {:?}", config);
        config
    }
}
