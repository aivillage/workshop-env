use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, HTTPGetAction, Pod, PodSpec, Probe, ResourceRequirements,
    Service, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;

use crate::config::{Config, Workshop};
use crate::orchestrator::{
    HUB_ID, LABEL_MANAGED_BY, LABEL_USER_ID, LABEL_WORKSHOP_NAME, TTL_ANNOTATION,
};

pub fn create_workshop_pod_spec(
    pod_name: &str,
    user_id: &str,
    workshop: &Workshop,
    config: &Config,
    expires_at_timestamp: i64,
) -> Pod {
    // 1. Prepare Metadata
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_USER_ID.to_string(), user_id.to_string());
    labels.insert(LABEL_WORKSHOP_NAME.to_string(), workshop.name.clone());
    labels.insert(LABEL_MANAGED_BY.to_string(), HUB_ID.to_string());
    labels.insert("app".to_string(), pod_name.to_string());

    let mut annotations = BTreeMap::new();
    annotations.insert(TTL_ANNOTATION.to_string(), expires_at_timestamp.to_string());

    // 2. Define Resources (Convert config values to Quantity)
    let mut resource_requests = BTreeMap::new();
    resource_requests.insert(
        "cpu".to_string(),
        Quantity(config.workshop_cpu_request.to_string()),
    );
    resource_requests.insert(
        "memory".to_string(),
        Quantity(config.workshop_mem_request.to_string()),
    );

    let mut resource_limits = BTreeMap::new();
    resource_limits.insert(
        "cpu".to_string(),
        Quantity(config.workshop_cpu_limit.to_string()),
    );
    resource_limits.insert(
        "memory".to_string(),
        Quantity(config.workshop_mem_limit.to_string()),
    );

    let workshop_env = if workshop.env.is_empty() {
        None
    } else {
        Some(workshop.env.iter().map(|(k,v)| {
            EnvVar {
                name: k.clone(),
                value: Some(v.clone()),
                ..Default::default()
            }
        }).collect::<Vec<_>>())
    };
    

    // 3. Define Containers
    let workshop_container = Container {
        name: "workshop".to_string(),
        image: Some(workshop.image.clone()),
        image_pull_policy: Some("Always".to_string()),
        ports: Some(vec![ContainerPort {
            container_port: workshop.port,
            ..Default::default()
        }]),
        resources: Some(ResourceRequirements {
            limits: Some(resource_limits),
            requests: Some(resource_requests),
            claims: None,
        }),
        env: workshop_env,
        ..Default::default()
    };

    let sidecar_container = Container {
        name: "sidecar".to_string(),
        image: Some(crate::SIDECAR.to_string()),
        image_pull_policy: Some("Always".to_string()),
        env: Some(vec![
            EnvVar {
                name: "SIDECAR_HTTP_LISTEN".to_string(),
                value: Some(format!("0.0.0.0:{}", config.sidecar_health_port)),
                ..Default::default()
            },
            EnvVar {
                name: "SIDECAR_TCP_LISTEN".to_string(),
                value: Some(format!("0.0.0.0:{}", config.sidecar_proxy_port)),
                ..Default::default()
            },
            EnvVar {
                name: "SIDECAR_TARGET_TCP".to_string(),
                value: Some(format!("127.0.0.1:{}", workshop.port)),
                ..Default::default()
            },
        ]),
        ports: Some(vec![
            ContainerPort {
                name: Some("health".to_string()),
                container_port: config.sidecar_health_port as i32,
                ..Default::default()
            },
            ContainerPort {
                name: Some("proxy".to_string()),
                container_port: config.sidecar_proxy_port as i32,
                ..Default::default()
            },
        ]),
        readiness_probe: Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/health".to_string()),
                // Note: Port in Probe is IntOrString
                port: IntOrString::Int(config.sidecar_health_port as i32),
                scheme: Some("HTTP".to_string()),
                ..Default::default()
            }),
            initial_delay_seconds: Some(5),
            period_seconds: Some(5),
            ..Default::default()
        }),
        liveness_probe: Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/health".to_string()),
                port: IntOrString::Int(config.sidecar_health_port as i32),
                scheme: Some("HTTP".to_string()),
                ..Default::default()
            }),
            initial_delay_seconds: Some(30),
            period_seconds: Some(30),
            failure_threshold: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    };

    // 4. Construct Pod
    Pod {
        metadata: ObjectMeta {
            name: Some(pod_name.to_string()),
            labels: Some(labels),
            annotations: Some(annotations),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            containers: vec![workshop_container, sidecar_container],
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn create_workshop_service_spec(
    service_name: &str,
    pod_name: &str,
    user_id: &str,
    workshop_name: &str,
    owner_ref: OwnerReference,
    config: &Config,
) -> Service {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_USER_ID.to_string(), user_id.to_string());
    labels.insert(LABEL_WORKSHOP_NAME.to_string(), workshop_name.to_string());
    labels.insert(LABEL_MANAGED_BY.to_string(), HUB_ID.to_string());

    let mut selector = BTreeMap::new();
    selector.insert("app".to_string(), pod_name.to_string());

    Service {
        metadata: ObjectMeta {
            name: Some(service_name.to_string()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()), // Note: field is `type_` not `type`
            selector: Some(selector),
            ports: Some(vec![
                ServicePort {
                    name: Some("proxy".to_string()),
                    port: config.sidecar_proxy_port as i32,
                    target_port: Some(IntOrString::Int(config.sidecar_proxy_port as i32)),
                    ..Default::default()
                },
                ServicePort {
                    name: Some("health".to_string()),
                    port: config.sidecar_health_port as i32,
                    target_port: Some(IntOrString::Int(config.sidecar_health_port as i32)),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}
