# PostgreSQL・Drizzle の Wasm 検証

2026-09-16、macOS arm64、Node.js 24.12.0 で PostgreSQL 0.7.4 のプリセットと、認証方式・Pool を個別選択した構成を検証しました。PostgreSQL core・Pool は0.7.0、通信部品は0.7.3、TLS エンジンは0.5.1、Node Stream は0.7.0、Node TCP／TLS は0.6.3、その他の基礎 Wasm・Node 互換は0.5.0です。任意拡張を同梱した Hono アプリから、**Wasm 上で PostgreSQL への接続・認証・クエリと Drizzle ORM の操作を確認しました。** Node API・DB ドライバーは引き続きアプリに同梱します。

## 実行した試験

| 対象 | 結果 |
| --- | --- |
| tarball のインストール | コンパイル済み Wasm と JS を利用。インストールフック・Rust ソース・C コンパイラーは不要 |
| 依存拡張の解決 | アプリは `@hibana/postgres` のみ指定。必要な基礎部品と Node アダプターを依存宣言から取り込み、通常の CLI で一つの Wasm に合成 |
| CLI の Wasm ビルド | Hono 4.13.7 + pg 8.23.0 の適応版 + Drizzle 0.45.2 を合成 |
| Control Plane の配備検証 | 成功。ホスト import は標準 WASI のみ |
| TCP／TLS の PostgreSQL 接続 | Wasmtime 36.0.14 から PostgreSQL 16.15 へ接続。SSLRequest から TLS に移行しカスタム CA を検証 |
| SCRAM-SHA-256 | 成功。Unicode のマッピング・NFKC 正規化が必要なパスワードでも認証 |
| SCRAM-SHA-256-PLUS | 成功。検証済み TLS 証明書とサーバーの SCRAM 証明を確認。`channel_binding=require/prefer/disable` と接続 URL の優先を検証 |
| channel binding の必須化 | TLS なし、証明書の binding 不一致、通常の SCRAM・平文パスワード・MD5・trust への切り替えを拒否。弱い認証方式の要求を拒否するときは認証情報・クエリを送信しない |
| pg のクエリ処理 | パラメーター、UTF-8、JSONB、bytea、callback、配列形式の行、名前付き prepared statement、SQL エラー後の再利用 |
| リクエスト内 Pool | 最大 2 接続で 8 クエリを処理し、終了時に解放 |
| Pool を省略した Client | 同じアプリを Pool あり・なしで合成。両方で TCP／TLS、SCRAM／SCRAM-PLUS、パラメーター付きクエリと読み取り専用トランザクションが成功。なしの構成に `pg.Pool` は存在しない |
| pipeline の終了処理 | `pipeline: true` でクエリ投入直後に `end()`。TCP／TLS、Promise／callback、成功／SQL エラーの8条件で、タイムアウトに頼らず全結果と終了を1回ずつ通知 |
| 接続開始前・開始時の失敗 | Client／Pool ごとに21種の不正なポート設定を拒否。通信部品の同期例外は Promise／callback に1回通知し、ソケットを解放して `end()` を完了 |
| Drizzle ORM | TCP／TLS の両方で INSERT／UPDATE／SELECT／DELETE、コミット・ロールバック |
| 失敗時処理 | 誤ったパスワード、信頼しない CA、ホスト名不一致、TLS 検証の無効化、クエリタイムアウトを拒否 |
| リソース解放 | 1 回の Wasm 実行内で 40 接続を順次開閉。ソケット上限を累積消費しない |
| 対照試験 | 通常の Node.js + 元の pg でも同じ一時 DB に TCP／TLS 接続・認証・クエリが成功 |

追加の拒否テストで、ホストファイル、ネイティブ libpq、クライアント証明書、不正な channel binding 設定、未対応の Pool 設定と SCRAM 反復制限も確認しました。Hibana の dev runtime が TCP 接続を `EACCES` で拒否すること、既存 TCP／TLS E2E と Rust の暗号ベクトル・上限テストが通ることも確認しています。証明書の DER は TLS と STARTTLS の両方で、Node の X509Certificate が解析した同一証明書と照合します。

