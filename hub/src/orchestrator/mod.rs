use std::time::Duration;

use chrono::Utc;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{
    Api, Client as KubeClient, Error as KubeError, ResourceExt,
    api::{DeleteParams, ListParams, PostParams},
};
use papaya::{Guard, HashMap};
use tracing::{debug, info, warn};

use crate::{HubError, config};

mod definition;
mod pod;
// Re-export ManagedPod and Health structs
pub use pod::{ManagedPod, SidecarHealth};

// Constants
const LABEL_USER_ID: &str = "workshop-hub/user-id";
const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const HUB_ID: &str = "workshop-hub";
pub const TTL_ANNOTATION: &str = "workshop-hub/ttl-expires-at";
pub const LABEL_WORKSHOP_NAME: &str = "workshop-hub/workshop-name";

pub struct Orchestrator {
    pub config: config::Config,
    pub pods: HashMap<String, ManagedPod>,
    http_client: reqwest::Client,
    pod_api: Api<Pod>,
    svc_api: Api<Service>,
}

pub enum PodStatus {
    Healthy(Pod, String),
    Old(ManagedPod),
}

#[derive(Debug)]
pub enum PodDiagnosis {
    /// The happy path: IP is assigned and we built the URL.
    Ready(String),
    /// Pod has been deleted or hasn't reported status yet.
    NoStatus,
    /// Pod is waiting for the K8s scheduler to assign a Node.
    PendingScheduling,
    /// Scheduled, but pulling images or creating the container.
    ContainerCreating,
    /// Critical: Configuration error or private repo auth failure.
    ImagePullError(String),
    /// Critical: App is starting but immediately dying.
    CrashLoop(String),
    /// Pod finished (Success or Failure) and is no longer running.
    Terminated(String),
    /// Fallback for when we don't know why it's pending.
    UnknownPending,
}

#[derive(Debug)]
pub enum SidecarError {
    /// The pod is still starting up (Pending, ContainerCreating).
    NotReady,
    /// The pod has vanished from Kubernetes.
    Gone,
    /// Critical K8s errors (CrashLoop, ImagePullBackOff).
    K8sFailure(String),
    /// Network level error (Connect timeout, DNS).
    NetworkError(reqwest::Error),
    /// The sidecar responded, but with a non-200 status code.
    UnhealthyResponse(reqwest::StatusCode),
    /// We couldn't parse the JSON response.
    InvalidResponse(reqwest::Error),
    /// Wrapper for internal HubErrors during refresh.
    Internal(String),
}

impl Orchestrator {
    pub async fn new() -> Self {
        let config = config::Config::from_env();
        Self::with_config(config).await
    }

    pub async fn with_config(config: config::Config) -> Self {
        let pods = HashMap::new();
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("Failed to create HTTP client");
        let kube_client = KubeClient::try_default().await.expect("Kube client failed");
        let pod_api: Api<Pod> = Api::namespaced(kube_client.clone(), &config.workshop_namespace);
        let svc_api: Api<Service> =
            Api::namespaced(kube_client.clone(), &config.workshop_namespace);
        Self {
            config,
            pods,
            http_client,
            pod_api,
            svc_api,
        }
    }

