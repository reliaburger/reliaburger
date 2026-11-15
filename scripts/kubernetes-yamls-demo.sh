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

# Colours (disabled when piped)
if [[ -t 1 ]]; then
    BOLD='\033[1m'
    CYAN='\033[36m'
    GREEN='\033[32m'
    YELLOW='\033[33m'
    DIM='\033[2m'
    RESET='\033[0m'
else
    BOLD='' CYAN='' GREEN='' YELLOW='' DIM='' RESET=''
fi

section() { echo -e "\n${BOLD}${CYAN}=== $1 ===${RESET}\n"; }
cmd()     { echo -e "${GREEN}--- $1 ---${RESET}"; }
note()    { echo -e "${DIM}$1${RESET}"; }
warn()    { echo -e "${YELLOW}$1${RESET}"; }

cleanup() {
    rm -rf "${TMPDIR}"
}
trap cleanup EXIT

# Every step asserts what it printed, so a regression fails the demo instead
# of scrolling past (#703).
fail() { echo -e "${YELLOW}FAIL: $1${RESET}" >&2; exit 1; }
expect() {   # expect FILE TEXT: FILE must contain TEXT
    grep -qF -- "$2" "$1" || fail "$(basename "$1") lacks: $2"
}
expect_not() {   # expect_not FILE TEXT: FILE must not contain TEXT
    if grep -qF -- "$2" "$1"; then fail "$(basename "$1") unexpectedly has: $2"; fi
}

section "Reliaburger Kubernetes Migration Demo"

# Build
cmd "building relish"
cargo build --bin relish --manifest-path "${REPO_DIR}/Cargo.toml" --quiet
RELISH="${REPO_DIR}/target/debug/relish"

# -----------------------------------------------------------------------
# Example 1: Web app with Service and Ingress
# -----------------------------------------------------------------------

section "1. Web App: Deployment + Service + Ingress"

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

note "Input YAML:"
cat "${TMPDIR}/web-app.yaml"
echo ""
cmd "relish import -f web-app.yaml"
"${RELISH}" import -f "${TMPDIR}/web-app.yaml" > "${TMPDIR}/web-app.toml" 2>"${TMPDIR}/web-report.txt" \
    || fail "import of web-app.yaml failed"
echo ""
note "Generated TOML:"
cat "${TMPDIR}/web-app.toml"
echo ""
warn "Migration report:"
cat "${TMPDIR}/web-report.txt"
expect "${TMPDIR}/web-app.toml" '[app.web]'
expect "${TMPDIR}/web-app.toml" 'image = "nginx:1.25-alpine"'
expect "${TMPDIR}/web-app.toml" 'host = "myapp.example.com"'
expect "${TMPDIR}/web-app.toml" 'path = "/healthz"'
expect "${TMPDIR}/web-report.txt" 'Deployment/web → [app.web]'
# The Service selects the Deployment by label, so it attaches silently.
expect_not "${TMPDIR}/web-report.txt" 'Service/web'

# -----------------------------------------------------------------------
# Example 2: DaemonSet (monitoring agent)
# -----------------------------------------------------------------------

section "2. DaemonSet: Monitoring Agent"

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

note "Input YAML (DaemonSet — runs on every node):"
cat "${TMPDIR}/monitoring.yaml"
echo ""
cmd "relish import -f monitoring.yaml"
"${RELISH}" import -f "${TMPDIR}/monitoring.yaml" > "${TMPDIR}/monitoring.toml" 2>/dev/null \
    || fail "import of monitoring.yaml failed"
echo ""
note "Generated TOML (note: replicas = \"*\"):"
cat "${TMPDIR}/monitoring.toml"
expect "${TMPDIR}/monitoring.toml" '[app.node-exporter]'
expect "${TMPDIR}/monitoring.toml" 'replicas = "*"'

# -----------------------------------------------------------------------
# Example 3: Job + CronJob
# -----------------------------------------------------------------------

section "3. Jobs: Database Migration + Scheduled Cleanup"

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

note "Input YAML (Job + CronJob):"
cat "${TMPDIR}/jobs.yaml"
echo ""
cmd "relish import -f jobs.yaml"
"${RELISH}" import -f "${TMPDIR}/jobs.yaml" > "${TMPDIR}/jobs.toml" 2>/dev/null \
    || fail "import of jobs.yaml failed"
echo ""
note "Generated TOML (note: backoffLimit 3 becomes max_attempts = 4):"
cat "${TMPDIR}/jobs.toml"
expect "${TMPDIR}/jobs.toml" '[job.db-migrate]'
expect "${TMPDIR}/jobs.toml" 'max_attempts = 4'
expect "${TMPDIR}/jobs.toml" '[job.cleanup]'
expect "${TMPDIR}/jobs.toml" 'schedule = "0 3 * * *"'

