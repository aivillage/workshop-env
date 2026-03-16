//! Test helpers for workshop-hub integration tests.
//!
//! Provides `TestContext` for setting up isolated Kubernetes test environments
//! and `TestGarbageCollector` for testing GC with configurable idle thresholds.

use super::config::{get_gc_test_config, get_test_config, validate_talos_environment};
use crate::{config::Config, orchestrator::Orchestrator};
use k8s_openapi::api::core::v1::{Namespace, Pod, Service};
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, PostParams},
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

/// Main test context that encapsulates all test dependencies.
///
/// Each test gets its own isolated namespace and `Orchestrator` instance,
/// ensuring tests don't interfere with each other.
pub struct TestContext {
    pub client: Client,
    pub orchestrator: Arc<Orchestrator>,
    pub test_namespace: String,
}

impl TestContext {
    /// Create a standard test context.
    ///
    /// Uses default test configuration with reasonable timeouts.
    /// `test_name` should be the name of the calling test function.
    #[tracing::instrument(level = "info", skip_all, fields(test_name = %test_name))]
    pub async fn new(test_name: &str) -> Self {
        info!("Creating standard test context");
        Self::with_config(get_test_config(), test_name).await
    }

    /// Create a test context optimized for GC testing.
    ///
    /// Uses shorter TTL and idle timeouts to speed up GC-related tests.
    #[tracing::instrument(level = "info", skip_all, fields(test_name = %test_name))]
    pub async fn new_for_gc(test_name: &str) -> Self {
        info!("Creating GC-optimized test context");
        Self::with_config(get_gc_test_config(), test_name).await
    }

