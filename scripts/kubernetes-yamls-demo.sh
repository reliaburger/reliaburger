#!/usr/bin/env bash
#
# Kubernetes import/export demo: converts real-world K8s manifests
# to Reliaburger TOML and back again.
#
# No daemon needed — relish import and relish export are local commands.
#
# Usage:
#   ./scripts/kubernetes-yamls-demo.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TMPDIR="$(mktemp -d)"

cleanup() {
    rm -rf "${TMPDIR}"
}
trap cleanup EXIT

echo "=== Reliaburger Kubernetes Migration Demo ==="
echo ""

# Build
echo "--- building relish ---"
cargo build --bin relish --manifest-path "${REPO_DIR}/Cargo.toml" --quiet
RELISH="${REPO_DIR}/target/debug/relish"
echo ""

# -----------------------------------------------------------------------
# Example 1: Web app with Service and Ingress
# -----------------------------------------------------------------------

echo "=== 1. Web App: Deployment + Service + Ingress ==="
echo ""

cat > "${TMPDIR}/web-app.yaml" << 'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  labels:
    app: web
spec:
  replicas: 3
  selector:
    matchLabels:
      app: web
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxSurge: 1
      maxUnavailable: 0
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: web
        image: nginx:1.25-alpine
        ports:
        - containerPort: 80
        readinessProbe:
          httpGet:
            path: /healthz
            port: 80
          periodSeconds: 10
          failureThreshold: 3
        resources:
          limits:
            cpu: "500m"
            memory: "256Mi"
        env:
        - name: NODE_ENV
          value: production
      terminationGracePeriodSeconds: 30
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
  - port: 80
    targetPort: 80
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web-ingress
spec:
  rules:
  - host: myapp.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: web
            port:
              number: 80
  tls:
  - hosts:
    - myapp.example.com
YAML

echo "Input: Deployment + Service + Ingress (3 K8s resources)"
echo ""
echo "--- relish import -f web-app.yaml ---"
"${RELISH}" import -f "${TMPDIR}/web-app.yaml" > "${TMPDIR}/web-app.toml" 2>"${TMPDIR}/web-report.txt" || true
echo ""
echo "Generated TOML:"
cat "${TMPDIR}/web-app.toml"
echo ""
echo "Migration report:"
cat "${TMPDIR}/web-report.txt"
echo ""

# -----------------------------------------------------------------------
# Example 2: DaemonSet (monitoring agent)
# -----------------------------------------------------------------------

echo "=== 2. DaemonSet: Monitoring Agent ==="
echo ""

cat > "${TMPDIR}/monitoring.yaml" << 'YAML'
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: node-exporter
spec:
  selector:
    matchLabels:
      app: node-exporter
  template:
    metadata:
      labels:
        app: node-exporter
    spec:
      containers:
      - name: node-exporter
        image: prom/node-exporter:v1.7.0
        ports:
        - containerPort: 9100
YAML

echo "Input: DaemonSet (runs on every node)"
echo ""
echo "--- relish import -f monitoring.yaml ---"
"${RELISH}" import -f "${TMPDIR}/monitoring.yaml" > "${TMPDIR}/monitoring.toml" 2>/dev/null || true
echo ""
echo "Generated TOML (note: replicas = \"*\"):"
cat "${TMPDIR}/monitoring.toml"
echo ""

# -----------------------------------------------------------------------
# Example 3: Job + CronJob
# -----------------------------------------------------------------------

echo "=== 3. Jobs: Database Migration + Scheduled Cleanup ==="
echo ""

cat > "${TMPDIR}/jobs.yaml" << 'YAML'
apiVersion: batch/v1
kind: Job
metadata:
  name: db-migrate
spec:
  template:
    spec:
      containers:
      - name: migrate
        image: myapp:v1
        command: ["npm", "run", "migrate"]
      restartPolicy: Never
  backoffLimit: 3
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: cleanup
spec:
  schedule: "0 3 * * *"
  jobTemplate:
    spec:
      template:
        spec:
          containers:
          - name: cleanup
            image: cleanup:latest
            command: ["./cleanup.sh"]
          restartPolicy: Never
YAML

echo "Input: Job + CronJob (2 resources)"
echo ""
echo "--- relish import -f jobs.yaml ---"
"${RELISH}" import -f "${TMPDIR}/jobs.yaml" > "${TMPDIR}/jobs.toml" 2>/dev/null || true
echo ""
echo "Generated TOML:"
cat "${TMPDIR}/jobs.toml"
echo ""

# -----------------------------------------------------------------------
# Example 4: Deployment with HPA (autoscaling)
# -----------------------------------------------------------------------

echo "=== 4. Autoscaled API: Deployment + HPA ==="
echo ""

cat > "${TMPDIR}/api.yaml" << 'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
spec:
  replicas: 3
  selector:
    matchLabels:
      app: api
  template:
    metadata:
      labels:
        app: api
    spec:
      containers:
      - name: api
        image: api-server:v2.1
        ports:
        - containerPort: 8080
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: api
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: api
  minReplicas: 2
  maxReplicas: 20
  metrics:
  - type: Resource
    resource:
      name: cpu
      target:
        type: Utilization
        averageUtilization: 70
YAML

echo "Input: Deployment + HPA (autoscaled to 2-20 replicas)"
echo ""
echo "--- relish import -f api.yaml ---"
"${RELISH}" import -f "${TMPDIR}/api.yaml" > "${TMPDIR}/api.toml" 2>/dev/null || true
echo ""
echo "Generated TOML (note: [autoscale] section):"
cat "${TMPDIR}/api.toml"
echo ""

# -----------------------------------------------------------------------
# Example 5: Round-trip — export back to K8s YAML
# -----------------------------------------------------------------------

echo "=== 5. Round Trip: TOML → K8s YAML ==="
echo ""

# Use the web app TOML we just generated
echo "--- relish export -f web-app.toml ---"
"${RELISH}" export -f "${TMPDIR}/web-app.toml" > "${TMPDIR}/web-app-exported.yaml" 2>/dev/null || true
echo ""
echo "Exported K8s YAML:"
cat "${TMPDIR}/web-app-exported.yaml"
echo ""

# -----------------------------------------------------------------------
# Example 6: Unknown resources are reported
# -----------------------------------------------------------------------

echo "=== 6. Unknown Resources: Handled Gracefully ==="
echo ""

cat > "${TMPDIR}/mixed.yaml" << 'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: worker
spec:
  replicas: 2
  selector:
    matchLabels:
      app: worker
  template:
    metadata:
      labels:
        app: worker
    spec:
      containers:
      - name: worker
        image: worker:v1
---
apiVersion: custom.example.com/v1
kind: MyCustomResource
metadata:
  name: custom-thing
spec:
  foo: bar
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: worker-sa
YAML

echo "Input: Deployment + CRD + ServiceAccount"
echo ""
echo "--- relish import -f mixed.yaml ---"
"${RELISH}" import -f "${TMPDIR}/mixed.yaml" 2>&1 || true
echo ""

echo "=== Demo Complete ==="
echo ""
echo "Summary:"
echo "  - Deployments, DaemonSets, Services, Ingress → [app.*] sections"
echo "  - Jobs, CronJobs → [job.*] sections"
echo "  - HPAs → [app.*.autoscale] sections"
echo "  - Unknown resources reported in migration report"
echo "  - Round-trip: TOML → K8s YAML preserves key fields"