    pub fn guard(&self) -> impl Guard + '_ {
        self.pods.owned_guard()
    }

    /// 1. POPULATE: Syncs in-memory state with Kubernetes.
    /// Lists all pods in the namespace and ensures they exist in the HashMap.
    pub async fn populate(&self) -> Result<(), KubeError> {
        let list_params =
            ListParams::default().labels(&format!("{}={}", LABEL_MANAGED_BY, HUB_ID,));

        let k_pods =
            self.pod_api
                .list(&list_params)
                .await?;

        // Lockless iteration over K8s results
        for k_pod in k_pods.items {
            // We need to extract both labels to reconstruct the session key
            if let Some(labels) = &k_pod.metadata.labels {
                if let (Some(user_id), Some(workshop_name)) = (
                    labels.get(LABEL_USER_ID),
                    labels.get(LABEL_WORKSHOP_NAME),
                ) {
                    let session_key = format!("{}-{}", workshop_name, user_id);

                    // Update existing or insert new (Adoption)
                    self.pods.pin().update_or_insert_with(
                        session_key.clone(),
                        |mp| {
                            let mut mp = mp.clone();
                            mp.set_pod(k_pod.clone());
                            mp
                        },
                        || ManagedPod::new(k_pod.clone()),
                    );
                }
            }
        }
        Ok(())
    }

    pub async fn check_health<'guard>(
        &self,
        session_key: &str,
        guard: &'guard impl Guard,
    ) -> Result<PodStatus, SidecarError> {
        // 1. Look up managed pod
        let mp = match self.pods.get(session_key, guard) {
            Some(mp) => mp.clone(),
            None => return Err(SidecarError::Gone),
        };

        // 2. Fast path: recently verified healthy
        if let Some(status) = self.check_cached_health(&mp) {
            return Ok(status);
        }

        // 3. Perform actual health check
        // Note: ensure_pod_loaded removal - Pod is guaranteed to be in ManagedPod
        self.perform_health_check(session_key, &mp, guard).await
    }

    /// Fast path: if we checked recently and pod has an IP, skip the health call
    fn check_cached_health(&self, mp: &ManagedPod) -> Option<PodStatus> {
        const FRESHNESS_DIVISOR: i64 = 4;
        let freshness_threshold = self.config.workshop_idle_seconds / FRESHNESS_DIVISOR;

        // No need to unwrap Option<Pod>, it is guaranteed to exist
        let pod = mp.pod();

        if mp.idle() >= freshness_threshold {
            return None; // Cache is stale, need fresh check
        }

        let pod_ip = pod.status.as_ref()?.pod_ip.as_ref()?;
        let url = self.build_proxy_url(pod_ip);
        Some(PodStatus::Healthy(pod.clone(), url))
    }