# -----------------------------------------------------------------------
# Example 4: Deployment with HPA (autoscaling)
# -----------------------------------------------------------------------

section "4. Autoscaled API: Deployment + HPA"

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

note "Input YAML (Deployment + HPA):"
cat "${TMPDIR}/api.yaml"
echo ""
cmd "relish import -f api.yaml"
"${RELISH}" import -f "${TMPDIR}/api.yaml" > "${TMPDIR}/api.toml" 2>/dev/null \
    || fail "import of api.yaml failed"
echo ""
note "Generated TOML (note: [autoscale] section):"
cat "${TMPDIR}/api.toml"
expect "${TMPDIR}/api.toml" '[app.api.autoscale]'
expect "${TMPDIR}/api.toml" 'max = 20'

# -----------------------------------------------------------------------
# Example 5: Round-trip — export back to K8s YAML
# -----------------------------------------------------------------------

section "5. Round Trip: TOML → K8s YAML"

cmd "relish export -f web-app.toml"
"${RELISH}" export -f "${TMPDIR}/web-app.toml" > "${TMPDIR}/web-app-exported.yaml" 2>/dev/null \
    || fail "export of web-app.toml failed"
echo ""
note "Exported K8s YAML:"
cat "${TMPDIR}/web-app-exported.yaml"
expect "${TMPDIR}/web-app-exported.yaml" 'kind: Deployment'
expect "${TMPDIR}/web-app-exported.yaml" 'kind: Service'
expect "${TMPDIR}/web-app-exported.yaml" 'kind: Ingress'
expect "${TMPDIR}/web-app-exported.yaml" 'image: nginx:1.25-alpine'
expect "${TMPDIR}/web-app-exported.yaml" 'NODE_ENV'

# -----------------------------------------------------------------------
# Example 6: Services attach by label selector, not by name
# -----------------------------------------------------------------------

section "6. Label Selectors: A Service Not Named After Its Deployment"

cat > "${TMPDIR}/chart.yaml" << 'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: shop
spec:
  selector:
    matchLabels:
      app.kubernetes.io/name: shop
  template:
    metadata:
      labels:
        app.kubernetes.io/name: shop
    spec:
      containers:
      - name: shop
        image: shop:v1
        ports:
        - containerPort: 8080
        - containerPort: 9090
---
apiVersion: v1
kind: Service
metadata:
  name: release-shop-http
spec:
  selector:
    app.kubernetes.io/name: shop
  ports:
  - port: 9090
YAML

note "Input YAML (Helm-style Service name):"
cat "${TMPDIR}/chart.yaml"
echo ""
cmd "relish import -f chart.yaml"
"${RELISH}" import -f "${TMPDIR}/chart.yaml" > "${TMPDIR}/chart.toml" 2>"${TMPDIR}/chart-report.txt" \
    || fail "import of chart.yaml failed"
cat "${TMPDIR}/chart.toml"
warn "Migration report:"
cat "${TMPDIR}/chart-report.txt"
expect "${TMPDIR}/chart.toml" 'port = 9090'
expect "${TMPDIR}/chart-report.txt" 'clients must connect to shop:9090'

# -----------------------------------------------------------------------
# Example 7: Unknown resources are reported
# -----------------------------------------------------------------------

section "7. Unknown Resources: Reported, and Fatal Under --strict"

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

note "Input YAML (Deployment + CRD + ServiceAccount):"
cat "${TMPDIR}/mixed.yaml"
echo ""
cmd "relish import -f mixed.yaml"
"${RELISH}" import -f "${TMPDIR}/mixed.yaml" > "${TMPDIR}/mixed.toml" 2>"${TMPDIR}/mixed-report.txt" \
    || fail "import of mixed.yaml failed"
cat "${TMPDIR}/mixed-report.txt"
expect "${TMPDIR}/mixed-report.txt" 'MyCustomResource/custom-thing'
expect "${TMPDIR}/mixed-report.txt" 'ServiceAccount/worker-sa'
expect "${TMPDIR}/mixed.toml" '[app.worker]'
echo ""
cmd "relish import --strict -f mixed.yaml"
if "${RELISH}" import --strict -f "${TMPDIR}/mixed.yaml" > /dev/null 2>"${TMPDIR}/strict.txt"; then
    fail "--strict passed although two resources were dropped"
fi
cat "${TMPDIR}/strict.txt"
expect "${TMPDIR}/strict.txt" '2 dropped resource(s)'

section "Demo Complete"

echo "  - Deployments, DaemonSets, Services (by label selector), Ingress → [app.*] sections"
echo "  - Jobs, CronJobs → [job.*] sections"
echo "  - HPAs → [app.*.autoscale] sections"
echo "  - Unknown resources reported in migration report; --strict fails on them"
echo "  - Round-trip: TOML → K8s YAML preserves key fields"
echo ""
