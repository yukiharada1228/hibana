> 過去の設計記録です。現行スコープはルートの仕様書.mdとREADME.mdを参照してください。削除済み機能を含みます。

# M9 設計: サンドボックス強化・サプライチェーン

仕様書 §15 M9 / §6.2 / §4.4 / §12 原則 5。

> **目標**（仕様書 §15）: 未信頼 wasm をマルチテナントで常時実行するための多層防御を完成させる。
> **完了条件**: 脅威モデルに基づくレッドチームテストで、悪意ある wasm が
> 隣接テナント / ホスト / 未許可 outbound に到達できないことを確認する。

M7/M8 と性質が違う。M5〜M8 は「機能を作って chaos で測る」だったが、M9 は
**攻撃側（レッドチーム）を書き、それが全部弾かれることを示す**マイルストーンである。
したがって設計は「守るコード」ではなく「まず脅威を並べ、各脅威をどの層が止めるか」から始める。

---

## 1. 現状のセキュリティ姿勢（着手前に実測した事実）

推測ではなく、敵対的 component を作って各層で何が起きるかを実測した（2026-09-04）。

### 1.1 既にある防御 — 二重の enforcement point、両方 deny-by-default

| 層 | 場所 | 実測結果 |
| --- | --- | --- |
| **アップロード検証** | `crates/control-plane/src/validation.rs` | `wasi:sockets/*` / `wasi:filesystem/*` を宣言する wasm は **422 で拒否**（承認接頭辞 `wasi:io/` `wasi:cli/` `wasi:clocks/` `wasi:random/` `faas:component/` のみ通過）。実測: filesystem を触る component は `unapproved host import 'wasi:filesystem/types'` で拒否された |
| **ランタイム WasiCtx** | `crates/worker/src/main.rs:1855` の `WasiCtxBuilder::new()` | **何も grant しない**。env 非継承（env dump は `env_count=0` を返す・実測）/ preopen 無し（fs 到達不可）/ `inherit_network` を呼ばない（sockets 到達不可）。`add_to_linker_async` で full WASI を*配線*するが、ケイパビリティが空なので実際には到達できない |

**重要な認識**: 真の防御は**ランタイムの空 WasiCtx** である。検証は「そもそもデプロイさせない」
第一関門だが、仮に検証をすり抜けても WasiCtx が空である限りホストには届かない。
この二重性は M9 の設計前提であり、**どちらか片方だけを強化しても意味がない**（両方を
同じ方針で動かす必要がある）。

### 1.2 埋めるべき 3 つの穴

| 穴 | 内容 | 仕様の根拠 |
| --- | --- | --- |
| **A: egress allowlist が実装不能** | 検証が `wasi:sockets` を全拒否するので、**どの component も一切 egress できない**。§4.4 の `net.allow_outbound: ["host:port"]` に配線が無い。AI/外部 API 連携（§13）はこの穴が開くまで不可能 | §15 M9「egress allowlist の実効的なネットワーク強制」/ §4.4 |
| **B: 検証がインプロセス** | `validate_wasm` は CP プロセス内の `spawn_blocking`。巨大 / 深ネスト wasm で wasmparser を殺す DoS に対し、semaphore（同時 4）は**同時数を縛るが per-validation の memory/CPU を縛らない**。CP が落ちれば全テナントが道連れ | §6.2 MUST「隔離して行う…別プロセス（サンドボックス）で実行」 |
| **C: 署名 / 供給網検証が無い** | アップロードに署名検証が無い。テナントの deploy トークンが漏れれば任意の wasm を active にできる | §15 M9「署名付き Component と供給網検証」/ §6.2 step 5 |

---

## 2. 脅威モデル

「誰が」「何を狙い」「どの層が止めるか」を明示する。攻撃者 = **未信頼のテナント Component**
（アップロードできる wasm は敵対的でありうる、が前提。仕様書 §12 原則 5）。