分割の検証では、基礎部品14個が各1個の Wasm を持つこと、各ハッシュの公開操作が `digest` のみであること、TCP に DNS の import がなく、DNS と TLS に TCP の import がないことを検査しました。SHA-256 単独アプリには Node の Buffer／process グローバルがありません。DNS 単独アプリは TCP を無効にして名前解決し、TLS 単独アプリはネットワークを無効にして ClientHello を生成します。

`@hibana/postgres-tcp` は TLS と証明書用ハッシュの Wasm を同梱しません。別の実成果物で SCRAM・クエリ・Pool・Drizzle CRUD／トランザクションを確認し、TLS オプションと `sslmode=require` の URL が `ERR_PG_TLS_UNAVAILABLE` で拒否されることを確認しました。通常版と TCP 専用版は同じ pg 移植コードから生成します。

0.6.0 では `postgres-core` に通信・認証部品を渡すローカル拡張を、プリセットとは別の Wasm 成果物へビルドしました。

| 選択した構成 | Wasm 部品数 | 検証結果 |
|---|---:|---|
| SCRAM＋TLS＋SHA-256 証明書 | 8 | SCRAM-PLUS・Pool・Drizzle が成功。MD5・平文パスワード・trust の要求は認証情報やクエリを送る前に拒否 |
| MD5＋TCP | 3 | 専用の MD5 保存ロールへ接続し、クエリ・Pool が成功。SCRAM と TLS は拒否 |
| SCRAM＋TLS、証明書ハッシュの登録なし | 8 | `require`／`prefer` で PLUS を選ぶと不足ハッシュのエラー。明示した `disable` では通常 SCRAM が成功 |

同じ一時 DB に対して元の Node pg でも SCRAM と MD5 の各認証要求を受けたことを確認しました。認証方式ごとの依存検査では、SCRAM 構成から MD5・SHA-224・SHA-384・SHA-512 系を、MD5 構成から SCRAM・TLS・乱数・NFKC を除外できています。SHA-384 証明書だけを追加した構成と、暗号計算を持たない trust 構成も依存検査を行いました。

`scripts/test-postgres-selection.mjs` は、複数の factory 間の認証設定の独立性、設定オブジェクトを後から変更した場合、認証を省略する不正なメッセージ順序を検証します。各 factory の Client／Pool は自身の設定を保持し、未選択の認証は `ERR_PG_AUTH_UNAVAILABLE` で拒否します。

同じ試験アプリの成果物は、全機能プリセット 20852848 bytes、TCP プリセット 19536031 bytes、SCRAM＋TLS＋SHA-256 20433552 bytes、MD5＋TCP 18985862 bytes でした。これは機能選択の比較であり、アプリ全体の性能・サイズ最適化の測定ではありません。再現スクリプトは各 HTTP リクエストのパス・状態・所要時間も報告書に残します。

## Pool の任意選択

0.7.0では `postgres-core` から `pg-pool` とその `setImmediate` 補助処理を除き、`postgres-pool` に移しました。公開プリセットは `pool: createPool` を選択し、従来の Client／Pool API を維持します。直接 `createPostgres` を使うローカル拡張では、Pool または Drizzle が必要な場合だけ明示的に選択します。Drizzle 0.45.2 は Client を渡した場合も内部で `pg.Pool` を参照するため、Pool 部品が必要です。

配布 tarball とビルド時の依存一覧を検査し、Client 専用構成に `pg-pool` が入らないこと、Pool 部品にドライバー・認証・通信・Wasm の実装が入らないことを確認しました。Pool に新しい通信権限や CLI の専用設定はありません。Client／Pool が同じ接続設定の検証と認証方針を保持すること、複数の構成が混ざらないことも検証しています。

