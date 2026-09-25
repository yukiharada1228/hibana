# 2GB × 3ノードのローカル容量検証

2026-09-24、VPSを契約する前の判断材料として、各ノードを **2 CPU相当の使用量・2GiB RAM・スワップなし**に制限したkindクラスタで検証した。**今回のHonoアプリと軽い利用では、Keycloakを含む最小構成が動作した。** KubernetesやHibanaの冗長構成、VPS上の速度、長期安定性を保証する試験ではない。

数値の記録は [vps-capacity-validation.json](vps-capacity-validation.json)。Hibana本体のコードや通常の配備マニフェストは変更していない。

同時実行数1はこの初回実験の設定値。[追加の同時実行数検証](vps-concurrency-validation.md)では、メモリ予算を調整し、256MiB枠で4件・128MiB枠で8件の同時実行を確認した。

## 構成と制限

MacのDocker Desktop上に、既存環境とは別の `hibana-capacity-2g` を作成。Kubernetesは既存の `kindest/node:v1.37.0` イメージと同じdigestを使用し、K3sは使用していない。Hibana/Consoleは `0.2.0-rc.10-linux-arm64`、CLIは同版のソース、作業時のコミットは `e0c742002370732e97cd9c3cc18900eeb5ea4bfd`。

| ノード | 配置 |
| --- | --- |
| control-plane | Kubernetes API、etcd、スケジューラー、DNS、ストレージプロビジョナーなど |
| worker | Keycloak、Keycloak用PostgreSQL、Hibana管理API、Console、Redis |
| worker2 | Hibana Worker、Hibana用PostgreSQL、MinIO |

Kubernetesの管理ノードは1台、ワーカーノードは2台。各アプリケーションサービスは1 Podとし、配置を固定した。PostgreSQL 2系統とMinIOにはlocal-pathのPVCを使用した。**3台でもHAではなく、ノード喪失でサービスが停止し得る。** 更新方式も、追加Podを並行起動しない `Recreate` とした。

Dockerノードの作成時から `--cpus=2 --memory=2g --memory-swap=2g` を適用し、再起動後も上限が変わらないことを確認した。cgroup v2の `memory.max=2147483648`、`memory.swap.max=0`、`cpu.max=200000 100000` が実際の制限。

kindのkubeletには共有VM全体の10 CPU・8124516Kiが容量として見える。そのままではPodを過剰配置できるため、`systemReserved` をCPU `8200m`・メモリ `6289508Ki` に補正し、**各ノードのallocatableを1800m・1792Mi**にした。これはこのDocker VM固有の補正値であり、VPSへコピーしない。2GiBのうち256Mi相当をPod以外のために残す配置予算であり、別VMのOS負荷そのものを再現する処理ではない。

主要な変更は次のとおり。

| 設定 | 今回の値 |
| --- | --- |
| Hibana管理API | 1 Pod、request 128Mi、limit 512Mi |
| Hibana Worker | 1 Pod、request 768Mi、limit 1280Mi |
| Worker同時実行数 | 1 |
| ゲストメモリ予約予算 | 512MiB |
| コンパイル同時数 / 子プロセスメモリ設定 | 1 / 1024MiB |
| テストアプリのメモリ / タイムアウト | 256MiB / 15秒 |
| Keycloak | 1 Pod、request 512Mi、limit 1Gi、Java heap 128〜512MiB |
| DB・MinIO・Redis | 既存の依存サービス用リソース設定を使用 |

Podのlimit合計を全サービス同時に使える構成ではない。DBやオブジェクトストレージまで同時に大きくなる負荷は別途測定が必要。

認証用のユーザー・パスワード・クライアント・DBは検証専用に生成した。既存ユーザーや実データはコピーしていない。公開口はMacのループバックのみ。Keycloakは既存のローカル方式と同様に、管理API内の小さなnginxブリッジを通して同一のループバックissuerへ接続した。このブリッジもノードのメモリ制限・計測に含む。インターネット向けTLS入口は今回の対象外。

## 機能試験

以下が成功した。

- Keycloakの実ブラウザログイン、Authorization Code + PKCE、CLIのループバックコールバック、APIトークン発行。クラスタ再開後にも同じ検証ユーザーで再ログイン。
- `scripts/smoke.mjs` によるHonoアプリのビルド・CLI配備・再配備。Wasm本体は約12.4MBで、アップロード後のコンパイルは制限したWorker内で実施。JSからWasmを作る開発者側のビルドはMac上で行った。
- HTTP、バイナリPOST、Secretの登録・参照、SSE、旧版・明示した版へのロールバック。
- `hibana tail` による新しい実行イベントのJSON受信。
- Worker Podの作り直し。空のコンパイルキャッシュから約31.3秒で配備済みアプリのHTTP応答を再確認。
- クラスタ3台の停止・再開。停止に約61.1秒、起動開始からHibanaの受付再開まで約79.8秒、停止開始から合計約140.8秒。アプリ・配備情報・Secretの保持を確認。時間は操作全体の計測であり、連続HTTP監視による厳密な停止時間ではない。

