# workshop-env

`workshop-env` is the infrastructure runtime engine for hosting hands-on security workshops. It consists of two decoupled Rust services:

1. **`workshop-hub`**: A dynamic Layer 7 reverse proxy (Cloudflare Pingora) and Kubernetes custom controller. It authenticates attendees, dynamically provisions on-demand challenge pods, and garbage-collects idle environments.
2. **`workshop-sidecar`**: A lightweight Layer 4 TCP proxy injected into each attendee pod. It monitors network activity and serves health metrics transparently without requiring modifications to workshop challenge containers.

---

## Architecture Overview

```text
[Attendee Browser] 
        |
        v  (e.g. http://yolo-l2.workshop.example.org)
[workshop-hub (Pingora on :8080)]
        |-- 1. Validates JWT cookie ("workshop_token")
        |-- 2. Extracts challenge slug ("yolo-l2") against BASE_DOMAIN
        |-- 3. Queries Kubernetes API for user pod ("yolo-l2-user-<id>")
        |-- 4. Dynamically spawns Pod (Challenge + Sidecar) & Service if absent
        |-- 5. Probes Sidecar health endpoint (http://{pod_ip}:9000/health)
        `-- 6. Proxies connection to Sidecar ({pod_ip}:8888)
                    |
                    v
          [workshop-sidecar (in Pod)]
                    |-- Layer 4 transparent bi-directional TCP stream
                    |-- Updates atomic last_activity timestamp on byte transfer
                    `-- Forwards to Challenge Container (127.0.0.1:{target_port})
```

---

## Deployment on Kubernetes

`workshop-hub` operates as an active Kubernetes controller. When deploying to any standard Kubernetes cluster (EKS, GKE, Talos, k3s, minikube), it requires four core resources:

1. **RBAC**: A `ServiceAccount`, `ClusterRole`, and `RoleBinding` granting permissions to create, monitor, and delete pods and services in the workshop namespace.
2. **ConfigMap**: Holds the application `config.yaml` specifying available challenges, timeouts, and images.
3. **Deployment**: Runs `workshop-hub`, mounted with the `ConfigMap` at `/etc/workshop/config.yaml`.
4. **Service**: Exposes port 8080 (via LoadBalancer, NodePort, or Ingress) to route incoming wildcard domain traffic (`*.BASE_DOMAIN`) to the Hub.

### Quickstart

Apply the complete standalone example manifest:

```bash
kubectl apply -f examples/kubernetes/hub.yaml
```

---

## Hub Application Configuration (`config.yaml`)

The Hub reads its configuration on startup. By default, it searches:
1. Path specified by `$WORKSHOP_CONFIG`
2. `./workshop.yaml`
3. `./examples/config.yaml`
4. `/app/config/workshop.yaml`
5. `/etc/workshop/config.yaml` (default Kubernetes mount)

### Example `config.yaml`

```yaml
# Base domain used for subdomain routing and wildcard cookie scoping
base_domain: "workshop.example.org"

# Container image injected as the sidecar proxy in attendee pods
sidecar_image: "ghcr.io/aivillage/workshop-sidecar:v0.1.0"

# Target namespace where user pods and services are created
workshop_namespace: "workshop"

# Timeouts (in seconds)
workshop_ttl_seconds: 28800      # Hard expiration: 8 hours
workshop_idle_seconds: 3600       # Idle expiration: 1 hour
garbage_collection_seconds: 300   # GC sweep interval: 5 minutes

# In-pod network ports
sidecar_proxy_port: 8888
sidecar_health_port: 9000

# Concurrency and resource limits
workshop_pod_limit: 50
workshop_cpu_request: "250m"
workshop_cpu_limit: "1000m"
workshop_mem_request: "512Mi"
workshop_mem_limit: "2Gi"

# Declared workshop challenges
workshops:
  - name: "yolo-l2"
    image: "ghcr.io/aivillage/workshop-yolo-l2-notebook:latest"
    description: "YOLO L2 distance adversarial attack challenge"
    port: 8888
    env:
      JUPYTER_TOKEN: "aivillage"

  - name: "email-indirect"
    image: "ghcr.io/aivillage/workshop-email-indirect-user:latest"
    description: "Indirect prompt injection challenge"
    port: 5000
```

### Hub Environment Variables

| Variable | Description | Required? |
| :--- | :--- | :--- |
| `BASE_DOMAIN` | Base domain for routing and auth cookie scoping (e.g. `workshop.example.org`). Overrides `base_domain` in YAML if present. | Yes (in YAML or Env) |
| `SIDECAR_IMAGE` | Container image for attendee pod proxy. Overrides `sidecar_image` in YAML if present. | No (defaults to `ghcr.io/aivillage/workshop-sidecar:latest`) |
| `WORKSHOP_CONFIG` | Path to custom YAML configuration file. | No |
| `KUBECONFIG` | Path to cluster config when running out-of-cluster during development. | No (uses in-cluster service account by default) |
| `RUST_LOG` | Logging filter (e.g. `info`, `hub=debug`). | No |

---

## workshop-sidecar

The sidecar is a pure Tokio TCP proxy and Axum HTTP server with zero Kubernetes dependencies. It is dynamically injected into attendee pods by `workshop-hub`.

### Sidecar Environment Variables

| Variable | Description | Example |
| :--- | :--- | :--- |
| `SIDECAR_HTTP_LISTEN` | Bind address for the `/health` endpoint. | `0.0.0.0:9000` |
| `SIDECAR_TCP_LISTEN` | Bind address for the client-facing TCP proxy. | `0.0.0.0:8888` |
| `SIDECAR_TARGET_TCP` | Local backend address of the challenge container. | `127.0.0.1:5000` |
| `SIDECAR_TARGET_UDS` | Unix domain socket path of the backend (alternative to TCP). | `/var/run/app.sock` |

Exactly one target (`SIDECAR_TARGET_TCP` or `SIDECAR_TARGET_UDS`) must be set.

---

## Local Development

### Testing the Rust Crates

```bash
# Test Hub routing and subdomain extraction logic (unit tests)
cargo test -p hub

# Run Hub cluster integration tests (requires active Talos cluster and KUBECONFIG)
cargo test -p hub -- --ignored

# Test Sidecar proxy bidirectional copying and activity tracking
cargo test -p sidecar
```

### Running the Sidecar Locally (No Kubernetes Required)

You can run the sidecar directly on your machine against any local service:

```bash
# Terminal 1: Run any local service
python3 -m http.server 5000

# Terminal 2: Run sidecar proxying to port 5000
SIDECAR_HTTP_LISTEN="127.0.0.1:9000" \
SIDECAR_TCP_LISTEN="127.0.0.1:8888" \
SIDECAR_TARGET_TCP="127.0.0.1:5000" \
cargo run -p sidecar
```

Connecting to `http://localhost:8888` streams data to the Python server and tracks byte activity. `http://localhost:9000/health` reports the current idle duration.