同じ [Client 用アプリ](../scripts/fixtures/postgres/client-only.mjs)の JS バンドルは、Pool あり 508682 bytes、なし 490616 bytes で、18066 bytes 減りました。最終 Wasm はそれぞれ 17898372 bytes と 17709898 bytes でした。JS エンジンの初期化済みメモリも含むため、コードの削減量と Wasm のサイズは比例しません。非同梱の検証にはバンドルの依存一覧を使い、Wasm のサイズだけで判断しません。

SDK の拡張解決と PostgreSQL の JS テスト66件、拡張の依存境界試験、tarball からの Wasm ビルド、PostgreSQL／Drizzle の受入試験が成功しました。受入試験は75回の HTTP リクエストを記録し、一時 DB は停止・削除済みです。既存アプリへの再配備は行っていません。

## Socket の Stream 依存

Node TCP／TLS 0.6.0は、Node Stream 0.5.1の `@hibana/node-stream/duplex` を使います。通常の `node:stream` を読み込まないため、Socket だけのアプリから Transform・PassThrough・pipeline・compose・追加の演算子を除外できます。アプリ側でこれらを使う場合は、後述の機能別入口か通常の `node:stream` を import します。

配布 tarball を使い、Socket のみを export した JS と、同じソースで `node:stream` も import した JS を比較しました。後者の312488 bytesに対して前者は268545 bytesで、43943 bytes（約14%）減りました。依存一覧でも対象モジュールが入っていないことを確認します。この数値は JS バンドルの比較です。

専用入口でも、Readable／Writable のバッファ・終了処理と必要なバイト列変換は維持します。回帰試験では、UTF-8・オフセット付き Uint8Array・非同期読み取り・終了通知、通常の Stream と同じ Duplex コンストラクターであること、pipeline／compose の正常終了とエラー時の解放を確認しました。実 Wasm の TCP／TLS／STARTTLS、240接続の終了処理、512 KiBの転送と backpressure、ハンドシェイク途中の切断80回も成功しました。パッケージ数と Wasm の基礎部品は増やしていません。

## Stream の機能別入口

Node Stream 0.6.0から、Readable・Writable・Duplex・Transform・PassThrough・pipeline・compose・finished を個別の subpath から import できます。以下は0.7.0の配布 tarball で各入口を単独にビルドした結果です。

| 入口 | バンドルに含む上位機能 | JS bytes |
|---|---|---:|
| readable | なし。Writable・Duplex も非同梱 | 220702 |
| writable | なし。Readable・Duplex も非同梱 | 191449 |
| duplex | なし。読み書きの両実装を使用 | 256028 |
| transform | Transform | 259444 |
| passthrough | Transform・PassThrough | 260240 |
| pipeline | Transform・PassThrough・pipeline | 273123 |
| compose | Transform・PassThrough・pipeline・compose | 279130 |
| finished | なし。読み書き・Duplex のクラスも非同梱 | 155938 |

同じ PassThrough を通常の `node:stream` から export した場合は299482 bytesでした。機能別入口では39242 bytes（約13%）減り、pipeline・compose・追加演算子・上流の一括入口が含まれないことを確認しました。関数形式の pipeline の戻り値、compose の非同期イテレーター、finished の監視解除、各クラスの単独利用とバイト列変換も成功しました。

[Stream fixture](../scripts/fixtures/network/streams.mjs) は通常の入口を一切 import せず、機能別入口を組み合わせます。実 Wasm 上でも、UTF-8 の変換、backpressure、完了通知1回、エラー時の全 Stream 解放が成功しました。通常の入口との併用試験では、8つの API が同じ関数・コンストラクターを共有することも確認しました。

### 再 export 経由の未使用機能の除去

