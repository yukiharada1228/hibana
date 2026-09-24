# アプリログ

アプリが標準出力・標準エラーに書いた内容を、実行ID・バージョンIDと紐付けて取得できます。JavaScript / Honoの`console.log()`・`console.error()`、Rustの`println!()`・`eprintln!()`などが対象です。実行履歴のHTTPステータス・ランタイムエラーとは別に表示します。

ログの収集・保存は`0.2.0-rc.7`以降で利用できます。`0.2.0-rc.8`ではCLIのログ閲覧を`hibana tail`へ統一し、従来の`logs`コマンドを削除しました。CLI・Control Plane・Worker・Consoleを同じ版に揃えてください。

## CLI

開発中の動作確認は、Wranglerの`tail`と同じ操作を基準にした`hibana tail`を使います（rc.8で追加。CLIとControl Planeの更新が必要です）。

```sh
hibana tail                         # hibana.json のアプリを今から監視
hibana tail my-api --format pretty  # 実行概要とstdout/stderrを読みやすく表示
hibana tail my-api --format json    # 1実行1行のJSON。jqなどへパイプ可能
hibana tail my-api --status error   # 実行失敗・タイムアウト
hibana tail my-api --search '雪'     # stdout/stderrの文字列に一致する実行
hibana tail my-api --version-id ver_ID
```

アプリ名は省略可能で、プロジェクト外では明示します。接続後の完了通知を到着順に下へ追加し、Ctrl+Cで終了します。開始前の履歴は再生しません。開始前から動いていた実行も、その後に完了すれば表示します。複数の実行が並行するため、表示する実行開始時刻の厳密な昇順は保証しません。

- 端末では`pretty`、パイプ・リダイレクト時は`json`を既定にします。`--format`で明示できます。
- `pretty`はHTTPステータス・実行結果・実行開始時刻（端末のタイムゾーン）・処理時間を表示します。出力が空でも実行概要を残し、`No application output.`は繰り返しません。`--verbose`で実行ID・バージョンIDを追加します。実際にログが未収集の場合と16KiBで切り詰めた場合は明示します。
- `--status ok`はアプリが正常終了した実行です。HTTP 404や500を返して正常終了した場合も含みます。`error`は実行失敗・タイムアウトです。`canceled`も受け付けますが、現行Hibanaにはキャンセル状態がないため該当実行はありません。
- `--search`はstdout・stderrへの大文字小文字を区別した部分一致です。正規表現ではありません。複数のフィルターはAND条件です。
- JSONは実行概要（`execution_id`、`version_id`、`status`、`http_status`、`created_at`、`wall_time_ms`、`error`、`logs`）に`outcome: "ok" | "error"`を加えたNDJSONです。接続案内・再接続・欠落警告はstderrへ出し、stdoutへ混ぜません。WranglerのJSONスキーマと同一ではありません。
- 通信断や一時的なサーバーエラーは同じカーソルから再接続します。権限不足・認証期限切れは終了し、再ログインを案内します。各取得で認証・Read権限・テナント境界を検証します。