    async fn with_config(mut config: Config, test_name: &str) -> Self {
        trace!("Validating Talos environment");
        validate_talos_environment()
            .expect("Not running in Talos environment. See README for setup instructions.");

        info!("🧪 Setting up test context");

        trace!("Creating Kubernetes client");
        let client = Client::try_default()
            .await
            .expect("Failed to create test Kubernetes client. Is the Talos cluster running?");

        // Create a consistent namespace name for this test (no random suffix)
        // This means the same test always uses the same namespace
        let test_namespace = format!("test-{}", test_name.to_lowercase().replace('_', "-"));
        debug!(namespace = %test_namespace, "Computed test namespace name");

        trace!("Creating/verifying test namespace");

        // Try to create the namespace (idempotent)
        let ns_api: Api<Namespace> = Api::all(client.clone());
        let namespace: Namespace = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": test_namespace,
                "labels": {
                    "workshop-hub/test": "true",
                    "workshop-hub/test-name": test_name
                }
            }
        }))
        .unwrap();

        // Create if it doesn't exist, ignore if it already exists
        match ns_api.create(&PostParams::default(), &namespace).await {
            Ok(_) => {
                info!(namespace = %test_namespace, "✓ Created test namespace");
                // Wait for namespace to be ready
                trace!("Waiting 500ms for namespace to be ready");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(kube::Error::Api(err)) if err.code == 409 => {
                info!(namespace = %test_namespace, "✓ Using existing test namespace");
            }
            Err(e) => {
                error!(namespace = %test_namespace, error = %e, "❌ Failed to create test namespace");
                panic!("Failed to create test namespace: {}", e);
            }
        }

        // Update config to use this namespace
        config.workshop_namespace = test_namespace.clone();
        // Make workshop_name unique to this namespace
        config.workshops[0].name = format!("{}-test", config.workshops[0].name);
        debug!(
            workshop_name = %config.workshops[0].name,
            workshop_namespace = %config.workshop_namespace,
            "Updated config for test context"
        );

        trace!("Creating orchestrator with test config");
        let orchestrator = Arc::new(Orchestrator::with_config(config).await);

        let ctx = Self {
            client,
            orchestrator,
            test_namespace,
        };

        info!("🧹 Clearing test namespace before test execution");
        ctx.clear().await;

        info!("✅ Test context ready");
        ctx
    }

    /// Get a reference to the test configuration.
    pub fn config(&self) -> &Config {
        &self.orchestrator.config
    }

    /// Clear all resources in the test namespace (but keep the namespace).
    ///
    /// This is called automatically when creating a test context.
    #[tracing::instrument(level = "info", skip(self), fields(namespace = %self.test_namespace))]
    pub async fn clear(&self) {
        info!("🧹 Clearing test namespace");

        // Delete all pods
        let pod_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.test_namespace);

        match pod_api.list(&ListParams::default()).await {
            Ok(pods) => {
                debug!(count = pods.items.len(), "Found pods to delete");
                for pod in pods.items {
                    if let Some(name) = pod.metadata.name {
                        trace!(pod = %name, "Deleting pod");
                        if let Err(e) = pod_api.delete(&name, &DeleteParams::default()).await {
                            warn!(pod = %name, error = %e, "Failed to delete pod");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to list pods for cleanup");
            }
        }

        // Delete all services
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), &self.test_namespace);

        match svc_api.list(&ListParams::default()).await {
            Ok(services) => {
                debug!(count = services.items.len(), "Found services to delete");
                for service in services.items {
                    if let Some(name) = service.metadata.name {
                        trace!(service = %name, "Deleting service");
                        if let Err(e) = svc_api.delete(&name, &DeleteParams::default()).await {
                            warn!(service = %name, error = %e, "Failed to delete service");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to list services for cleanup");
            }
        }

        // Wait for deletions to complete
        debug!("Waiting 2s for resource deletions to propagate");
        tokio::time::sleep(Duration::from_secs(2)).await;

        info!("✓ Test namespace cleared");
    }

    /// Create a test pod with standard labels.
    ///
    /// The pod is labeled for management by workshop-hub and can be
    /// targeted by the garbage collector.
    #[tracing::instrument(level = "info", skip(self), fields(user_id = %user_id))]
    pub async fn create_test_pod(&self, user_id: &str) -> Result<Pod, kube::Error> {
        info!("Creating test pod");

        let pod_api: Api<Pod> =
            Api::namespaced(self.client.clone(), &self.config().workshop_namespace);

        let pod_name = format!("{}-{}", self.config().workshops[0].name, user_id);

        let mut labels = BTreeMap::new();
        labels.insert("workshop-hub/user-id".to_string(), user_id.to_string());
        labels.insert(
            "workshop-hub/workshop-name".to_string(),
            self.config().workshops[0].name.clone(),
        );
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            "workshop-hub".to_string(),
        );
        // Add app label for service selector (matches orchestrator pattern)
        labels.insert("app".to_string(), pod_name.clone());

        debug!(pod_name = %pod_name, labels = ?labels, "Pod configuration");

        let pod: Pod = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": pod_name,
                "namespace": self.config().workshop_namespace,
                "labels": labels,
            },
            "spec": {
                "containers": [{
                    "name": "workshop",
                    "image": self.config().workshops[0].image,
                    "ports": [{
                        "containerPort": 80
                    }],
                    "resources": {
                        "requests": {
                            "cpu": self.config().workshop_cpu_request,
                            "memory": self.config().workshop_mem_request,
                        },
                        "limits": {
                            "cpu": self.config().workshop_cpu_limit,
                            "memory": self.config().workshop_mem_limit,
                        }
                    }
                }]
            }
        }))
        .unwrap();

        let created_pod = pod_api.create(&PostParams::default(), &pod).await?;

        info!(pod_name = %pod_name, "✓ Test pod created");
        trace!(metadata = ?created_pod.metadata, "Pod details");

        Ok(created_pod)
    }

    /// Create a test service for a user pod.
    #[tracing::instrument(level = "info", skip(self), fields(user_id = %user_id))]
    pub async fn create_test_service(&self, user_id: &str) -> Result<Service, kube::Error> {
        info!("Creating test service");

        let svc_api: Api<Service> =
            Api::namespaced(self.client.clone(), &self.config().workshop_namespace);

        let service_name = format!("{}-{}", self.config().workshops[0].name, user_id);
        let pod_name = format!("{}-{}", self.config().workshops[0].name, user_id);

        debug!(
            service_name = %service_name,
            target_pod = %pod_name,
            "Service configuration"
        );

        let mut selector = BTreeMap::new();
        selector.insert("app".to_string(), pod_name);

        let service: Service = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": service_name,
                "namespace": self.config().workshop_namespace,
            },
            "spec": {
                "selector": selector,
                "ports": [{
                    "protocol": "TCP",
                    "port": 8080,
                    "targetPort": 8080,
                }]
            }
        }))
        .unwrap();

        let created_service = svc_api.create(&PostParams::default(), &service).await?;

        info!(service_name = %service_name, "✓ Test service created");

        Ok(created_service)
    }

    /// Check if a pod exists.
    #[tracing::instrument(level = "trace", skip(self), fields(pod_name = %pod_name))]
    pub async fn pod_exists(&self, pod_name: &str) -> bool {
        trace!("Checking if pod exists");

        let pod_api: Api<Pod> =
            Api::namespaced(self.client.clone(), &self.config().workshop_namespace);

        let exists = pod_api.get(pod_name).await.is_ok();

        trace!(exists = exists, "Pod existence check result");

        exists
    }

    /// Check if a service exists.
    #[tracing::instrument(level = "trace", skip(self), fields(service_name = %service_name))]
    pub async fn service_exists(&self, service_name: &str) -> bool {
        trace!("Checking if service exists");

        let svc_api: Api<Service> =
            Api::namespaced(self.client.clone(), &self.config().workshop_namespace);

        let exists = svc_api.get(service_name).await.is_ok();

        trace!(exists = exists, "Service existence check result");

        exists
    }

    /// Count the number of workshop-managed pods in the namespace.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn count_managed_pods(&self) -> usize {
        trace!("Counting managed pods");

        let pod_api: Api<Pod> =
            Api::namespaced(self.client.clone(), &self.config().workshop_namespace);

        let label_selector = format!(
            "app.kubernetes.io/managed-by=workshop-hub,workshop-hub/workshop-name={}",
            self.config().workshops[0].name
        );
        trace!(label_selector = %label_selector, "Using label selector");

        let list_params = ListParams::default().labels(&label_selector);

        let pods = pod_api
            .list(&list_params)
            .await
            .expect("Failed to list pods");

        let count = pods.items.len();

        debug!(count = count, "Found managed pods");

        count
    }

    /// Wait for a pod to reach running state.
    #[tracing::instrument(level = "info", skip(self), fields(pod_name = %pod_name))]
    pub async fn wait_for_pod_running(
        &self,
        pod_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("⏳ Waiting for pod to reach Running state");

        let pod_api: Api<Pod> =
            Api::namespaced(self.client.clone(), &self.config().workshop_namespace);

        let timeout = Duration::from_secs(60);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                error!("❌ Timeout waiting for pod to reach Running state");
                return Err(format!("Timeout waiting for pod {} to be running", pod_name).into());
            }

            match pod_api.get(pod_name).await {
                Ok(pod) => {
                    if let Some(status) = &pod.status {
                        if let Some(phase) = &status.phase {
                            trace!(phase = %phase, "Current pod phase");

                            if phase == "Running" {
                                info!("✓ Pod is Running");
                                return Ok(());
                            }

                            if phase == "Failed" || phase == "Unknown" {
                                error!(phase = %phase, "❌ Pod entered failed state");
                                return Err(
                                    format!("Pod {} failed to start: {}", pod_name, phase).into()
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Error getting pod status");
                    return Err(format!("Failed to get pod {}: {}", pod_name, e).into());
                }
            }

            trace!("Pod not ready yet, waiting 500ms");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