Node Stream 0.6.1では、アプリの共通ファイルから複数の機能別入口を再 export した場合に、未使用の上位機能が残る問題を修正しました。修正前は Readable だけを使っても Transform・PassThrough・pipeline・compose が出力され、追加した回帰試験が失敗しました。`package.json` の `sideEffects` に必要な初期化だけを指定し、未使用の入口を除去できるようにしています。

配布 tarball から8つの入口をそれぞれ再 export 経由でビルドし、実際に出力されるモジュールが必要な機能に限定されることを検証します。バイト列変換の初期化と、`import "node:stream"` による通常 API の初期化も保持します。実 Wasm 上の Readable 単独試験では、オフセット付き Uint8Array・UTF-8・終了後の解放を確認します。

この修正を選ぶ依存更新は Node TCP／TLS 0.6.2、PostgreSQL 通信部品0.7.2、プリセット0.7.3です。パッケージ数・Wasm 部品・通信権限・公開 API は増やしていません。

### Readable／Writable の循環参照の除去

Node Stream 0.7.0では、Readable／Writable が Duplex の型判定をするためだけに反対側のクラスまで読み込む依存を除去しました。型参照を小さな内部モジュールへ移し、Duplex が読み込まれた場合だけそのコンストラクターを渡します。上流と同じ `instanceof` 判定を保ち、Stream の読み書き・バッファ・終了処理は同じ実装を使います。

0.6.1の同じ単独入口の JS はどちらも254669 bytesでした。0.7.0では Readable が220702 bytes（33967 bytes、約13%削減）、Writable が191449 bytes（63220 bytes、約25%削減）です。直接 import と共通ファイルからの再 export の両方で、不要なクラスが出力に残らないことを検査しました。この変更による削減対象は単方向の Stream です。Duplex を使う Socket と PostgreSQL は読み書きの両実装を必要とし、この変更で小さくなるという意味ではありません。

単独の Readable／Writable をそれぞれネットワークなしの Wasm で実行し、UTF-8・オフセット付き Uint8Array・backpressure・終了時の解放を確認しました。Duplex の後からの読み込み、サブクラス、`instanceof`、公開 State コンストラクター、読み書き別の objectMode／highWaterMark も検証しました。全入口は同じ生成済みモジュールを共有し、通常の入口の API と初期化を維持します。

配布物には到達可能な上流モジュールとライセンスを含め、Node 専用入口と、別コピーの `readable-stream` npm パッケージを含めません。利用者側での拡張自体の生成やインストール時のパッチは不要です。Node TCP／TLS 0.6.3、PostgreSQL 通信部品0.7.3、プリセット0.7.4へ依存を更新し、TCP／TLS／STARTTLS と PostgreSQL／Drizzle の実通信試験も成功しました。

## 接続失敗と部品の受け渡しの回帰試験

0.6.1でレビューの3件を修正し、0.6.2では認証全体の順序と終了処理を追加修正しました。0.6.3では明示的な接続キャンセルを、Node TLS 0.5.2では STARTTLS の引数検証を修正しています。

