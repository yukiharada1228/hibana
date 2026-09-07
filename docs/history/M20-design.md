> 過去の設計記録です。現行スコープはルートの仕様書.mdとREADME.mdを参照してください。削除済み機能を含みます。

# M20 設計書 — Durable Objects の常駐ランタイム（in-memory 持続 + WebSocket, §10 拡張）

本書は **DO の残ギャップ**（リクエストを跨ぐ in-memory 状態の持続 と WebSocket）を実装するための設計提案である。
現状（M16/M17）で **storage / グローバル単一書き手 / alarms** は動いているが、これらは「毎 invoke 起動
（per-invocation activation）」モデルの上に載っている。in-memory 持続と WebSocket は**このモデルを超える**ため、
コードに入る前に方式を確定させる必要がある。本書は §1 で現状の限界を file:line で示し、§2 で方式候補と
トレードオフ、§3 で**推奨案**、§4 で段階的な実装計画、§5 で非スコープを与える。

> **この文書の目的**: 実装ではなく**意思決定**。§3 の推奨（Option A: worker 常駐アクター + 配置レイヤ）で
> 進めてよいか、あるいは別案かを決めるためのもの。承認後に M20-1 から実装する。

---

## 1. なぜ現状モデルでは足りないか

現在の DO 実行はステートレスな per-invocation：

- `run_component`（crates/worker/src/main.rs:2111）は **ジョブごとに wasm を新規インスタンス化**する
  （`ProxyPre::instantiate_async`, :2353）。実行が終わればインスタンスは破棄される。
- DO の `fetch()` / `alarm()` は毎回 **新しい JS オブジェクト**として構築される（SDK shim の `new Cls(ctx, env)`）。
  跨リクエストで `this.foo` のような **in-memory 状態は保持されない**（storage 経由でのみ永続）。
- 単一書き手は Postgres advisory lock（`handle_do_lock`, crates/worker/src/main.rs:325）で**実行の間だけ**
  直列化する。ロックは実行末に解放され、インスタンスは常駐しない。
- 公開 ingress（`ingress_fallback`, crates/control-plane/src/ingress.rs:70）は **request/response の 1 往復**を
  invoke パイプラインに載せるだけで、**WebSocket / コネクションの持続をそもそも扱えない**。

したがって Workers 互換の以下が未対応：

1. **in-memory 持続** — 同一 DO id への連続リクエストで JS オブジェクトが生き続ける（キャッシュ・接続プール等）。
2. **WebSocket** — `server.accept()` / `state.acceptWebSocket()`（Hibernation API）とメッセージの双方向配送。

いずれも「DO インスタンスが**リクエストを跨いで生存し**、その id 宛のトラフィックが**同じ生存インスタンスへ
route される**」ことを要求する。これは配送・配置・接続終端の 3 点で現行と根本的に異なる。

---

## 2. 方式候補

### Option A — worker 常駐アクター + 配置レイヤ（推奨）

DO インスタンスを **1 つの worker に pin** し、その worker がメモリ上に wasm インスタンス + JS オブジェクトを
**常駐**させる。id 宛のトラフィックは配置レイヤで所有 worker へ route する。

- **配置（placement）**: `(tenant, class, id)` → 所有 worker を決める。案: Postgres の
  `do_placements(tenant, class, id, worker_id, lease_expires_at)` にリースを持たせ、`FOR UPDATE SKIP LOCKED`
  で獲得（既存の advisory-lock 資産と同型）。所有 worker が落ちたらリース失効で再配置。
- **route**: gateway / worker は id のリクエストを所有 worker の内部エンドポイントへ転送（`do.hibana.internal`
  の延長 or NATS の per-worker inbox）。所有 worker はインスタンスを in-memory `HashMap<DoKey, ResidentDo>` に保持。
- **単一書き手**: 「1 id = 1 worker + そのプロセス内で直列」で自然に成立（現状の advisory lock はリース獲得へ格上げ）。
- **WebSocket**: gateway が WS を終端し、所有 worker へフレームを転送（or WS を所有 worker へ直結）。
- **eviction / hibernation**: idle タイマでインスタンスを退避。Hibernation API は WS attachment を storage に
  逃がしてインスタンスを落とし、次フレームで復元。