| # | 脅威 | 攻撃ベクタ | 現状 | M9 後に止める層 |
| --- | --- | --- | --- | --- |
| T1 | ホスト FS 読み取り（CP secret / 他テナント cwasm） | `std::fs` → `wasi:filesystem` | ✅ 検証で拒否 + WasiCtx 空 | 既存を維持（回帰ガードのみ） |
| T2 | 環境変数の窃取（CP 資格情報 / 他テナント secret） | `std::env` → `wasi:cli/environment` | ✅ WasiCtx が env 非継承（空を返す） | 既存を維持（回帰ガード） |
| T3 | **未許可ホストへの outbound**（データ持ち出し / SSRF / 内部ネット探索） | `std::net` → `wasi:sockets` | ✅ 検証で全拒否（= egress 不能） | **M9c: 承認された allowlist のみ許可、それ以外は socket_addr_check で拒否** |
| T4 | **許可ホストになりすまし到達**（DNS rebinding で allowlist を回避） | 許可名を解決 → 実 IP は内部ネット | N/A（egress 自体が無い） | **M9c: 解決後の SocketAddr を allowlist の IP と照合** |
| T5 | **検証 DoS**（巨大/深ネスト wasm で CP を落とす） | アップロード経路 | ⚠️ semaphore のみ（プロセス道連れ） | **M9b: 別プロセス + rlimit + timeout** |
| T6 | **供給網汚染**（deploy トークン漏洩で悪性 wasm を active 化） | `POST /versions` | ⚠️ 署名検証無し | **M9a: テナント公開鍵での署名検証** |
| T7 | 隣接テナントの実行への干渉（CPU/メモリ枯渇で共倒れ） | 資源枯渇 | ✅ M4 の全リソース制限 + M8 の lane/permit | 既存を維持（回帰ガード） |
| T8 | 隣接テナントのデータ読み取り（DB/S3/NATS 越し） | RLS/署名バイパス | ✅ M3 RLS + M3c 署名 + M8 lane filter | 既存を維持（回帰ガード） |

**M9 が新規に閉じるのは T3・T4・T5・T6 の 4 つ**。T1・T2・T7・T8 は既存の防御で閉じており、
M9 のレッドチームテストは**それらが M9 の変更で退行していないこと**も確認する（回帰ガード）。

---

## 3. M9a: 署名付き Component と供給網検証（T6）

### 3.1 何を守るか

deploy スコープのトークンが漏れても、**テナントが登録した公開鍵で署名された wasm でなければ
active にできない**ようにする。deploy トークンは「アップロードの認可」、署名は「本体の真正性」で、
別々の秘密に依存させる（片方が漏れても攻撃が成立しない）。

### 3.2 設計

- **署名対象は `wasm_sha256`**（本体そのものではなくダイジェスト。既に validation が算出済み）。
  detached signature を multipart の `signature` フィールドで受け取る。
- **アルゴリズムは Ed25519**（既存 `signing.rs` の `Signer`/`Verifier` インフラと同じ curve。
  ただし鍵はテナント所有で、job token 鍵とは完全に別ドメイン）。
- **テナント公開鍵の登録** を新 migration + admin API で持つ:
  `component_signing_keys(tenant_id, key_id, public_key, status, created_at)`、FORCE RLS。
  複数鍵を許し（ローテーション）、`status IN ('active','retired')`。retired は検証を通すが
  新規署名の推奨から外す（M7 secret の KEK と同じ思想）。
- **enforcement は per-tenant のポリシー**: `tenants.require_signed_components`（bool, 既定 false）。
  既定 false にする理由は M8 と同じ ——「アップグレードで既存テナントのデプロイが突然壊れない」。
  true のテナントは署名必須、false は従来どおり（署名があれば検証はする＝任意で使える）。
- 検証失敗 / 鍵不在 / ポリシー違反はすべて **422 で fail-closed**、`audit_logs` に記録。

### 3.3 なぜ「本体丸ごと署名」ではなく sha256 署名か

本体は最大 32MiB。署名検証のために全バイトをもう一度ハッシュするのは、validation が既に
sha256 を算出しているので二度手間。sha256 に署名し、検証は「アップロードされた本体の
sha256 == 署名が主張する sha256」+ Ed25519 検証の 2 段。sha256 の衝突耐性に依存するが、
これは既に §6.6 の冪等性と cwasm キャッシュキーが依存している前提なので新たな仮定を増やさない。

---

## 4. M9b: 検証のプロセス隔離（T5）

### 4.1 現状の何が問題か

`validate_wasm`（validation.rs:130）は `spawn_blocking` で wasmparser の `validate_all` を回す。
これは **CP プロセス内**なので、攻撃者が「valid だが検証器のメモリを爆発させる」wasm
（深くネストした型 / 大量の section）を送ると、`MAX_CONCURRENT_VALIDATIONS=4` の枠内でも
1 検証あたりのメモリ上限が無いため CP の RSS を押し上げ、OOM で **全テナントの CP が落ちる**。
semaphore は同時実行数を縛るだけで、1 回の検証の資源を縛らない。

### 4.2 設計 — 別バイナリ + rlimit + timeout

- **新バイナリ `faas-validator`**（`crates/validator`、または control-plane の `--validate-stdin`
  サブコマンド）。stdin から wasm バイトを読み、検証して結果 JSON を stdout に書く。
  **CP とはプロセス境界で隔離**され、殺されても CP は生きる。
