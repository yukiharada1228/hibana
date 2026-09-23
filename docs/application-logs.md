# アプリログ

アプリが標準出力・標準エラーに書いた内容を、実行ID・バージョンIDと紐付けて取得できます。JavaScript / Honoの`console.log()`・`console.error()`、Rustの`println!()`・`eprintln!()`などが対象です。実行履歴のHTTPステータス・ランタイムエラーとは別に表示します。

この機能は`0.2.0-rc.7`以降で利用できます。CLI・Control Plane・Worker・Consoleを同じ版に揃えてください。

## CLI

```sh
hibana logs                         # hibana.json のアプリ、直近24時間の20実行
hibana logs my-api --errors-only    # HTTP 4xx/5xx、実行失敗、タイムアウト
hibana logs --execution exec_ID     # 指定した実行。プロジェクト外でも取得可能
hibana logs my-api --json           # 取得結果をJSONで出力
hibana logs my-api --before 'CURSOR' # 表示されたカーソルで次の20実行
```

接続先は他の操作と同じ`--profile`・`--url`、または`HIBANA_URL`・`HIBANA_TOKEN`で選びます。Readスコープが必要です。通常表示は端末の制御文字を無効化し、`--json`はAPIの文字列をJSONとしてエスケープして出力します。

Consoleではアプリの「実行履歴」→「実行の詳細」→「アプリログを表示」で取得します。再取得も可能です。ローカルの`hibana dev`では、実行終了後に開発端末へ出力します。

## 取得範囲と保存期間

- 正常終了・異常終了・タイムアウトのいずれも、Workerが完了結果を保存できた時点で取得できます。実行中のリアルタイム配信はありません。
- 実行ごとに標準出力・標準エラーを合計16KiBまで取り込みます。以降の出力は破棄し、アプリの書き込み自体は成功させます。切り詰めを画面・CLIに表示します。
- 二つのストリームは別々に表示します。ストリーム間の発生順序・行ごとの時刻は記録しません。不正なUTF-8は置換し、NULは文字列`\u0000`へ変換します。変換後も合計16KiB以内です。
- 保存期間は完了時刻から24時間です。APIは期限切れのログを返しません。DB上のログ本文は既存のreaperで定期削除します（既定30秒、各テナント1周期最大5,000件、500件ずつ別トランザクション）。停止中のテナントも対象です。清掃が遅延した場合は本文の物理的な削除まで時間がかかります。
- 実行履歴・利用量・監査記録は、ログ本文の清掃では削除しません。DBバックアップに含まれる本文はバックアップの保存方針に従います。
- Workerの強制終了や完了結果の保存失敗では、ログが残らないことがあります。監査証跡としての完全性は保証しません。過去の実行のログは復元できません。

## 権限と内容

ログは実行と同じテナントのRead権限で参照でき、DBのFORCE RLSにも従います。現在の権限はテナント単位であり、アプリごとの閲覧制限はありません。Worker / Control Planeの共有運用ログにはアプリの出力を転記しません。

アプリが出力した内容は保存されます。Secrets、Authorizationヘッダー、個人情報などをアプリのログへ出さないでください。本文の自動マスキングは行いません。HibanaがHTTPリクエスト・レスポンス本文や環境変数をログへ自動記録することはありません。

## 管理APIと更新

- `GET /components/{component_id}/logs`：実行の作成時刻順、20件ずつ。`errors_only`・`before`は実行履歴と同じです。ログ出力のない実行も含みます。
- `GET /executions/{execution_id}`：従来の実行詳細に`logs`を追加します。取得可能な場合は`{stdout, stderr, truncated}`、実行中・旧版で未収集・期限切れなどは`null`です。
- 通常の`GET /components/{component_id}/executions`一覧にはログ本文を含めません。
- ログを返す応答は`Cache-Control: no-store`です。

更新前にDBをバックアップし、`m20260923_000013_application_logs`を適用してから対応するControl Plane・Worker・Console・CLIを使用します。既存実行は保持されます。ログ列の削除で内容が失われるため、このマイグレーションのdownは拒否します。

ログの取り込み時に出力上限を超えたバイト数はWorkerの`faas_guest_log_dropped_bytes_total`で確認できます。従来の`faas_guest_stderr_dropped_bytes_total`は置き換わります。
