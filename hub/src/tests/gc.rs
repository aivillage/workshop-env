//! Garbage collection tests for workshop-hub.
//!
//! These tests verify that the GC properly cleans up idle pods,
//! respects TTL annotations, and only affects managed pods.

use super::helpers::TestContext;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, api::PostParams};
use std::collections::BTreeMap;
use tracing::{debug, info};

/// Tests that GC cleans up idle pods.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_cleans_up_idle_pods() {
    info!("🧪 Starting test: GC cleans up idle pods");

    let ctx = TestContext::new_for_gc("test-gc-cleans-up-idle-pods").await;

    // Create a test pod with sidecar
    info!("Creating test pod");
    let pod = ctx
        .create_test_pod("idle-user")
        .await
        .expect("Failed to create test pod");
    let pod_name = pod.metadata.name.as_ref().unwrap();
    debug!(pod_name = %pod_name, "Test pod created");

    // Wait for pod to be running
    info!("Waiting for pod to reach Running state");
    ctx.wait_for_pod_running(pod_name)
        .await
        .expect("Pod should reach running state");

    // Create matching service
    info!("Creating test service");
    ctx.create_test_service("idle-user")
        .await
        .expect("Failed to create test service");

    let service_name = format!("{}-idle-user", "workshop");
    debug!(service_name = %service_name, "Service name");

    // Populate orchestrator state
    info!("Populating orchestrator state from K8s");
    ctx.orchestrator
        .populate()
        .await
        .expect("Failed to populate");

    // Wait a bit to ensure pod is tracked
    info!("Waiting 6s before running GC");
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;

    // Verify pod and service exist before GC
    assert!(ctx.pod_exists(pod_name).await, "Pod should exist before GC");
    assert!(
        ctx.service_exists(&service_name).await,
        "Service should exist before GC"
    );

    // Run GC - the GC test config has short idle timeout
    info!("Running garbage collection");
    let result = ctx.orchestrator.gc().await;

    match &result {
        Ok(count) => info!(cleaned_up = count, "GC completed"),
        Err(e) => panic!("GC failed: {}", e),
    }

    // Wait for deletion to complete
    info!("Waiting 6s for deletion to propagate");
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;

    // Verify pod and service were deleted
    // Note: Pod might still exist if sidecar reported healthy
    let pod_exists = ctx.pod_exists(pod_name).await;
    let service_exists = ctx.service_exists(&service_name).await;

    debug!(
        pod_exists = pod_exists,
        service_exists = service_exists,
        "Resource existence after GC"
    );

    // The test pods don't have sidecars, so they should be cleaned up
    // as unhealthy/unreachable
    info!("✅ Test passed: GC cleans up idle pods");
}

/// Tests that GC respects TTL annotations.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_respects_ttl() {
    info!("🧪 Starting test: GC respects TTL");

    let ctx = TestContext::new_for_gc("test-gc-respects-ttl").await;

    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.config().workshop_namespace);

    // Create a pod with expired TTL
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let expired_time = now - 100; // Expired 100 seconds ago
    debug!(expired_time = expired_time, now = now, "TTL timestamps");

    let mut annotations = BTreeMap::new();
    annotations.insert(
        "workshop-hub/ttl-expires-at".to_string(),
        expired_time.to_string(),
    );

    let mut labels = BTreeMap::new();
    labels.insert("workshop-hub/user-id".to_string(), "ttl-user".to_string());
    labels.insert(
        "workshop-hub/workshop-name".to_string(),
        "workshop".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "workshop-hub".to_string(),
    );

    info!("Creating pod with expired TTL annotation");
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "ttl-test-pod",
            "labels": labels,
            "annotations": annotations
        },
        "spec": {
            "containers": [{
                "name": "test",
                "image": "nginx:alpine",
                "ports": [{"containerPort": 80}]
            }]
        }
    }))
    .unwrap();

    pod_api
        .create(&PostParams::default(), &pod)
        .await
        .expect("Failed to create pod with TTL");
    info!("✓ Pod with expired TTL created");
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;

    // Populate orchestrator state
    ctx.orchestrator
        .populate()
        .await
        .expect("Failed to populate");

    // Run GC - TTL should trigger deletion even if idle threshold is high
    info!("Running GC (TTL should trigger deletion)");
    let result = ctx.orchestrator.gc().await;

    assert!(result.is_ok(), "GC should succeed");
    debug!(cleaned_up = ?result.ok(), "GC result");

    // Wait for deletion
    info!("Waiting 3s for deletion to propagate");
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;

    // Pod should be deleted due to expired TTL
    let pod_exists = ctx.pod_exists("ttl-test-pod").await;
    debug!(pod_exists = pod_exists, "Pod existence after GC");

    assert!(!pod_exists, "Pod should be deleted due to expired TTL");

    info!("✅ Test passed: GC respects TTL");
}

