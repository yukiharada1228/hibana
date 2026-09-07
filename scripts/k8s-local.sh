#!/usr/bin/env bash
# Isolated, disposable Kubernetes acceptance environment. Does not use current-context.
set -euo pipefail
cd "$(dirname "$0")/.."
state="$PWD/.local/kubernetes"
kind_bin="${KIND_BIN:-kind}"
if ! command -v "$kind_bin" >/dev/null 2>&1 && [ -x .local/bin/kind ] && [ -z "${KIND_BIN:-}" ]; then
  kind_bin="$PWD/.local/bin/kind"
fi
cluster=hibana-dev
action="${1:-up}"
umask 077
mkdir -p "$state"
kube() { kubectl --kubeconfig "$state/kubeconfig" --context "kind-$cluster" "$@"; }

case "$action" in
  up)
    if "$kind_bin" get clusters | rg -qx "$cluster"; then
      test -f "$state/kubeconfig" || { echo "Existing hibana-dev is not owned by this checkout; refusing." >&2; exit 1; }
    else
      "$kind_bin" create cluster --name "$cluster" --config deploy/kubernetes/local/kind.yaml --kubeconfig "$state/kubeconfig" --wait 120s
    fi
    docker build -t hibana-platform:dev .
    "$kind_bin" load docker-image hibana-platform:dev --name "$cluster"
    # Apply only the namespace first, so credentials and migrations precede app startup.
    kube apply -f - <<'YAML'
apiVersion: v1
kind: Namespace
metadata:
  name: hibana
  labels:
    app.kubernetes.io/part-of: hibana
    pod-security.kubernetes.io/enforce: baseline
    pod-security.kubernetes.io/enforce-version: v1.35
YAML
    python3 scripts/k8s-local-secrets.py "$state"
    kube apply -f "$state/secrets.json"
    kube apply -k deploy/kubernetes/local/dependencies
    for dep in postgres redis minio; do
      kube -n hibana rollout status "deployment/hibana-$dep" --timeout=180s
    done
    kube -n hibana wait --for=condition=complete job/hibana-local-bucket --timeout=180s
    # Completed Jobs are immutable; rerun migrations for this image, never kill a running migration.
    if kube -n hibana get job hibana-migrate >/dev/null 2>&1; then
      succeeded="$(kube -n hibana get job hibana-migrate -o jsonpath='{.status.succeeded}')"
      test "$succeeded" = 1 || { echo "Migration exists and has not succeeded; inspect it before retrying." >&2; exit 1; }
      kube -n hibana delete job hibana-migrate
    fi
    kube apply -k deploy/kubernetes/local/migration
    kube -n hibana wait --for=condition=complete job/hibana-migrate --timeout=600s
    kube apply -k deploy/kubernetes/local
    # Remove retired compiler resources in this explicitly owned development cluster.
    kube -n hibana delete deployment,service,poddisruptionbudget,networkpolicy hibana-compiler --ignore-not-found
    kube -n hibana delete networkpolicy hibana-control-plane-compiler --ignore-not-found
    # The dev tag is intentionally reusable; restart so repeated runs pick up the freshly loaded image.
    kube -n hibana rollout restart deployment/hibana-control-plane deployment/hibana-worker
    kube -n hibana rollout status deployment/hibana-control-plane --timeout=300s
    kube -n hibana rollout status deployment/hibana-worker --timeout=300s
    # Retired messaging is removed only in this owned development cluster; PVCs are retained.
    kube -n hibana delete deployment,service hibana-nats --ignore-not-found
    printf 'Ready. Run: bash scripts/k8s-local.sh forward\nCredentials: .local/kubernetes/sdk.env\n'
    ;;
  forward)
    kube -n hibana port-forward --address 127.0.0.1 service/hibana-apps 18084:8083 &
    apps_forward=$!
    trap 'kill "$apps_forward" 2>/dev/null || true' EXIT
    kube -n hibana port-forward --address 127.0.0.1 service/hibana-api 18080:8080
    ;;
  status)
    kube -n hibana get pods,jobs,services,pdb -o wide
    ;;
  *) echo "usage: bash scripts/k8s-local.sh [up|forward|status]" >&2; exit 2 ;;
esac