**長所**: 既存 worker フリートを再利用。in-memory / WebSocket / hibernation すべてを 1 つの土台で満たせる。
**短所**: 配置・route・WS 終端・lease failover と、実装面積が最大。worker がステートフルになる（drain/rebalance 設計が要る）。

### Option B — 専用 DO ランタイムサービス

常駐インスタンスを持つ**別プロセス**（DO runtime）を新設し、worker/gateway はそこへ委譲する。

**長所**: 一般 worker をステートレスに保てる（スケール規律が別々にできる）。
**短所**: 新デプロイ単位・新スケーリング・新監視。配置/route/WS の複雑さは Option A と同等に残る。MVP には過剰。

### Option C — WebSocket Hibernation のみ（in-memory 持続は非対応）

Workers の **Hibernation API は「メッセージ間で DO を退避してよい」設計**なので、in-memory 常駐を諦めても
WS は成立しうる。WS 接続だけは gateway/worker が保持し、フレーム到着ごとに per-invocation で `webSocketMessage()`
を起動、attachment は storage から復元。

**長所**: 実行モデル（per-invocation）を維持でき、Option A より小さい。
**短所**: 「接続の保持」はやはり request/response モデルの外なので gateway に WS 終端 + フレーム→invoke 配送が要る。
in-memory 持続（`this.foo` 保持や WS 以外のユースケース）は満たせない。

---

## 3. 推奨

**Option A（worker 常駐アクター + 配置レイヤ）を段階実装する。**

理由：
- in-memory 持続と WebSocket の**両方**を 1 つの土台で満たせる（Option C は前者を諦める）。
- 既存資産（advisory lock による単一書き手、`do.hibana.internal` 内部ホスト、native wasi:http 実行）の延長線上に
  置ける。lock を「lease」へ、per-invocation を「常駐 + route」へ格上げする形。
- 新プロセス（Option B）を増やさず worker フリートに載せる。

ただし **worker がステートフルになる**のは本プロジェクト最大の設計変更なので、**最小の縦切り**から入り、
各段で実機検証（+ smoke 拡張）してから次へ進む。

---

## 4. 段階的実装計画

- **M20-1 配置 + fetch アフィニティ**: `do_placements`（lease）を追加。gateway/worker が id リクエストを所有 worker へ
  route し、所有 worker がインスタンスを常駐（idle 退避つき）。これで `fetch()` の **in-memory 持続**が動く。
  WebSocket は未対応。単一書き手は lease で担保（advisory lock から移行）。**検証**: 同一 id への連続 `fetch` で
  in-memory カウンタが保持される（storage 非依存）ことを実機 + smoke で確認。
- **M20-2 WebSocket（non-hibernating）**: gateway で WS を終端し、所有 worker の常駐インスタンスへフレーム転送。
  `fetch` 内の `new WebSocketPair()` / `server.accept()` と `webSocketMessage/Close/Error` を配線。**検証**: echo WS。
- **M20-3 Hibernation API**: `state.acceptWebSocket()` + attachment 退避 + idle でインスタンスを落として復元。
  長時間アイドルな大量 WS をメモリ常駐なしで保持。**検証**: 退避→フレーム→復元で attachment が保たれる。
- **M20-4 failover / rebalance**: 所有 worker 停止時の lease 失効 → 再配置、drain 時の明け渡し。**検証**: chaos で
  worker kill 中の WS/fetch 継続。

各段は独立 PR。M20-1 だけでも「in-memory 持続」という価値が出る。

---

## 5. 非スコープ（この設計では扱わない）

- DO の**地理分散配置**（Workers の smart placement 相当）。単一リージョン内配置のみ。
- WS の**下位互換でない拡張**（sub-protocol 折衝の高度なケース等）は最小に留める。
- 既存の storage / alarm / 単一書き手のセマンティクス変更（M16/M17 のまま。lock は lease へ内部移行するが
  ユーザから見た保証は不変）。

---

## 6. 決定事項（要承認）

1. 方式は **Option A**（worker 常駐 + 配置レイヤ）でよいか。
2. route の担い手は **NATS per-worker inbox** と **HTTP 直接転送**のどちらを優先するか（M20-1 で確定させたい）。
3. まず **M20-1（配置 + fetch アフィニティ）** から入る段取りでよいか。

承認が得られ次第 M20-1 の詳細設計 + 実装に進む。
