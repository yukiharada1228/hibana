# Kubernetes 上に複数の Wasmtime Worker を並べる構成（物理 / デプロイ構成）

物理デプロイの観点で、Data Plane を Kubernetes 上の複数 Worker Pod として水平スケールする構成を示す。論理的なデータフロー（結果返却経路など）は `アーキテクチャ図.md` を参照。正典は `仕様書.md` 本文とする。

```mermaid
flowchart TB

    Client["Client"]

    Ingress["Ingress / LoadBalancer"]

    subgraph ControlPlane["Control Plane"]
        Axum["Axum (Deployment x N)"]
        Postgres[("PostgreSQL")]
    end

    NATS["NATS JetStream<br/>(clustered, R>=3, mTLS)"]

    Shared["Shared Store<br/>Redis (HA; rate limit / counters)"]

    Client --> Ingress
    Ingress --> Axum

    Axum --> Postgres
    Axum -->|"rate limit / in-flight / login失敗"| Shared
    Axum -->|"Publish Job<br/>tenant.{id}.component.invoke"| NATS

    subgraph DataPlane["Data Plane (Worker Deployment + HPA)"]
        Worker1["Wasmtime Worker Pod #1"]
        Worker2["Wasmtime Worker Pod #2"]
        Worker3["Wasmtime Worker Pod #3"]
    end

    NATS -->|"shared Pull Consumer<br/>(competing consumers)"| Worker1
    NATS --> Worker2
    NATS --> Worker3

    Storage["Object Storage<br/>(per-tenant prefix; read-only + 出力キー限定 write)"]

    Worker1 -->|"read 取得 / cache miss 時のみ"| Storage
    Worker2 --> Storage
    Worker3 --> Storage

    Worker1 -->|Result / Logs| NATS
    Worker2 --> NATS
    Worker3 --> NATS

    NATS -->|"署名・出所検証して永続化"| Axum
```

> Worker は HPA で水平スケールし、JetStream の共有 Pull Consumer（competing consumers）により 1 ジョブを 1 Worker のみが処理する（多重実行なし）。Pod は非 root / read-only rootfs / NetworkPolicy / seccomp を併用してサンドボックスを多層化する（`仕様書.md` §7）。