| 問題 | 修正と検証 |
|---|---|
| TLS 確立前の EOF で接続が待ち続ける | Rust TLS エンジンが `ECONNRESET` を返すよう修正。実 Wasm の TLS／STARTTLS で各40回の切断を繰り返し、エラー1回・close・元ソケットの解放を確認。32接続の上限を累積消費しない |
| MD5 の計算失敗が connect に返らずソケットが残る | 同期例外と Promise の拒否を接続失敗へ伝播。Client の Promise／callback と Pool で元のエラーを通知し、ソケットを解放。パスワード取得失敗と切断後に完了する MD5 処理も確認 |
| クラス形式の通信／SCRAM 部品が検証後に壊れる | 必要なメソッドを元のインスタンスに bind して保持。プロトタイプ・private field・後からのメソッド差し替えを含む回帰試験を追加 |
| 切断後にパスワード取得・SCRAM 計算が完了して認証処理を再開する | 非同期処理の前後で接続状態を確認。終了後のセッション作成・証明送信・二重エラーを防止。MD5・平文パスワード・SCRAM の失敗で Client／Pool の接続を終了 |
| 応答を送る前の認証成功や重複要求を受け入れる | 認証状態を一つの実装で管理。実 Wasm と TLS の試験相手で、早すぎる成功通知2種・MD5 要求の重複・SCRAM の継続／最終通知の先行を検証。認証情報・クエリは送信しない |
| 独自 SCRAM の非同期 final 検証を待たずに接続できる | `startSession`／`finalizeSession` の同期呼び出し規約を明示し、Promise を返す実装は接続失敗として扱う。後からの Promise 拒否も未処理にならない |
| 接続途中の `end()` が `connect()` を未完了のまま残す | 接続待ちを `ERR_PG_CONNECTION_CANCELLED` で1回だけ完了。接続開始・パスワード取得・MD5・SCRAM・ReadyForQuery 待ちの各段階で Promise／callback を検証。繰り返し終了・callback 内での再終了・終了後の遅い認証結果も確認。実 Wasm と PostgreSQL の TCP／TLS 接続でも成功 |
| STARTTLS の不正な timeout が元のソケットを使えなくする | 所有権の移動前に検証。実 Wasm で負数・NaN・Infinity・文字列・null を `connect`／`TLSSocket` の両方で拒否し、その後も TCP echo と正しい設定での TLS 接続が成功 |
| pipeline 実行中の `end()` が完了応答を無視する | 0.6.4で最初の ReadyForQuery に対する認証検証と、その後のクエリ完了処理を分離。終了待ちでも投入済みクエリを処理し、認証失敗・切断後の応答は無視。SQL エラーがある場合も後続クエリを完了して閉じる |
| 不正なポートや通信部品の同期例外で `end()` が未完了になる | 0.6.5でポートを Client／Pool の生成時に検証。pg のエラー・終了通知を登録してから通信を開始し、同期例外を通常の接続失敗処理へ渡す。callback 内の再終了と接続待ちクエリの終了も検証 |
| 読み取り操作なしで相手が切断すると終了を検知しない | Node TCP 0.5.1で接続時のバッファリング開始と EOF 後の終了処理を修正。実 Wasm の TCP／TLS／STARTTLS、通常終了／半閉鎖の6条件で各40接続を解放。読み取りを遅らせた512 KiBの受信も欠損なく完了し、readableHighWaterMarkに従ってバッファリングを停止 |

Node TCP 0.5.1・Node TLS 0.5.3 の回帰試験では、`data` の購読や `resume()` を呼ばない場合も `end`・`finish`・`close` を各1回通知します。EOF を処理するためにデータを読み捨てたり、TLS の不正な終了を許容したりはしません。修正は共通 Socket の JS に限定し、Wasm エンジン・WIT・基盤の通信権限を変更していません。

PostgreSQL プリセット0.6.6でも、実 tarball からの Wasm ビルド、PostgreSQL／Drizzle の受入試験、TCP／TLS の通信試験が成功しました。SDK の拡張解決と PostgreSQL の JS テストは計63件が成功し、一時 DB は停止・削除済みです。既存の配備済みアプリにはまだ再配備していません。

0.6.5では JS テスト49件、実 tarball からの Wasm ビルド、PostgreSQL／Drizzle の受入試験が成功しました。ポートの0・範囲外・小数・数値以外・URL クエリでの指定を拒否し、通常の整数・数値文字列・既定値・URL 優先は維持します。実 Wasm でも不正なポート42条件と、`setNoDelay`／`connect` の同期例外8条件（Client／Pool × Promise／callback）を確認しました。接続タイムアウトを無効にしても元のエラーを1回通知し、ソケットと Pool 内の接続を解放して終了します。

