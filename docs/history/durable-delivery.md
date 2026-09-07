> 廃止済みの非同期配送設計です。現在のMVPは [HTTPのみ](../mvp.md) です。

# Native invokeの配送

HTTP直接実行と非同期Native invokeは同じWasmtime実行モジュールを使用します。配送経路は分かれます。

## 非同期invoke

受付はexecutionとoutboxをPostgreSQLの同一トランザクションへ保存します。NATS未接続でも、受理済みの実行意図はDBに残ります。dispatcherが有効期限付きジョブを署名し、JetStreamの保存ACK後に送信済みと記録します。

Workerは結果を永続ストリームへ保存してからジョブをACKします。CPのsubscriberは署名・テナント・バージョン・実行IDを照合し、実行の終端遷移と使用量計測を一度だけコミットします。結果の再配送は同じ実行を二重計上しません。これはゲストの外部副作用のexactly-onceを保証するものではありません。

## HTTP直接実行

アプリHTTPはCPからWorkerへ署名付き内部リクエストで渡します。WorkerはCPからDB固定のジョブを引き換え、応答をストリーム転送します。完了結果は内部APIでCPへ保存します。通常のHTTP経路はNATS配送に依存しませんが、DB・認証/Secrets・admission・Wasm保管先などへの依存はあります。

旧版のCron・Alarm・Queue・chainの受付/配送は提供しません。旧テーブルはマイグレーション履歴とデータ保全のため残ります。

## 試験

`bash scripts/test-durable-delivery.sh`は使い捨てPostgreSQL/NATSで、受付トランザクションのロールバック、outbox再送、結果保存失敗/再配送、使用量の単一計上、直接HTTPの署名照合を検証します。`scripts/k8s-local-delivery.py`は専用kindでNATS停止中の受付とCP全Pod置換後の復旧を検証します。