`tail`は実行完了後のライブ監視です。実行途中の1行ごとの配信、HTTPメソッド・パス・ヘッダー・IPでのフィルター、`--sampling-rate`は未対応です。Wranglerの[コマンド仕様](https://developers.cloudflare.com/workers/wrangler/commands/workers/#tail)を基準に、Hibanaが保持している情報で対応しています。

接続先は他の操作と同じ`--profile`・`--url`、または`HIBANA_URL`・`HIBANA_TOKEN`で選びます。Readスコープが必要です。通常表示は端末の制御文字を無効化し、`--format json`はAPIの文字列をJSONとしてエスケープして出力します。

過去の保存ログは、Consoleでアプリの「実行履歴」→「実行の詳細」→「アプリログを表示」から取得します。再取得も可能です。ローカルの`hibana dev`では、実行終了後に開発端末へ出力します。

## 取得範囲と保存期間

- 正常終了・異常終了・タイムアウトのいずれも、Workerが完了結果を保存できた時点で取得できます。実行中のリアルタイム配信はありません。
- 実行ごとに標準出力・標準エラーを合計16KiBまで取り込みます。以降の出力は破棄し、アプリの書き込み自体は成功させます。切り詰めを画面・CLIに表示します。
- 二つのストリームは別々に表示します。ストリーム間の発生順序・行ごとの時刻は記録しません。不正なUTF-8は置換し、NULは文字列`\u0000`へ変換します。変換後も合計16KiB以内です。
- 保存期間は完了時刻から24時間です。APIは期限切れのログを返しません。DB上のログ本文は既存のreaperで定期削除します（既定30秒、各テナント1周期最大5,000件、500件ずつ別トランザクション）。停止中のテナントも対象です。清掃が遅延した場合は本文の物理的な削除まで時間がかかります。
- 実行履歴・利用量・監査記録は、ログ本文の清掃では削除しません。次期版では、履歴行に別の[保存期間と自動削除](database.md#実行履歴の保存期間)を適用します。DBバックアップに含まれる本文はバックアップの保存方針に従います。
- Workerの強制終了や完了結果の保存失敗では、ログが残らないことがあります。監査証跡としての完全性は保証しません。過去の実行のログは復元できません。

ライブ通知は完了結果のDBコミット後にRedisへ送ります。Redisには実行IDだけを置き、本文は従来のDBからRLS下で取得します。監視中のアプリごとに最大1,000通知を保持し、最後の取得から90秒で通知バッファを失効させます。CLIは通常1秒ごとに取得し、未取得分があれば続けて読みます。バッファの上限超過・失効・リセットによる欠落はCLIで警告します。通知の障害やDBコミット直後のプロセス停止ではライブ通知が届かない場合があります。通知失敗は確定済みのアプリ実行を失敗にはしません。保存済みの結果はConsoleの実行履歴で確認してください。

## 権限と内容

ログは実行と同じテナントのRead権限で参照でき、DBのFORCE RLSにも従います。現在の権限はテナント単位であり、アプリごとの閲覧制限はありません。Worker / Control Planeの共有運用ログにはアプリの出力を転記しません。

アプリが出力した内容は保存されます。Secrets、Authorizationヘッダー、個人情報などをアプリのログへ出さないでください。本文の自動マスキングは行いません。HibanaがHTTPリクエスト・レスポンス本文や環境変数をログへ自動記録することはありません。

## 管理APIと更新

- `GET /components/{component_id}/tail`：カーソルなしで監視開始位置を返し、`items`は空です。以降は`cursor`を指定し、完了通知順に最大100実行と`cursor`・`has_more`・`lagged`を返します。`status=ok|error|canceled`・`search`・`version_id`で絞り込みます。フィルターで全件除外されてもカーソルは進みます。通知のカーソルは実行開始時刻に依存しません。
- `GET /components/{component_id}/logs`：実行の作成時刻順、20件ずつ。`errors_only`・`before`は実行履歴と同じです。ログ出力のない実行も含みます。
- `GET /executions/{execution_id}`：従来の実行詳細に`logs`を追加します。取得可能な場合は`{stdout, stderr, truncated}`、実行中・旧版で未収集・期限切れなどは`null`です。
- 通常の`GET /components/{component_id}/executions`一覧にはログ本文を含めません。
- ログを返す応答は`Cache-Control: no-store`です。

更新前にDBをバックアップし、`m20260923_000013_application_logs`を適用してから対応するControl Plane・Worker・Console・CLIを使用します。既存実行は保持されます。ログ列の削除で内容が失われるため、このマイグレーションのdownは拒否します。

ログの取り込み時に出力上限を超えたバイト数はWorkerの`faas_guest_log_dropped_bytes_total`で確認できます。従来の`faas_guest_stderr_dropped_bytes_total`は置き換わります。