- CP は子プロセスを spawn し、次を課す:
  - **wall-clock timeout**（`VALIDATION_TIMEOUT_SECS`, 既定 5）。超過で SIGKILL。全 OS で効く。
  - **メモリ上限**（Linux: `setrlimit(RLIMIT_AS)`。`VALIDATION_MEM_LIMIT_MB`, 既定 256）。
    子プロセス側で `main` の先頭に設定する。**macOS では RLIMIT_AS が効かない**ので、
    macOS はローカル開発専用と割り切り timeout + 出力サイズ上限で縛る（本番 Linux は rlimit）。
  - **出力サイズ上限**（想定外に巨大な JSON を返さない）。
  - stdin/stdout 以外は閉じる（子は FS/net を触らない。検証はバイト列 → 判定だけ）。
- **決定: subprocess であって wasm-in-wasm ではない**。検証器自体を wasm にして wasmtime で
  走らせる案（究極の隔離）は、wasmparser を wasm ターゲットでビルドする複雑さと、cwasm 事前
  コンパイルとの二重管理を招く。subprocess + rlimit で §6.2 の「別プロセス」要件を満たし、
  ブラスト半径（子プロセス 1 個）も十分小さい。
- **並行数の制御は CP 側の semaphore を維持**（同時に走る子プロセス数の上限）。rlimit は
  1 プロセスの資源、semaphore は総数、の 2 軸で縛る。

### 4.3 fail 方針

子プロセスが timeout / OOM / 非ゼロ終了したら、そのアップロードは **422**（検証失敗）で拒否。
CP は生存を続ける。子プロセスの spawn 自体が失敗した場合（fork 上限等）は **503**（一時的、
retryable）にして「本体が悪い」と「基盤が詰まっている」を区別する。

---

## 5. M9c: egress allowlist の実効強制（T3 / T4）

### 5.1 現状の穴

検証が `wasi:sockets/*` を全拒否するので、**egress が構造的に存在しない**。これは安全側だが、
§4.4 / §13 が想定する「Component が承認された外部 API を呼ぶ」ユースケース（AI 連携含む）を
一切実現できない。M9c は「**承認された宛先だけ**に穴を開ける」。

### 5.2 設計 — 検証承認とランタイム強制の 2 点セット（両方必要）

§1.1 の二重 enforcement をそのまま使う:

1. **検証側（承認）**: capability に `net.allow_outbound: ["api.example.com:443", ...]` を宣言でき、
   admin が承認した component だけ `wasi:sockets/*` の import を許可する。承認されていない
   component が sockets を宣言したら従来どおり 422。承認は per-version（`component_versions.capabilities`
   に既に保存する枠がある）。
2. **ランタイム側（実効強制）**: worker が capability から `socket_addr_check` を組み立てて WasiCtx に
   設定する。`wasmtime-wasi 29.0.1` に `WasiCtxBuilder::socket_addr_check(Fn(SocketAddr, SocketAddrUse) -> bool)`
   が**存在することを確認済み**。allowlist に無い宛先への connect は false を返して拒否する。
   `allow_tcp(true)` は立てるが `allow_udp` は立てない（UDP の用途が無く攻撃面だけ増える）。

### 5.3 T4（DNS rebinding）への対処 — ここが設計の難所

allowlist は **ホスト名**（`api.example.com:443`）だが、`socket_addr_check` が受け取るのは
**解決後の SocketAddr**（IP:port）である。素朴に「ホスト名を許可」すると、攻撃者が
`api.example.com` を自分の DNS で `169.254.169.254`（クラウドメタデータ）や `10.x`（内部ネット）へ
向けて **allowlist を名前で通し実 IP は内部**、という SSRF/rebinding ができる。

対処（多層）:
- **プライベート/リンクローカル/ループバック IP は常に拒否**する（allowlist に何が書いてあっても）。
  `SocketAddr` が `is_private()` / `is_loopback()` / `is_link_local()` / メタデータ IP なら無条件 false。
  これは allowlist より優先する hard deny。**これだけで T4 の実害（内部到達）は消える**。
- ホスト名 allowlist は「解決結果がパブリック IP であること」を必須にする。
- ポートも allowlist で縛る（`:443` のみ、等）。
- **決定: 名前解決自体は worker に委ねる**（`allow_ip_name_lookup` を allowlist 付き component にだけ
  立てる）。解決結果の IP を `socket_addr_check` で hard-deny リストと照合するので、
  「名前は通るが内部 IP は撃たれる」が成立する。

### 5.4 egress バイト計量（§14 との接続）

egress を開くなら「どれだけ出て行ったか」を計量したい（§14 の I/O / egress バイト）。
ただし M9 の完了条件は「到達可否」であって「計量」ではないので、**計量は M9 スコープ外**とし
（gauge の配線は follow-up）、M9 は「許可/拒否が正しいこと」だけに集中する。過剰にしない。

---