/// Actually call the sidecar health endpoint and update state
    async fn perform_health_check<'guard>(
        &self,
        session_key: &str,
        mp: &ManagedPod,
        guard: &'guard impl Guard,
    ) -> Result<PodStatus, SidecarError> {
        let pod = mp.pod();

        // We now handle a Result instead of an Option
        let (health, pod_ip) = self.query_sidecar_health(session_key, pod).await?;

        // Update cached health state
        let mut updated_mp = mp.clone();
        updated_mp.set_health(health);
        self.pods
            .insert(session_key.to_string(), updated_mp.clone(), guard);

        // Check if pod has exceeded idle timeout
        if updated_mp.idle() >= self.config.workshop_idle_seconds {
            Ok(PodStatus::Old(updated_mp))
        } else {
            let url = self.build_proxy_url(&pod_ip);
            Ok(PodStatus::Healthy(pod.clone(), url))
        }
    }

    /// Query the sidecar's health endpoint.
    /// Handles recoverable errors by refreshing the pod from K8s.
    /// Returns a Result containing specific failure reasons if unrecoverable.
    async fn query_sidecar_health(
        &self,
        session_key: &str,
        initial_pod: &Pod,
    ) -> Result<(SidecarHealth, String), SidecarError> {
        
        // 1. Diagnose the pod provided (which might be stale from cache)
        let mut diagnosis = self.diagnose_pod(initial_pod);

        // 2. Handle Recoverable State: If the cached pod looks stuck or stale, refresh it.
        if matches!(
            diagnosis,
            PodDiagnosis::NoStatus | PodDiagnosis::PendingScheduling | PodDiagnosis::ContainerCreating
        ) {
            // Attempt to fetch the latest version from K8s
            match self.refresh_pod(session_key).await {
                Ok(Some(fresh_pod)) => {
                    // Re-diagnose with the fresh data
                    diagnosis = self.diagnose_pod(&fresh_pod);
                }
                Ok(None) => return Err(SidecarError::Gone),
                Err(e) => return Err(SidecarError::Internal(e.to_string())),
            }
        }

        // 3. Resolve IP or Return Unrecoverable Error
        let pod_ip = match diagnosis {
            PodDiagnosis::Ready(ip) => ip,
            // If it is still pending after a refresh, it's simply not ready yet.
            PodDiagnosis::PendingScheduling 
            | PodDiagnosis::ContainerCreating 
            | PodDiagnosis::UnknownPending 
            | PodDiagnosis::NoStatus => return Err(SidecarError::NotReady),
            
            // Unrecoverable Failures
            PodDiagnosis::ImagePullError(s) => return Err(SidecarError::K8sFailure(s)),
            PodDiagnosis::CrashLoop(s) => return Err(SidecarError::K8sFailure(s)),
            PodDiagnosis::Terminated(s) => return Err(SidecarError::K8sFailure(format!("Terminated: {}", s))),
        };

        // 4. Perform Network Request
        let url = format!(
            "http://{}:{}/health",
            pod_ip, self.config.sidecar_health_port
        );

        let resp = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(SidecarError::NetworkError)?;

        if !resp.status().is_success() {
            warn!(
                "Health check returned {} for {}",
                resp.status(),
                session_key
            );
            return Err(SidecarError::UnhealthyResponse(resp.status()));
        }

        let health = resp
            .json::<SidecarHealth>()
            .await
            .map_err(SidecarError::InvalidResponse)?;

        Ok((health, pod_ip))
    }

    /// Refreshes the internal state of a specific user's pod by fetching it from K8s.
    /// Returns the fresh Pod if found, or None if the pod has vanished.
    pub async fn refresh_pod(&self, session_key: &str) -> Result<Option<Pod>, KubeError> {
        // 1. Look up the pod name currently associated with this user
        let pod_name = {
            let guard = self.pods.guard();
            // We only need the name to query K8s
            match self.pods.get(session_key, &guard) {
                Some(mp) => mp.pod().name_any(),
                None => return Ok(None), // We aren't tracking a pod for this user
            }
        };

        // 2. Fetch the latest Pod object from Kubernetes
        match self.pod_api.get(&pod_name).await {
            Ok(k_pod) => {
                self.pods.pin().update(session_key.to_string(), |mp| {
                    let mut mp = mp.clone();
                    mp.set_pod(k_pod.clone());
                    mp
                });
                Ok(Some(k_pod))
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                self.pods.pin().remove(session_key);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn diagnose_pod(&self, pod: &Pod) -> PodDiagnosis {
        let status = match pod.status.as_ref() {
            Some(s) => s,
            None => return PodDiagnosis::NoStatus,
        };

        match status.phase.as_deref() {
            Some("Succeeded") => return PodDiagnosis::Terminated("Completed".into()),
            Some("Failed") => return PodDiagnosis::Terminated("Failed".into()),
            _ => {} // Continue checking Pending states
        }

        // B. Scan Container Statuses for specific failure reasons
        let all_statuses = status
            .init_container_statuses
            .iter()
            .flatten()
            .chain(status.container_statuses.iter().flatten());

        for container_status in all_statuses {
            if let Some(waiting) = &container_status
                .state
                .as_ref()
                .and_then(|s| s.waiting.as_ref())
            {
                let reason = waiting.reason.as_deref().unwrap_or("Unknown");
                let message = waiting.message.clone().unwrap_or_default();

                match reason {
                    "ContainerCreating" | "PodInitializing" => return PodDiagnosis::ContainerCreating,
                    "ErrImagePull" | "ImagePullBackOff" => {
                        return PodDiagnosis::ImagePullError(format!("{}: {}", reason, message));
                    }
                    "CrashLoopBackOff" => {
                        return PodDiagnosis::CrashLoop(format!("{}: {}", reason, message));
                    }
                    "InvalidImageName" => {
                        return PodDiagnosis::ImagePullError("Invalid image name".into());
                    }
                    _ => {} // Keep looking or fall through
                }
            }

            // Also check Terminated state for previous crashes
            if let Some(terminated) = &container_status
                .state
                .as_ref()
                .and_then(|s| s.terminated.as_ref())
            {
                if terminated.exit_code != 0 {
                    return PodDiagnosis::CrashLoop(format!("Exit Code {}", terminated.exit_code));
                }
            }
        }

        
        if status.host_ip.is_none() {
            return PodDiagnosis::PendingScheduling;
        }

        if let Some(pod_ip) = &status.pod_ip {
            PodDiagnosis::Ready(pod_ip.clone())
        } else {
            PodDiagnosis::UnknownPending
        }
    }

    /// Build the proxy URL from a pod IP
    fn build_proxy_url(&self, pod_ip: &str) -> String {
        format!("{}:{}", pod_ip, self.config.sidecar_proxy_port)
    }

    /// 3. DELETE: Removes K8s resources and clears the map entry.
    pub async fn delete(&self, session_key: &str) -> Result<(), HubError> {
        // Get pod name from map first
        let pod_name = {
            let guard = self.pods.pin();
            match guard.get(session_key) {
                // Direct access to pod()
                Some(mp) => Some(mp.pod().name_any()),
                None => return Ok(()), // Already gone
            }
        };

        if let Some(name) = pod_name {
            info!("Orchestrator: Deleting resources for user {}", session_key);
            let dp = DeleteParams::default();

            // Delete Service (ignore 404s)
            let _ = self.svc_api.delete(&name, &dp).await;

            // Delete Pod
            let _ = self.pod_api.delete(&name, &dp).await;
        }

        // Remove from memory
        self.pods.pin().remove(session_key);

        Ok(())
    }

    /// Retrieves an existing pod, recovers state from K8s if missing, or creates a new one.
    pub async fn get_or_create_pod(
        &self,
        user_id: &str,
        workshop_name: &str,
    ) -> Result<String, HubError> {
        // 1. FAST PATH: Check In-Memory Map
        let workshop = self.config.get_workshop(workshop_name).ok_or_else(|| {
            tracing::error!("Unknown workshop requested: {}", workshop_name);
            HubError::WorkshopNotFound
        })?;

        let session_key = format!("{}-{}", workshop.name, user_id);
        let guard = self.guard();
        match self.check_health(&session_key, &guard)
                .await {
            Ok(PodStatus::Healthy(_, url)) => return Ok(url),
            Ok(PodStatus::Old(mp)) => {
                // Old pod that wasn't cleaned up yet
                let pod = mp.pod();
                match pod.status.as_ref().and_then(|s| s.pod_ip.as_ref()) {
                    Some(pod_ip) => {
                        let url = self.build_proxy_url(pod_ip);
                        return Ok(url);
                    }
                    None => {
                        tracing::warn!("Old pod has no IP, deleting and recreating");
                        if let Err(e) = self.delete(&session_key).await {
                            tracing::error!(?e, "Failed to delete stale pod");
                        }
                    }
                }
            }
            Err(SidecarError::Gone) => {
                // We need to make the pod
                self.pods.remove(&session_key, &guard);
            }
            Err(SidecarError::NotReady) => return Err(HubError::PodNotReady),
            Err(SidecarError::K8sFailure(error)) => return Err(HubError::Error(error)),
            Err(SidecarError::Internal(error)) => return Err(HubError::Error(error)),
            Err(SidecarError::NetworkError(error)) => {
                tracing::warn!(?error, "Network error, likely cilium booting");
                return Err(HubError::PodNotReady)
            },
            Err(SidecarError::InvalidResponse(error)) => {
                tracing::error!(?error, "Pod unhealthy, deleting and trying again");
                if let Err(e) = self.delete(&session_key).await {
                    tracing::error!(?e, "Failed to delete unhealthy");
                }
            },
            Err(SidecarError::UnhealthyResponse(status)) => {
                tracing::error!(?status, "Pod unhealthy, deleting and trying again");
                if let Err(e) = self.delete(&session_key).await {
                    tracing::error!(?e, "Failed to delete stale pod");
                }
            },
        }

        // 2. SLOW PATH: Create New Pod
        // B. Check Global Limits
        // We do not need a lock as if there's an extra 1 or 2 workshops, that is fine!
        if self.pods.len() >= self.config.workshop_pod_limit {
            return Err(HubError::PodLimitReached);
        }

        // C. Create Resources
        let pod_name = format!("{}-{}", workshop_name, user_id);
        let expires_at = Utc::now().timestamp() + self.config.workshop_ttl_seconds;

        let pod_spec = definition::create_workshop_pod_spec(
            &pod_name,
            user_id,
            workshop,
            &self.config,
            expires_at,
        );
        let pod = match self
            .pod_api
            .create(&PostParams::default(), &pod_spec).await {
                Ok(pod) => pod,
                Err(kube::Error::Api(error)) if error.code == 409 => {
                    tracing::warn!(?error, "Duplicate create request");
                    return Err(HubError::PodNotReady);
                }
                Err(error) => {
                    tracing::error!(?error, "Kubernetes error while creating pod");
                    return Err(HubError::Error(format!("Kubernetes error while creating pod {}", error)))
                },
            };
        info!("Created pod {}", pod_name);

        let owner_ref = OwnerReference {
            api_version: "v1".to_string(),
            kind: "Pod".to_string(),
            name: pod_name.clone(),
            uid: pod.metadata.uid.clone().unwrap_or_default(),
            ..Default::default()
        };

        let svc_spec = definition::create_workshop_service_spec(
            &pod_name,
            &pod_name,
            user_id,
            workshop_name,
            owner_ref,
            &self.config,
        );
        if let Err(error) = self.svc_api.create(&PostParams::default(), &svc_spec).await {
            tracing::error!(?error, "Service creation failed, rolling back pod");
            if let Err(delete_err) = self.pod_api.delete(&pod_name, &DeleteParams::default()).await {
                tracing::error!(?delete_err, "Failed to rollback pod after service creation failure");
            }
            return Err(HubError::Error(format!("Service creation failed: {}", error)));
        }
        self.pods.insert(session_key, ManagedPod::new(pod), &guard);
        Err(HubError::PodNotReady)
    }


    /// 4. GARBAGE COLLECTION: Removes expired or idle pods.
    /// Implements a "Low Watermark" strategy:
    /// - Always deletes Expired (TTL) pods.
    /// - Deletes Idle pods only if capacity is > 50%.
    /// - Prioritizes deleting the *most* idle pods first.
    pub async fn gc(&self) -> Result<usize, HubError> {
        let now = Utc::now().timestamp();
        let guard = self.pods.owned_guard();
        
        // 1. Snapshot the total count once to avoid race conditions during the loop
        let initial_count = self.pods.len();
        let limit = self.config.workshop_pod_limit;
        // The target count we want to reach (e.g., 50% of limit)
        let low_watermark = limit / 2;

        // 2. SCAN: Identify candidates
        // Store (key, is_expired, idle_seconds) so we can sort
        let mut candidates = Vec::new();
        
        for (session_key, mp) in self.pods.iter(&guard) {
            let idle_seconds = mp.idle();
            let is_idle = idle_seconds > self.config.workshop_idle_seconds;

            let is_expired = mp
                .pod()
                .metadata
                .annotations
                .as_ref()
                .and_then(|ann| ann.get(TTL_ANNOTATION))
                .and_then(|t| t.parse::<i64>().ok())
                .map(|expires_at| now > expires_at)
                .unwrap_or(false);

            if is_idle || is_expired {
                candidates.push((session_key.clone(), is_expired, idle_seconds));
            }
        }

        candidates.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.cmp(&a.2))
        });

        let mut deleted_count = 0;
        let mut planned_deletions = 0;

        for (session_key, is_expired, _) in candidates {
            let current_projected_count = initial_count.saturating_sub(planned_deletions);

            let should_delete = if is_expired {
                true
            } else {
                // If it's just Idle (not expired), check capacity.
                // If we are below the watermark, we can perform "Mercy" and skip this check.
                if current_projected_count <= low_watermark {
                    info!("GC: Mercy - Keeping idle pod {} (Capacity {} <= {})", 
                        session_key, current_projected_count, low_watermark);
                    false
                } else {
                    // We are above capacity, perform health check to verify idleness
                    match self.check_health(&session_key, &guard).await {
                        Ok(PodStatus::Healthy(_, _)) => {
                            debug!("GC: Skipping {} - fresh health check shows pod is not idle", session_key);
                            false
                        }
                        Ok(PodStatus::Old(_)) => {
                            true
                        },
                        Err(SidecarError::Gone) => true,
                        Err(SidecarError::K8sFailure(e)) => {
                            warn!("GC: Deleting broken pod {}: {}", session_key, e);
                            true
                        }
                        Err(SidecarError::UnhealthyResponse(code)) => {
                            warn!("GC: Deleting unhealthy pod {} (Status: {})", session_key, code);
                            true
                        }
                        Err(e) => {
                            warn!("GC: Skipping {} - couldn't verify health ({:?}), will retry", session_key, e);
                            false
                        }
                    }
                }
            };

            if should_delete {
                planned_deletions += 1;
                
                info!("GC: Deleting {} (expired: {})", session_key, is_expired);

                if let Err(e) = self.delete(&session_key).await {
                    tracing::error!("GC: Failed to delete session for {}: {}", session_key, e);
                } else {
                    deleted_count += 1;
                }
            }
        }

        if deleted_count > 0 {
            info!("GC: Completed. Cleaned up {} sessions.", deleted_count);
        }

        Ok(deleted_count)
    }
}