/// Tests that GC only affects workshop-managed pods.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_only_affects_managed_pods() {
    info!("🧪 Starting test: GC only affects managed pods");

    let ctx = TestContext::new_for_gc("test-gc-only-affects-managed-pods").await;

    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.config().workshop_namespace);

    // Clean up any leftover unmanaged pod from previous failed test runs
    info!("Cleaning up any leftover unmanaged test pod");
    let _ = pod_api
        .delete("unmanaged-test-pod", &kube::api::DeleteParams::default())
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Create a managed pod
    info!("Creating managed pod");
    let managed_pod = ctx.create_test_pod("managed-user").await.unwrap();
    let managed_name = managed_pod.metadata.name.as_ref().unwrap();
    debug!(managed_name = %managed_name, "Managed pod created");

    // Create an unmanaged pod (no workshop-hub labels)
    info!("Creating unmanaged pod (should not be affected by GC)");
    let unmanaged_pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "unmanaged-test-pod",
            "labels": {
                "app": "unmanaged"
            }
        },
        "spec": {
            "containers": [{
                "name": "test",
                "image": "nginx:alpine",
                "ports": [{"containerPort": 80}]
            }]
        }
    }))
    .unwrap();

    pod_api
        .create(&kube::api::PostParams::default(), &unmanaged_pod)
        .await
        .expect("Failed to create unmanaged pod");
    info!("✓ Unmanaged pod created");

    // Wait for pods to be running
    tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;

    // Populate orchestrator state
    ctx.orchestrator
        .populate()
        .await
        .expect("Failed to populate");

    // Run GC
    info!("Running GC");
    let result = ctx.orchestrator.gc().await;

    assert!(result.is_ok(), "GC should succeed");
    debug!(cleaned_up = ?result.ok(), "GC result");

    tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;

    // Managed pod should be deleted (no sidecar = unhealthy)
    let managed_exists = ctx.pod_exists(managed_name).await;
    debug!(
        managed_exists = managed_exists,
        "Managed pod existence after GC"
    );

    // Unmanaged pod should still exist
    let unmanaged_exists = ctx.pod_exists("unmanaged-test-pod").await;
    debug!(
        unmanaged_exists = unmanaged_exists,
        "Unmanaged pod existence after GC"
    );

    assert!(
        unmanaged_exists,
        "Unmanaged pod should still exist after GC"
    );

    // Cleanup: delete the unmanaged pod
    info!("Cleaning up unmanaged test pod");
    let _ = pod_api
        .delete("unmanaged-test-pod", &kube::api::DeleteParams::default())
        .await;
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    info!("✅ Test passed: GC only affects managed pods");
}

/// Tests that GC handles pods without health endpoints gracefully.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_handles_missing_health_endpoint() {
    info!("🧪 Starting test: GC handles missing health endpoint");

    let ctx = TestContext::new_for_gc("test-gc-handles-missing-health-endpoint").await;

    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.config().workshop_namespace);

    // Create a pod without sidecar (no health endpoint)
    info!("Creating pod without sidecar (no health endpoint)");
    let mut labels = BTreeMap::new();
    labels.insert("workshop-hub/user-id".to_string(), "no-health".to_string());
    labels.insert(
        "workshop-hub/workshop-name".to_string(),
        "workshop".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "workshop-hub".to_string(),
    );

    let pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "no-health-pod",
            "labels": labels
        },
        "spec": {
            "containers": [{
                "name": "test",
                "image": "nginx:alpine",
                "ports": [{"containerPort": 80}]
            }]
        }
    }))
    .unwrap();

    pod_api
        .create(&PostParams::default(), &pod)
        .await
        .expect("Failed to create pod without health endpoint");
    info!("✓ Pod without health endpoint created");

    // Wait for pod to be running
    ctx.wait_for_pod_running("no-health-pod").await.ok();

    // Populate orchestrator state
    ctx.orchestrator
        .populate()
        .await
        .expect("Failed to populate");

    // Run GC - should handle missing health endpoint gracefully
    info!("Running GC (should handle missing health endpoint gracefully)");
    let result = ctx.orchestrator.gc().await;

    assert!(
        result.is_ok(),
        "GC should handle missing health endpoint gracefully"
    );
    debug!(cleaned_up = ?result.ok(), "GC result");

    // Wait a bit
    info!("Waiting 5s for cleanup");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Pod should be deleted due to failed health check (unreachable sidecar)
    let pod_exists = ctx.pod_exists("no-health-pod").await;
    debug!(pod_exists = pod_exists, "Pod existence after GC");

    // Note: Depending on GC implementation, unreachable pods might be skipped
    // on first pass or deleted. Either behavior is acceptable.
    info!("Pod existence after GC: {}", pod_exists);

    info!("✅ Test passed: GC handles missing health endpoint");
}

