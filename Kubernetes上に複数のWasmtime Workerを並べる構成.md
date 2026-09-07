# Kubernetes上のHibana

KubernetesがControl Plane/Wasmtime WorkerのPodを配置し、HibanaがComponentと呼び出しを管理します。アプリごとのPodではなくWorker群を共有し、呼び出しごとにStore・環境変数・資源制限を分離します。

構成と導入手順は[deploy/kubernetes/README.md](deploy/kubernetes/README.md)、本番要件は[docs/on-prem-production.md](docs/on-prem-production.md)を参照してください。