初回のバケット初期化Jobは1回失敗し、再試行で成功した。全ノード再開時も、DNSやDBが使える前に起動したConsole・管理API・Worker・Keycloakが接続エラーで再試行した。その後は自動で復帰しており、これらはOOMKilledではない。**起動から再開まで常に無停止だった、あるいは全期間で再起動がゼロだったという結果ではない。**

## 5分間の負荷試験

単一のHonoアプリの `/` にアクセスし、HTTP 200の本文も確認した。リクエストの自動再試行は行っていない。

| 条件 | 時間 | 総リクエスト | HTTP 200 | HTTP 429 | HTTP 503 | 成功応答のp95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 毎秒2リクエスト | 180.2秒 | 360 | 360 | 0 | 0 | 37.6ms |
| 2クライアント・待機なし | 60秒 | 108,538 | 3,116 | 105,039 | 383 | 12.0ms |
| 4クライアント・待機なし | 60秒 | 165,613 | 2,989 | 162,612 | 12 | 14.6ms |

通常の軽い負荷は全件成功。過負荷試験は受付拒否を含む結果であり、全件成功ではない。後半2条件の成功処理は平均約52件/秒・50件/秒で、総送信数をHibanaの実行性能として扱わない。成功応答の本文不一致はいずれも0。

負荷期間の全常駐Podについて、UID変更とコンテナ再起動回数の増加はいずれも0。全観測でノードの `oom_kill` カウンターも0だった。

追加で、200msのストリーム処理を同時に2件開始すると、1件が200、もう1件が503となることを確認した。同時実行数1の構成では、短い平均処理時間でも処理が重なれば拒否が起こる。過負荷試験中の503本文は保存していないため、その395件すべての原因をこの追加試験だけで断定しない。

## メモリ

ノード全体のcgroupを約2秒ごとに記録した。Kubernetes、containerd、アプリ、依存サービスを含む。下表のworking setは `memory.current - memory.stat[inactive_file]` で算出した観測値。アプリのRSSだけの値ではない。

| ノード | 軽負荷中の観測最大working set | 全記録の観測最大working set | カーネル記録のmemory.peak（キャッシュ込み） |
| --- | ---: | ---: | ---: |
| control-plane | 704.4MiB | 1053.8MiB | 1910.9MiB |
| worker | 936.2MiB | 1072.3MiB | 2009.8MiB |
| worker2 | 639.8MiB | 815.5MiB | 1516.5MiB |

イメージ読み込みなどのページキャッシュを含めると2GiBに近づく場面もあったが、OOMによる終了はなかった。working setの最大は約1.05GiBで、これを「全メモリの瞬間最大が1.05GiB」とは解釈しない。2秒未満のworking setの変動は取りこぼし得る。全体の記録は約13.7分であり、数時間〜数日の継続試験ではない。

## 判断と再現範囲

**「小さなアプリを、低頻度で、停止を許容してまず動かす」なら、2GiB × 3台を候補から外す必要はない。** 標準の2レプリカ構成をそのまま使う判断ではなく、今回の1レプリカ・同時実行数1・固定配置という条件付きの結果。

次の違いは残る。

- MacのARM CPUとVPSのCPUは異なる。CPU時間の上限は設定できても、1コアの処理速度や共有CPUの混雑は再現していない。
- kindはLinuxカーネルを共有する。3台の独立したOS、VPSのディスク速度、ネットワーク遅延・帯域・障害は再現していない。
- kindのメモリ圧迫時eviction設定は無効のまま。今回はcgroupの上限、OOM、Pod状態で検証した。実VMのkubeletによる退避動作の試験ではない。
- 公開HTTPS、証明書発行・更新、外部バックアップ、監視基盤、多数アプリ・長期のDB/ログ増大は未検証。
- 単一のKubernetes管理ノード、各サービス1 Pod、ノード固定のPVCのため、ホスト喪失やディスク破損への可用性は検証対象外。

実験用の設定・スクリプト・生の計測は `.local/capacity-pilot/20260924-2g/` に保存した。`setup.py`、`test.mjs`、`observe.py`、`snapshot.py`、`restart.py` が試験処理。認証情報を含むため、このディレクトリ全体を共有・コミットしない。再実行には空いているポートと新しい検証クラスタを用意し、そのホストの容量に合わせてallocatableの補正を再計算する。

## 実験後の復旧

一時停止していた `hibana-dev` と `hibana-console` を再開した。元のDockerノードID・CPU/メモリ設定・全Deploymentのspecが実験前と一致し、全Deploymentの必要レプリカが利用可能であることを確認。既存データは保持した。別プロジェクトのサービスの停止・設定変更は行っていない。

再起動でノードのDocker内IPが変わったため、既存のKeycloak用ローカルnginx中継を再読み込みした。元のAPI `http://127.0.0.1:28080/readyz`、Console `http://127.0.0.1:28081/healthz`、KeycloakのOIDC discoveryはいずれもHTTP 200。`hibana-dev`には元からホスト公開HTTPポートがないため、Kubernetes上の稼働状態と構成一致で確認した。

実験クラスタの3ノードと計測プロセスは停止済み。再検証用のデータはローカルに残している。