0.6.4では JS テスト39件と PostgreSQL／Drizzle の受入試験が成功しました。追加した回帰テストは修正前に Promise／callback と成功／SQL エラーの4条件で失敗し、修正後に成功しました。実 Wasm では TCP／TLS の8条件すべてが HTTP 200 となり、`query_timeout: 0` でもクエリ結果と `end()` が完了してソケットが閉じることを確認しました。従来の認証途中のキャンセル、認証順序の拒否、SCRAM・MD5 の構成別試験も成功しています。

0.6.3では JS の選択・失敗処理のテスト34件、TCP／TLS E2E、PostgreSQL／Drizzle の受入試験、拡張の依存境界試験がすべて成功しました。TLS 0.5.1の修正時には Rust テスト4件と doc-test 1件、clippyも成功しています。認証の実装は `postgres-core/src/authentication.mjs` に集約し、上流 Client の不要な認証ハンドラー・pgpass の処理と通知は配布 JS から除去しました。これらの修正は拡張側で完結し、Hibana 本体への追加機能や新しい拡張パッケージはありません。パッチ版の導入方法は[配布と移行](../extensions/README.md#利用者向けの配布と移行)を参照してください。

## 検証範囲の境界

DB への通信試験は、テスト専用にネットワークを許可した **汎用 Wasmtime** で行います。本番 Hibana のネットワーク設定を変更した試験ではありません。最終 Wasm は実際の Control Plane で検証し、Hibana の dev runtime では外向き通信が拒否されることを別途確認します。

標準設定の Hibana は private／loopback 宛てを拒否します。パッケージを追加しても、Docker 内部や同一 Kubernetes 内の DB に自動的に接続できるわけではありません。配備時には、現在のポリシーで利用可能な接続先について管理者による egress 許可が必要です。

自動受入試験の DB は新規の一時コンテナだけを使用し、DB を tmpfs に置きます。終了時にコンテナ・認証情報・証明書を削除します。このスクリプトは既存 DB と Kubernetes 環境を変更しません。

## Kubernetes から Neon への確認

以下は0.6.0での配備記録です。その後の0.7.4までの回帰試験では、この配備と既存 DB は変更していません。

2026-09-16、利用者の `hello` アプリを `0.1.0-neon.5` として `hibana deploy` で既存の検証クラスタへ配備しました。PostgreSQL 0.6.0 の部品を使い、`database/pg.mjs` で SCRAM・TLS・SHA-256 証明書ハッシュを選択します。`hibana.json` は `extensions: ["./database"]` へ変更し、Hono のソースと `pg` import は維持しました。依存パッケージは21個から18個、Wasm 部品は14個から8個になり、MD5・SHA-224・SHA-384・SHA-512 系を除外しています。

Neon の公開証明書を認証なしの検証済み TLS 接続で確認し、SHA-256 の署名であることを確認してから選択しました。配備後は Wasm から Neon へ TCP 5432／検証済み TLS／SCRAM-SHA-256-PLUS で接続し、`GET /db/health` の HTTP 200 を確認しました。新バージョンには従来と同じ Neon ホストの TCP 5432 だけを許可し、解決済み IP に限定した Kubernetes NetworkPolicy を維持しています。本体や Worker イメージの変更は不要でした。

既存の `DATABASE_URL` Secret と `sslmode=require&channel_binding=require` を維持しました。Drizzle の読み取り専用トランザクションで定数・時刻・`transaction_read_only=on` を確認します。DB のテーブル・スキーマ・既存データは変更せず、元の `/` の応答と旧配備バージョン `0.1.0-neon.4` も保持しました。新バージョンの応答で `tls`・`channelBinding`・`readOnly` がすべて `true`、`Cache-Control: no-store` であることを確認しています。証明書更新で署名ハッシュが変わった場合は、必要なハッシュ部品を追加して再配備します。

## 追加した実装

以前は通信拡張だけで無変更の `pg` をビルドし、Node 組み込み API 6 種の不足で失敗していました。解決は [PostgreSQL 用の任意拡張](../extensions/postgres/README.md)で行いました。アプリの `import ... from 'pg'` をこの適応版へ解決します。

| 問題 | 拡張側の対応 |
| --- | --- |
| `crypto` と JS エンジンにない NFKC | 認証用 SHA-256／HMAC／PBKDF2、乱数、Unicode 正規化を Rust Component に実装 |
| `util`／`util/types` | pg 内部で必要な通知と日付判定だけを局所的に提供 |
| `fs`／`path`／ネイティブ用 DNS | pgpass とホストファイルへの依存を除外。非対応経路を明示的に拒否し、通常の DNS は WASI sockets に任せる |
| `setNoDelay` | pg 専用 stream では性能上のヒントとして省略。汎用 `net.Socket` の対応範囲は変更しない |
| Pool の認証情報 | pg が非列挙にする password を明示的に引き継ぐ |
| TLS エラーが切断通知に隠れる | TLS エラーを先に通知してから元のソケットを閉じる |
| channel binding に必要な Node API | rustls が検証した末端証明書を `getPeerCertificate().raw` で提供し、RFC 5929 の証明書ダイジェストを Rust の SHA-2 で計算。pg の SCRAM 検証を再利用して必須モードを強制 |
| DNS の次候補で接続した後の古い失敗 | 接続成功時に以前の TCP 接続エラーを破棄。証明書取得で DNS の過去の失敗を参照しない |

Rust 暗号ライブラリの利用箇所と反復回数・入力サイズを制限します。PBKDF2 の既知ベクトルと上限を Rust テストで確認します。pg のプロトコル・型変換・SCRAM の検証処理を再利用し、Node.js 全体の互換実装は提供しません。

Drizzle は公開の [node-postgres アダプター](https://orm.drizzle.team/docs/get-started-postgresql)を使用しています。確認した範囲は表の CRUD とトランザクションです。他の ORM、COPY、長寿命の通知待機、すべての pg API、性能上限は検証対象外です。

## 再現手順

Node.js 24 以降、Docker、OpenSSL、Rust toolchain、WAC、SDK 依存、ビルド済みの対象拡張とその依存・Control Plane・Worker・Wasmtime CLI が必要です。[拡張のビルド手順](../extensions/README.md#配布者)も参照してください。

```sh
npm ci --prefix sdk
npm ci --ignore-scripts --prefix extensions
npm ci --ignore-scripts --prefix scripts/fixtures/postgres
WASI_SDK_PATH=/path/to/wasi-sdk-34 npm run build --prefix extensions
node --test scripts/test-postgres-selection.mjs
docker pull postgres:16
WASMTIME_BIN=/absolute/path/to/wasmtime \
HIBANA_TEST_CP_BIN=target/debug/hibana-control-plane \
HIBANA_TEST_RUNTIME_BIN=target/release/hibana-worker \
node scripts/test-postgres-compatibility.mjs
```

[受入試験](../scripts/test-postgres-compatibility.mjs)は実 tarball と lockfile から一時アプリを組み立て、Wasm の HTTP 応答と DB 操作結果を検査します。**終了コード 0 は全受入試験の成功**です。以前の「ビルド失敗を記録するだけ」の診断から置き換え、CI にも登録しました。

出力は `.local/verification/postgres/report.json` と `hibana-build.log` です。`HIBANA_PG_REPORT_DIR` で出力先、`HIBANA_PG_IMAGE` でローカル PostgreSQL イメージを変更できます。使用した image ID と tarball の integrity を報告書に記録します。

ORM の [schema](../scripts/fixtures/postgres/schema.mjs)と[初期マイグレーション](../scripts/fixtures/postgres/migrations/0001_probe.sql)を fixture として維持します。マイグレーションは試験の管理側から一時 DB に適用します。Hibana の管理用 DB の SeaORM／マイグレーションとは独立しています。
