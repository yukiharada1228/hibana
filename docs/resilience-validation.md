# 隔離・復元・障害試験の実測

2026-09-08、チェックアウト専用`kind-hibana-dev`で実施しました。Docker Desktopの単一物理ホスト上に、control-plane 1ノード・worker 2ノードを置いた構成です。Hibana CP/Workerは各2 Podです。

## 結果

| 項目 | 実測 |
| --- | --- |
| Linuxコンパイル | Hono Componentをネットワークなし・read-only rootfs・capabilitiesなしのコンテナでコンパイル成功。子のRLIMIT_ASは1 GiB、コンテナ全体は1.5 GiB |
| バックアップ | Wasm 65オブジェクト、DB、署名鍵・Secrets暗号鍵・接続用Secretを保存。SHA-256照合成功 |
| DB復元試験 | 一時DBへpg_restoreし、テナント・Component・バージョン・Secrets・Secrets履歴・実行履歴の行数を照合して成功 |
| PVC移行 | PostgreSQL/MinIOをemptyDirからPVCへ移行。S3全オブジェクトを再取得してSHA-256照合、DB行数照合成功 |
| CLIの復元後確認 | login、新規deploy、Secrets、Hono HTTP、binary POST、環境ヘッダー偽装拒否、SSE、rollback成功 |
| 5言語混合負荷 | Hono / TypeScript / JavaScript / Go / Rust、300秒、並列4、17,040リクエストすべて成功 |
| 混合負荷終了時 | pending=0、running=0、Worker再起動0、DB接続最大25 |
| ディスクキャッシュ | 試験中のWorkerごとの最大224,852 KiB（約220 MiB） |
| Workerメモリ | Metrics Serverのworking setは開始約248/259 MiB、終了約328/332 MiB。短時間のcold startを含む値で、長期安定性の判定ではない |
| Redis中断 | 20秒pause。中断中のHTTPプローブは失敗、開始から30.8秒で元のHTTP本文へ復旧 |
| PostgreSQL中断 | 20秒pause。中断中のHTTPプローブは失敗、開始から30.8秒で復旧 |
| MinIO中断 | 20秒pause。キャッシュ済み関数はHTTP 200を維持、開始から20.8秒で復旧確認 |
| kind worker中断 | 60秒pause。PodのNotReadyを観測し、開始から71.2秒でHTTP復旧 |
| 全中断試験後 | 未完了実行0。DB実行ロールは非superuser・非BYPASSRLS。新規S3アップロード、Secrets、rollbackのsmokeも成功 |

Rustの通常テストは262件成功（実DB専用1件は通常実行の対象外）。運用スクリプト16件とKubernetes・アーキテクチャ・RLSの静的検査も成功しました。

コンパイルのキャンセル・タイムアウト時の子プロセス回収、非正常終了からの成果物拒否、環境変数の除去、ディスクキャッシュのバイト数/件数上限も単体テストで確認しています。バックアップ破損・危険なパス/テーブル名・非空復元先の拒否、停止途中のエラー時のレプリカ復元、HPA存在時の操作拒否はオフライン回帰テストで確認しています。

## 証跡と範囲

追加レビューで見つかった5件（同名アプリの設定混同、アップロード中のidle transaction、鍵IDの欠落、停止順序、運用コマンドの無期限待機）は回帰テストを追加して修正しました。

- `bash scripts/test-http.sh`：使い捨てPostgreSQL/Redisと実CPで検証。実行ID・SHAの組み合わせによる設定固定、16.5秒のアップロード成功とidle transaction 0、重複アップロードによる既存オブジェクトの上書き防止、S3保存待ち中のポリシー変更・削除の再確認を確認します。S3は制御可能なHTTPスタブを使います。
- 同じ試験で、受付停止中の結果保存、ランタイムDBロールによるゲート変更の拒否、処理中のアップロードのdrain、CP再起動後の受付停止継続、実DBに保存したSecretの復号と鍵ID不一致の拒否を確認します。
- 後続の原子的デプロイ改善では、同じ使い捨て環境に実Workerを加え、Read・Deployスコープのユーザーによる配備、選択したSecretだけの注入、S3待機中の利用許可取り消しによる公開全体の拒否、同時配備、varsを含むrollback、Secret削除・再作成時のID固定を実HTTPで確認しています。migration 0028の旧データ引継ぎと新テーブルのRLS・複合FKも検証対象です。これは前掲のkindイメージの試験とは別です。
- `python3 scripts/test_resilience.py`と`node --test scripts/test-bounded-process.test.mjs`：ローテーション後の鍵IDのバックアップ、復元失敗時の受付閉鎖、結果保存を待つ停止処理、停止要求を無視する子・孫プロセスの回収と独立した後片付けを確認します。

これらは修正後のコードに対する隔離テストです。上のkind実測イメージでformat 2バックアップや今回のメンテナンス経路まで試験済みという意味ではありません。適用には新しいCP/Workerイメージとmigration 0027が必要です。

- 混合負荷：`.local/scale-test/soak-1788814595016.jsonl`。低速受信と、別SHAのHono成果物への再デプロイを含みます。
- 障害：`.local/kubernetes/fault-redis-1788815026.json`、`fault-postgres-1788815057.json`、`fault-minio-1788815078.json`、`fault-hibana-dev-worker-1788815236.json`。
- 移行用バックアップ：`.local/backups/storage-migration-20260908`。**鍵を含むため共有・コミットしません**。
- 実機イメージ：`hibana-platform:resilience`、manifest `sha256:939fa4fb4aa31a00c2373e6c68cfd9a38b2d37f3a667a46c6d8fba788e0aaa4a`。最後のディスク256件制限と環境変数除去の追加テストは、このイメージの後に単体検証しました。並行するCLI/モジュール名変更全体の実機検証を意味しません。

この試験は実ノードの電源断、DBのprimary昇格、Redis Sentinel/HA endpoint切替、複数台S3の耐障害性を検証したものではありません。kindのlocal-path PVCはノード依存で、依存先も各1台です。中断時のHTTP失敗を観測しており、無停止HAの達成を示す結果ではありません。

2時間・24時間のsoakを指定する手段は実装済みですが、今回実行したのは5分です。メモリ・DB容量・p95の長期傾向、実ロードバランサーの迂回、CNI・TLS・物理障害ドメインでの検証は[手順](resilience.md)に沿って本番候補環境で行います。