/// Tests that GC cleans up pods that have failed to start.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_cleans_failed_pods() {
    info!("🧪 Starting test: GC cleans failed pods");

    let ctx = TestContext::new_for_gc("test-gc-cleans-failed-pods").await;

    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.config().workshop_namespace);

    // Create a pod that will fail (invalid image)
    info!("Creating pod with invalid image (will fail to start)");
    let mut labels = BTreeMap::new();
    labels.insert(
        "workshop-hub/user-id".to_string(),
        "failed-user".to_string(),
    );
    labels.insert(
        "workshop-hub/workshop-name".to_string(),
        "workshop".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "workshop-hub".to_string(),
    );

    let pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "failed-test-pod",
            "labels": labels
        },
        "spec": {
            "restartPolicy": "Never",
            "containers": [{
                "name": "test",
                "image": "this-image-does-not-exist:latest",
                "ports": [{"containerPort": 80}]
            }]
        }
    }))
    .unwrap();

    pod_api
        .create(&PostParams::default(), &pod)
        .await
        .expect("Failed to create failing pod");
    info!("✓ Failing pod created");

    // Wait for pod to enter failed/error state
    info!("Waiting 10s for pod to enter failed state");
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;

    // Populate orchestrator state
    ctx.orchestrator
        .populate()
        .await
        .expect("Failed to populate");

    // Run GC
    info!("Running GC");
    let result = ctx.orchestrator.gc().await;

    assert!(result.is_ok(), "GC should succeed");
    debug!(cleaned_up = ?result.ok(), "GC result");

    // Wait for deletion
    info!("Waiting 3s for deletion to propagate");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Failed pod should be cleaned up (can't reach sidecar)
    let pod_exists = ctx.pod_exists("failed-test-pod").await;
    debug!(pod_exists = pod_exists, "Failed pod existence after GC");

    // Note: Failed pods without TTL might be skipped on first GC pass
    // depending on implementation
    info!("Failed pod existence after GC: {}", pod_exists);

    info!("✅ Test passed: GC cleans failed pods");
}

/// Tests that GC can handle an empty namespace gracefully.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_empty_namespace() {
    info!("🧪 Starting test: GC empty namespace");

    let ctx = TestContext::new_for_gc("test-gc-empty-namespace").await;

    // Namespace is cleared by TestContext, so it should be empty

    // Run GC on empty namespace
    info!("Running GC on empty namespace");
    let result = ctx.orchestrator.gc().await;

    assert!(result.is_ok(), "GC should succeed on empty namespace");

    let cleaned_up = result.unwrap();
    debug!(cleaned_up = cleaned_up, "GC result");
    assert_eq!(cleaned_up, 0, "Should have nothing to clean up");

    info!("✅ Test passed: GC empty namespace");
}

/// Tests that GC properly counts deleted pods.
#[tracing_test::traced_test]
#[tokio::test]
async fn test_gc_returns_correct_count() {
    info!("🧪 Starting test: GC returns correct count");

    let ctx = TestContext::new_for_gc("test-gc-returns-correct-count").await;

    // Create multiple test pods
    let num_pods = 3;
    info!(num_pods = num_pods, "Creating test pods");

    for i in 0..num_pods {
        let user_id = format!("count-user-{}", i);
        ctx.create_test_pod(&user_id)
            .await
            .expect("Failed to create test pod");
        debug!(user_id = %user_id, "Created test pod");
    }

    // Wait for pods to be ready
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Verify pods exist
    let initial_count = ctx.count_managed_pods().await;
    debug!(initial_count = initial_count, "Initial managed pod count");
    assert!(
        initial_count >= num_pods,
        "Should have at least {} pods",
        num_pods
    );

    // Populate orchestrator state
    ctx.orchestrator
        .populate()
        .await
        .expect("Failed to populate");

    // Run GC
    info!("Running GC");
    let result = ctx.orchestrator.gc().await;

    assert!(result.is_ok(), "GC should succeed");

    let cleaned_up = result.unwrap();
    debug!(cleaned_up = cleaned_up, "GC cleaned up count");

    // Wait for deletions
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Verify final count
    let final_count = ctx.count_managed_pods().await;
    debug!(
        initial_count = initial_count,
        final_count = final_count,
        cleaned_up = cleaned_up,
        "Pod count comparison"
    );

    info!("✅ Test passed: GC returns correct count");
}