## 6. 完了条件: レッドチームテスト `crates/control-plane/tests/chaos_m9.rs`

全て `#[ignore]`。攻撃 component を実際にアップロード/実行して、各脅威が弾かれることを
**公開 API + `/metrics` の整数観測**で確認する（M8 で確立した黒箱の作法）。

| シナリオ | 脅威 | 期待 |
| --- | --- | --- |
| `chaos_v1_filesystem_denied` | T1 | fs を触る component は 422（アップロード拒否）。回帰ガード |
| `chaos_v2_env_not_leaked` | T2 | env-only component はアップロードできるが、実行時 `env_count=0`。回帰ガード |
| `chaos_v3_unapproved_egress_denied` | T3 | sockets を宣言し **allowlist 未承認**の component は 422 |
| `chaos_v4_approved_egress_only_to_allowlist` | T3 | 承認 component は allowlist のホストにのみ到達、他は実行時に拒否される |
| `chaos_v5_dns_rebinding_to_internal_denied` | T4 | allowlist 名を内部 IP に向けても connect が拒否される（hard deny が優先） |
| `chaos_v6_validation_dos_does_not_kill_cp` | T5 | 巨大/深ネスト wasm を投げても CP の `/readyz` が 200 を保ち、後続の正常アップロードが成功する |
| `chaos_v7_unsigned_component_rejected` | T6 | `require_signed_components=true` のテナントで、署名なし / 不正署名の wasm が 422 |
| `chaos_v8_isolation_regression` | T7/T8 | M8 の U1 相当を縮小再実行し、M9 の変更でテナント分離が退行していないこと |

**負の対照**（M8 で確立した原則）: 各拒否テストは「拒否される攻撃」だけでなく
「**成功すべき正規操作**」も併せて確認する（例: v4 は allowlist 内への到達が**成功する**ことも見る）。
「全部 422」を「安全」と誤認しないため。

---

## 7. 実装スライスと着手順の推奨

M9 は M7/M8 より大きく、3 つの sub-feature は**独立にリリース可能**である。1 つの巨大 PR に
せず、機能ごとに分けることを推奨する。リスクと依存関係から着手順を提案する:

| 順 | スライス | 理由 | リスク |
| --- | --- | --- | --- |
| **1** | **M9b（検証のプロセス隔離）** | §6.2 が MUST と明記した既知の穴で、新機能ではなく**穴埋め**。他の 2 つと独立。攻撃面を増やさない（むしろ減らす） | 低。子プロセス管理と OS 差（macOS の rlimit）だけ |
| 2 | **M9c（egress allowlist）** | 新しい**能力を開ける**変更なので最も慎重を要する。二重 enforcement + DNS rebinding 対処が肝。ここが M9 の技術的中心 | 高。穴を開ける方向なので、設計を 1 つ間違えると SSRF になる |
| 3 | **M9a（署名検証）** | 独立性が高く後回し可能。migration + admin API + 検証配線 | 中。鍵管理 API の設計量 |

> **私の推奨: まず M9b を単独で land する。** 既知の MUST 違反を埋める安全な一歩で、
> レッドチームテストの `chaos_v6` を先に緑にできる。M9c は穴を開ける方向で最も危険なので、
> M9b が入って「検証基盤が隔離済み」になってから、腰を据えて設計レビュー込みで着手する。

### 7.1 各スライスの完了条件（DoD）

- **M9b**: `chaos_v6` 緑 + 既存の検証ユニットテスト全緑 + 正常アップロードの回帰緑。
- **M9c**: `chaos_v3/v4/v5` 緑 + `socket_addr_check` のハードデニー単体テスト（IP 分類の全列挙）。
- **M9a**: `chaos_v7` 緑 + 署名検証の単体テスト + `require_signed=false` テナントの回帰緑。

---

## 8. 非スコープ（M9 で「やらない」と決めたこと）

| 項目 | 判断 | 理由 |
| --- | --- | --- |
| 検証器を wasm-in-wasm で走らせる | 不採用（subprocess + rlimit） | §6.2 の「別プロセス」は subprocess で満たす。wasm 化は cwasm 二重管理を招き複雑度に見合わない |
| egress バイトの計量 | M9 スコープ外（follow-up） | 完了条件は到達可否であって計量ではない。§14 の gauge 配線は別途 |
| UDP egress | 実装しない | 用途が無く攻撃面だけ増える。`allow_udp` を立てない |
| mTLS / 宛先証明書ピンニング | M10 以降 | allowlist（ホスト:ポート + 内部 IP hard deny）で M9 の完了条件は満たせる |
| seccomp / gVisor 等の OS レベル追加サンドボックス | M10 以降 | wasmtime の WasiCtx + 検証隔離で M9 の脅威モデルは閉じる。OS レベルは次の深さ |
