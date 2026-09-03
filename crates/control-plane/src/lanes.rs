//! テナント別 lane Consumer の provisioning (M8, §3.7)。
//!
//! # 責務配置
//!
//! | 責務 | 担当 | 理由 |
//! | --- | --- | --- |
//! | lane の作成 / 更新 / 削除 | **control-plane**（reconcile リーダーを取った 1 インスタンス） | テナント一覧とクォータの権威が CP にある |
//! | lane の発見 | **worker** | 権威は NATS の consumer 一覧。worker は DB も CP HTTP も触らない |
//! | lane の設定値の解決 | **control-plane** | 既存の解決順序（グローバル既定 → tenants.quotas）を 1 箇所に保つ |
//!
//! M7 までは worker が共有 durable `workers` を作っていた。M8 で CP へ移す理由は 2 つ:
//! (1) テナント別 lane を作るにはテナント一覧とクォータが要り、それは CP の持ち物である。
//! (2) worker 0 台のとき consumer が存在しないと、backlog シグナル（M8-8）が
//!     「未消化の仕事があるのに consumer が無いので読めない」というブートストラップ・
//!     デッドロックを起こす（scale-from-zero が原理的に成立しない）。
//!
//! # なぜ単一 writer が必要か（§3.7.1）
//!
//! CP は「ステートレス × N」が前提（仕様書 §8）。排他無しに全インスタンスがそれぞれの DB
//! スナップショットで reconcile すると:
//! - A が overflow から subject X を外した直後に、古い desired を持つ B が X を戻す
//!   → **X がどの lane にも属さない窓**が生じ、その間の publish は誰にも配送されず
//!   900 秒後に stuck sweeper が偽 failed に倒す。
//! - 逆順なら X が 2 つの lane に属し、WorkQueue では filter の重なりをサーバが拒否するので
//!   drift が永久に収束しない。
//!
//! よって 1 パス全体を `pg_try_advisory_lock` で囲む。取れなかったインスタンスはその
//! ラウンドを skip する（cron スケジューラの `FOR UPDATE SKIP LOCKED` と同じ single-flight 思想）。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::consumer::pull::Config as PullConfig;
use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
use async_nats::jetstream::stream::Stream;
use futures::StreamExt as _;

use crate::db;
use crate::state::AppState;

/// consumer metadata に載せるキー。worker はこれを読んで per-lane の実行クレジットを決める。
pub const META_TENANT_ID: &str = "faas_tenant_id";
pub const META_LANE_CONCURRENCY: &str = "faas_lane_concurrency";
pub const META_LANE_KIND: &str = "faas_lane_kind";

/// 1 本の lane consumer の「あるべき姿」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredLane {
    /// durable 名（= consumer 名）。
    pub name: String,
    /// この lane が購読する subject 群。**空にしてはならない**（空は全 subject 購読を意味し、
    /// 「どの subject もちょうど 1 consumer」という不変条件を破壊する）。
    pub filter_subjects: Vec<String>,
    /// 配送済み未 ack の上限。テナント単位で導出する（合算ではない）。
    pub max_ack_pending: i64,
    /// worker へ配る付随情報。
    pub metadata: BTreeMap<String, String>,
}

/// 望ましい lane トポロジ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneTopology {
    /// M7 までと同じ共有 durable 1 本（`TENANT_LANES_ENABLED=false` / ロールバック時）。
    Legacy { max_ack_pending: i64 },
    /// テナント別 lane（+ 必要なら overflow lane 1 本）。
    Lanes(Vec<DesiredLane>),
}

impl LaneTopology {
    pub fn lanes(&self) -> Vec<DesiredLane> {
        match self {
            Self::Legacy { max_ack_pending } => vec![DesiredLane {
                name: faas_shared::LEGACY_SHARED_DURABLE.to_string(),
                filter_subjects: vec![faas_shared::invoke_subject_wildcard().to_string()],
                max_ack_pending: *max_ack_pending,
                metadata: BTreeMap::from([(META_LANE_KIND.to_string(), "legacy".to_string())]),
            }],
            Self::Lanes(v) => v.clone(),
        }
    }

    /// ロールバック経路（lanes → legacy）かどうか。
    ///
    /// この経路では「未処理 0 のときだけ削除する」という通常の MUST を**明示的に免除する**
    /// （§9.2）。理由: WorkQueue では filter が重なる consumer の作成をサーバが拒否するため、
    /// 「全 lane を消してから legacy を作る」以外の順序が存在しない。免除で失われるのは
    /// consumer 側の ack floor / 配送状態だけで、**メッセージは WorkQueue に残ったまま**
    /// legacy consumer が `DeliverPolicy::All` で拾い直す。DB 行側は stuck sweeper が二重に守る。
    pub fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy { .. })
    }
}

/// 望ましいトポロジを計算する（純粋な写像。DB / NATS を触らない）。
pub fn desired_topology(
    state: &AppState,
    tenants: &[(String, db::TenantQuotaOverrides)],
) -> LaneTopology {
    if !state.tenant_lanes_enabled() {
        return LaneTopology::Legacy {
            max_ack_pending: state.lane_overflow_ack_pending(),
        };
    }

    let ids: Vec<String> = tenants.iter().map(|(id, _)| id.clone()).collect();
    let assignment = faas_shared::assign_lanes(&ids, state.max_dedicated_lanes());
    let headroom = state.lane_ack_pending_headroom();

    let mut lanes = Vec::with_capacity(assignment.dedicated.len() + 1);
    for (durable, tenant) in &assignment.dedicated {
        let overrides = tenants
            .iter()
            .find(|(id, _)| id == tenant)
            .map(|(_, o)| *o)
            .unwrap_or_default();
        let resolved = state.admission().resolve_for_tenant(tenant, &overrides);
        lanes.push(DesiredLane {
            name: durable.clone(),
            filter_subjects: vec![faas_shared::invoke_subject(tenant)],
            max_ack_pending: resolved.lane_ack_pending(headroom),
            metadata: BTreeMap::from([
                (META_TENANT_ID.to_string(), tenant.clone()),
                (
                    META_LANE_CONCURRENCY.to_string(),
                    resolved.lane_concurrency.to_string(),
                ),
                (META_LANE_KIND.to_string(), "dedicated".to_string()),
            ]),
        });
    }

    // overflow lane は **空なら作らない (MUST)**。filter_subjects が空の consumer は
    // 「全 subject 購読」を意味し、dedicated lane と filter が重なって不変条件を壊す。
    if !assignment.overflow.is_empty() {
        lanes.push(DesiredLane {
            name: faas_shared::OVERFLOW_LANE_DURABLE.to_string(),
            filter_subjects: assignment
                .overflow
                .iter()
                .map(|t| faas_shared::invoke_subject(t))
                .collect(),
            // **所属テナント数に比例させない**。比例させると合算上限が事実上撤廃され、
            // overflow lane が「M7 までの共有 consumer」そのものに戻ってしまう。
            max_ack_pending: state.lane_overflow_ack_pending(),
            metadata: BTreeMap::from([
                (META_LANE_KIND.to_string(), "overflow".to_string()),
                (
                    META_LANE_CONCURRENCY.to_string(),
                    state.admission().lane_concurrency.to_string(),
                ),
            ]),
        });
    }

    LaneTopology::Lanes(lanes)
}

/// この CP が管理する consumer 名かどうか（他システムの consumer を巻き込まないためのガード）。
fn is_managed_consumer(name: &str) -> bool {
    name == faas_shared::LEGACY_SHARED_DURABLE
        || name == faas_shared::OVERFLOW_LANE_DURABLE
        || faas_shared::tenant_from_lane_durable(name).is_some()
}

/// stream 上に実在する「管理対象の」consumer 名を集める。
async fn collect_consumer_names(stream: &Stream) -> anyhow::Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let mut it = stream.consumer_names();
    while let Some(n) = it.next().await {
        let n = n.context("listing consumer names")?;
        if is_managed_consumer(&n) {
            names.insert(n);
        }
    }
    Ok(names)
}

/// `DesiredLane` を async-nats の consumer config へ変換する。
fn to_pull_config(state: &AppState, lane: &DesiredLane) -> PullConfig {
    let (ack_wait_secs, max_deliver, backoff_secs) = state.lane_delivery_params();
    PullConfig {
        durable_name: Some(lane.name.clone()),
        ack_policy: AckPolicy::Explicit,
        deliver_policy: DeliverPolicy::All,
        // 単一 filter なら filter_subject、複数なら filter_subjects を使う
        // （NATS は両方同時指定を拒否する）。
        filter_subject: if lane.filter_subjects.len() == 1 {
            lane.filter_subjects[0].clone()
        } else {
            String::new()
        },
        filter_subjects: if lane.filter_subjects.len() == 1 {
            Vec::new()
        } else {
            lane.filter_subjects.clone()
        },
        // M3c: トークン exp と同一定数から導出した ack_wait / max_deliver (§3.3)。
        // M8 で consumer の作成者が CP に移ったため、CP がこの結合の責任を持つ。
        ack_wait: Duration::from_secs(ack_wait_secs),
        max_deliver: max_deliver as i64,
        backoff: backoff_secs
            .iter()
            .copied()
            .map(Duration::from_secs)
            .collect(),
        max_ack_pending: lane.max_ack_pending,
        metadata: lane
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        ..Default::default()
    }
}

/// 実 consumer が望ましい設定からずれているか。
///
/// **既存の drift 判定（M7 までの worker 側）には `filter_subject` 軸と `metadata` 軸が無く、
/// 誤った filter の lane が永久に是正されなかった**。lane 化では filter がトポロジそのものなので
/// 必ず含める。
/// サーバが実際に保存する ack_wait へ正規化する。
///
/// **NATS は `backoff` 配列が設定されているとき、`ack_wait` を `backoff[0]` から導出して保存する**
/// （最初の再配送までの待ちが `backoff[0]` なので、意味的にもそれが正しい）。desired 側は
/// TTL 結合の基準値として `ACK_WAIT_SECS`（既定 30）を持つが、`backoff = [5,15,60]` を併せて
/// 送ると **サーバは ack_wait を 5 秒として保存する**。
///
/// この正規化を挟まないと `actual.ack_wait(5s) != desired.ack_wait(30s)` が恒久的に成立し、
/// reconcile が毎周期 drift と判定して次を無限に繰り返す:
/// consumer UPDATE を投げる → lane 世代をバンプ → **lane キャッシュが全消え**（invoke ホット
/// パスが毎回 ensure を打つ）→ `faas.lane.changed` を publish → **全 worker が再 discovery**。
/// 壊れはしないが、キャッシュと通知の設計意図が丸ごと無効化される。
///
/// TTL 結合（§3.3）は壊れない: トークン exp は `ack_wait * max_deliver`（= 150s）で計算する一方、
/// backoff 配列構成の最悪滞留は `Σ backoff`（= 80s）なので、exp は依然として保守側に長い。
fn effective_ack_wait(ack_wait: Duration, backoff: &[Duration]) -> Duration {
    backoff.first().copied().unwrap_or(ack_wait)
}

fn has_drift(actual: &async_nats::jetstream::consumer::Config, desired: &PullConfig) -> bool {
    // filter は単一形 / 複数形のどちらで入っているかが実装依存なので、集合として比較する。
    let norm = |single: &str, multi: &[String]| -> BTreeSet<String> {
        if multi.is_empty() {
            if single.is_empty() {
                BTreeSet::new()
            } else {
                BTreeSet::from([single.to_string()])
            }
        } else {
            multi.iter().cloned().collect()
        }
    };
    let actual_filters = norm(&actual.filter_subject, &actual.filter_subjects);
    let desired_filters = norm(&desired.filter_subject, &desired.filter_subjects);

    // metadata は NATS 側がサーバ由来のキー（_nats.* 等）を足すので、こちらが管理するキーだけ比べる。
    let managed_meta_differs = [META_TENANT_ID, META_LANE_CONCURRENCY, META_LANE_KIND]
        .iter()
        .any(|k| actual.metadata.get(*k) != desired.metadata.get(*k));

    actual_filters != desired_filters
        || effective_ack_wait(actual.ack_wait, &actual.backoff)
            != effective_ack_wait(desired.ack_wait, &desired.backoff)
        || actual.max_deliver != desired.max_deliver
        || actual.backoff != desired.backoff
        || actual.max_ack_pending != desired.max_ack_pending
        || managed_meta_differs
}

/// 削除してよいか（未処理が残っていないか）を確認する。
///
/// 未処理が残る lane を消すと、その分の配送が止まる（WorkQueue なのでメッセージ自体は
/// 残るが、購読者がいない subject になる）。残っていれば次周期へ繰り越す。
async fn is_drained(stream: &Stream, name: &str) -> bool {
    match stream.consumer_info(name).await {
        Ok(info) => info.num_pending == 0 && info.num_ack_pending == 0,
        // 情報が取れないときは安全側（削除しない）。
        Err(_) => false,
    }
}

/// reconcile の 1 パス。
///
/// 適用順序は **DELETE → UPDATE → CREATE** でなければならない (MUST)。WorkQueue では
/// filter が重なる consumer の作成をサーバが拒否するため、先に古い購読を外さないと
/// 新しい lane が作れない。逆に CREATE を先にすると一時的に filter が重なって失敗する。
/// **この順序制約は advisory lock 下でのみ意味を持つ**（複数インスタンスが交錯すると
/// 順序そのものが崩れる）。
pub async fn reconcile_lanes_once(state: &AppState) -> anyhow::Result<()> {
    // (0) 単一 writer を強制する。取れなければこのラウンドは他インスタンスに任せる。
    let Some(_guard) = state.try_lane_reconcile_lock().await? else {
        return Ok(());
    };

    // (1) 全体列挙の失敗はパス全体を Err にして次周期へ（reaper の誤り分離規約と同じ）。
    let tenants = db::list_active_tenants_with_quotas(state.pool()).await?;
    let stream = state
        .jetstream()
        .get_stream(faas_shared::INVOKE_STREAM_NAME)
        .await
        .context("get_stream(FAAS_INVOKE) for lane reconcile")?;
    let actual = collect_consumer_names(&stream).await?;

    // (2) 望ましいトポロジ。
    let topology = desired_topology(state, &tenants);
    let desired = topology.lanes();
    let desired_names: BTreeSet<String> = desired.iter().map(|l| l.name.clone()).collect();

    let mut changed = false;

    // (3) DELETE: 不要になった lane を消す。
    for name in actual.difference(&desired_names) {
        // ロールバック経路（lanes → legacy）では未処理 0 条件を**明示的に免除する**。
        // これをしないと legacy consumer を作れず（filter が重なる）ロールバックが完了しない。
        let must_drain = !topology.is_legacy();
        if must_drain && !is_drained(&stream, name).await {
            tracing::warn!(
                consumer = %name,
                "lane still has undelivered work; deferring deletion to the next round"
            );
            continue;
        }
        match stream.delete_consumer(name).await {
            Ok(_) => {
                tracing::info!(consumer = %name, "deleted lane consumer");
                changed = true;
            }
            Err(e) => {
                tracing::warn!(consumer = %name, error = %e, "failed to delete lane consumer")
            }
        }
    }

    // (4) UPDATE: 既存 lane の drift を是正する。**delete + recreate はしない**
    //     （M7 までの worker はそれをしており、DeliverPolicy::All と組み合わさって
    //      stream 全履歴の再配送を招きうる経路だった）。
    for lane in desired.iter().filter(|l| actual.contains(&l.name)) {
        let cfg = to_pull_config(state, lane);
        match stream.consumer_info(&lane.name).await {
            Ok(info) => {
                if has_drift(&info.config, &cfg) {
                    match stream.update_consumer(cfg).await {
                        Ok(_) => {
                            tracing::info!(consumer = %lane.name, "updated lane consumer (drift)");
                            changed = true;
                        }
                        Err(e) => tracing::warn!(
                            consumer = %lane.name, error = %e,
                            "failed to update lane consumer"
                        ),
                    }
                }
            }
            Err(e) => {
                tracing::warn!(consumer = %lane.name, error = %e, "failed to read consumer info")
            }
        }
    }

    // (5) CREATE: 足りない lane を作る。
    for lane in desired.iter().filter(|l| !actual.contains(&l.name)) {
        match stream.create_consumer(to_pull_config(state, lane)).await {
            Ok(_) => {
                tracing::info!(
                    consumer = %lane.name,
                    max_ack_pending = lane.max_ack_pending,
                    "created lane consumer"
                );
                changed = true;
            }
            Err(e) => {
                tracing::warn!(consumer = %lane.name, error = %e, "failed to create lane consumer")
            }
        }
    }

    // (6) enqueue ホットパスが「dedicated 枠に空きがあるか」を assign_lanes を評価せずに
    //     判定できるよう、reconcile が観測した dedicated lane 数を記録する。
    let dedicated = desired
        .iter()
        .filter(|l| faas_shared::tenant_from_lane_durable(&l.name).is_some())
        .count();
    state.set_dedicated_lane_count(dedicated);

    // (7) 変化があれば worker へ即時通知し、ensure キャッシュの世代をバンプする。
    if changed {
        state.bump_lane_generation();
        notify_lane_changed(state).await;
    }

    Ok(())
}

/// lane トポロジの変更を worker へ即時通知する (§3.7.4)。
///
/// **周期 discovery だけでは M6 の完了条件が壊れる**: 新規テナントの初回
/// `POST /invoke?wait=1` は「lane はあるが worker がまだ購読していない」窓に必ず当たり、
/// `SYNC_REPLY_TIMEOUT_MS`（既定 5 秒）内に reply が返らず 202 へ縮退する。
///
/// core NATS を使う（JetStream にしない）。取りこぼしても周期 discovery が収束させるので
/// durable 保証は不要で、stream を増やすと観測・運用の複雑度が上がる
/// （`.failed` を core にした M4c の判断と同じ論理）。
async fn notify_lane_changed(state: &AppState) {
    if let Err(e) = state
        .nats()
        .publish(faas_shared::LANE_CHANGED_SUBJECT, Vec::new().into())
        .await
    {
        // best-effort: 取りこぼしても周期 discovery が収束させる。
        tracing::warn!(error = %e, "failed to publish lane change notification");
    }
}

/// lane reconcile の背景ループ（`reaper::run_secret_kid_gauge` と同型）。
pub async fn run_lane_reconcile(state: AppState, interval_secs: u64) {
    let period = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    tracing::info!(
        interval_secs = period.as_secs(),
        lanes_enabled = state.tenant_lanes_enabled(),
        "lane reconciler started"
    );

    loop {
        ticker.tick().await;
        if let Err(e) = reconcile_lanes_once(&state).await {
            // best-effort: 1 周期の失敗で本流を止めない（次周期で再試行）。
            tracing::warn!(error = %e, "lane reconcile pass failed; will retry next interval");
        }
    }
}

/// enqueue のホットパスから呼ぶ lane ensure (§3.7.3)。
///
/// **publish は必ず lane 作成の後**という不変条件を立て、「メッセージは stream にあるが
/// 誰も購読していない」窓を消す。
///
/// 制約（レビュー指摘の反映）:
/// 1. **dedicated 枠に空きがあるテナントの dedicated lane 作成だけ**を行う。overflow lane の
///    `filter_subjects` は read-modify-write になり、CP N 台の同時更新が last-writer-wins で
///    互いの subject を消すため、**overflow は reconcile（単一 writer）だけが更新する**。
/// 2. overflow 対象になるテナントの enqueue では skip し、reconcile が最大 1 周期以内に
///    購読を張ることを遅延契約とする。
/// 3. キャッシュは世代カウンタとセットで、reconcile が lane を触ったら丸ごと捨てる
///    （「suspend → lane 削除 → 再 activate」の後にキャッシュヒットで二度と作り直さない、
///    という無音の暗転を消す）。
///
/// **fail-open**（`FailPolicy::TENANT_LANE`）: 失敗しても enqueue を止めない。止めると
/// NATS の一時的な不調でテナントのジョブが一切受け付けられなくなる。縮退は必ず記録する。
pub async fn ensure_lane_for_tenant(state: &AppState, tenant: &str) {
    if !state.tenant_lanes_enabled() {
        return;
    }
    if state.lane_cache_contains(tenant) {
        return;
    }

    // dedicated 枠に空きがあるときだけ作る（overflow は reconcile の担当）。
    if !state.has_dedicated_lane_capacity() {
        return;
    }

    let name = faas_shared::lane_durable(tenant);
    let overrides = match db::load_tenant_status_and_quotas(state.pool(), tenant).await {
        Ok(Some((_status, quotas))) => quotas,
        // テナントが引けない / 停止中でも enqueue 自体は既存の認証層が判断済みなので、
        // ここでは既定値で lane を作る（fail-open）。
        _ => db::TenantQuotaOverrides::default(),
    };
    let resolved = state.admission().resolve_for_tenant(tenant, &overrides);
    let lane = DesiredLane {
        name: name.clone(),
        filter_subjects: vec![faas_shared::invoke_subject(tenant)],
        max_ack_pending: resolved.lane_ack_pending(state.lane_ack_pending_headroom()),
        metadata: BTreeMap::from([
            (META_TENANT_ID.to_string(), tenant.to_string()),
            (
                META_LANE_CONCURRENCY.to_string(),
                resolved.lane_concurrency.to_string(),
            ),
            (META_LANE_KIND.to_string(), "dedicated".to_string()),
        ]),
    };

    let stream = match state
        .jetstream()
        .get_stream(faas_shared::INVOKE_STREAM_NAME)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                tenant = %tenant, error = %e, action = "lane_provision_degraded",
                "could not reach the invoke stream to ensure a tenant lane; \
                 enqueue proceeds (fail-open) and the reconciler will converge"
            );
            return;
        }
    };

    // 既に在れば作成はスキップし、キャッシュだけ立てる（他インスタンス / reconcile が作った場合）。
    match stream.consumer_info(&name).await {
        Ok(_) => {
            state.lane_cache_insert(tenant);
            return;
        }
        Err(_) => { /* 不在。作りに行く */ }
    }

    match stream.create_consumer(to_pull_config(state, &lane)).await {
        Ok(_) => {
            tracing::info!(tenant = %tenant, consumer = %name, "created tenant lane on enqueue");
            state.lane_cache_insert(tenant);
            notify_lane_changed(state).await;
        }
        Err(e) => {
            tracing::warn!(
                tenant = %tenant, consumer = %name, error = %e,
                action = "lane_provision_degraded",
                "failed to create tenant lane; enqueue proceeds (fail-open) and the \
                 reconciler will converge"
            );
        }
    }
}

// ============================================================================
// M8-8 (§5.2): backlog シグナル
// ============================================================================

/// 1 lane ぶんの観測値。
struct LaneDepth {
    name: String,
    num_pending: u64,
    num_ack_pending: u64,
}

/// 全 lane を 1 パス走査して深さを集める。
///
/// `Consumer::info()` は `&mut self` を要求し、`cached_info()` は作成時スナップショットで
/// ネットワークに出ないため、どちらもここでは使えない。`Stream::consumers()` の
/// ストリームが返す `consumer::Info` を直接読む。
async fn collect_lane_depths(stream: &Stream) -> anyhow::Result<Vec<LaneDepth>> {
    let mut out = Vec::new();
    let mut it = stream.consumers();
    while let Some(info) = it.next().await {
        let info = info.context("listing consumer info")?;
        let name = info
            .config
            .durable_name
            .clone()
            .unwrap_or_else(|| info.name.clone());
        if !is_managed_consumer(&name) {
            continue;
        }
        out.push(LaneDepth {
            name,
            num_pending: info.num_pending,
            num_ack_pending: info.num_ack_pending as u64,
        });
    }
    Ok(out)
}

/// backlog ポーラ (§5.2.3)。
///
/// # backlog の定義
///
/// ```text
/// backlog = Σ over all lanes ( num_pending + num_ack_pending )
/// ```
///
/// **`num_pending` 単体を使ってはならない**。1 worker が大量に claim した瞬間 `num_pending` は
/// 0 に落ちるが、仕事は終わっていない。`num_ack_pending` は「配送済みだが未 ack」= まさに
/// 未完了の仕事なので、両者の和が唯一正しい「未消化の仕事量」である。worker が突然死した
/// 場合も、抱えていたメッセージは `ack_wait` 経過まで `num_ack_pending` に残り backlog から
/// 消えない（シグナルとして堅牢）。
///
/// # scale-from-zero が成立する根拠
///
/// lane / legacy consumer を作るのは **CP** であり（§3.7）、durable consumer はクライアント
/// 接続と独立にサーバ側状態として残る。したがって **worker 0 台でも consumer 情報は読める**。
/// これが scale-from-zero の成立条件そのものである（M7 までのように worker が consumer を
/// 作る構造だと、worker 0 台 → consumer 無し → backlog 読めず → 増やす判断ができない、
/// というブートストラップ・デッドロックになる）。
pub async fn run_backlog_poller(state: AppState, interval_secs: u64) {
    let period = Duration::from_secs(interval_secs.max(1));
    let mut ticker = tokio::time::interval(period);
    tracing::info!(
        interval_secs = period.as_secs(),
        "jetstream backlog poller started"
    );

    let mut stream: Option<Stream> = None;
    let mut consecutive_failures: u64 = 0;

    loop {
        ticker.tick().await;

        if stream.is_none() {
            match state
                .jetstream()
                .get_stream(faas_shared::INVOKE_STREAM_NAME)
                .await
            {
                Ok(s) => stream = Some(s),
                Err(e) => {
                    note_backlog_failure(&state, &mut consecutive_failures, &e.to_string());
                    continue;
                }
            }
        }
        let Some(s) = stream.as_ref() else { continue };

        match collect_lane_depths(s).await {
            Ok(depths) => {
                consecutive_failures = 0;
                let m = state.metrics();
                // lane が消えたら系列も消す（reaper の kid gauge と同じ理由で reset してから set）。
                m.lane_pending_messages.reset();
                m.lane_ack_pending.reset();
                let mut backlog: u64 = 0;
                for d in &depths {
                    m.lane_pending_messages
                        .with_label_values(&[d.name.as_str()])
                        .set(d.num_pending as i64);
                    m.lane_ack_pending
                        .with_label_values(&[d.name.as_str()])
                        .set(d.num_ack_pending as i64);
                    backlog += d.num_pending + d.num_ack_pending;
                }
                let decision = state.observe_backlog(backlog, depths.len());
                m.scale_backlog.set(backlog as i64);
                m.scale_signal_age_seconds.set(0);
                m.scale_desired_workers.set(i64::from(decision.target));
            }
            Err(e) => {
                // ★ reaper の kid gauge と決定的に違う点: **backlog 側の gauge をリセットしない**。
                //   0 に落とすと「仕事が無い」と誤読され、NATS の一時的な瞬断がそのまま
                //   scale-to-zero を誘発する。代わりに age を伸ばし、判断ロジックを hold に落とす
                //   （§5.3 R2）。この非対称性は意図的である。
                state
                    .metrics()
                    .scale_signal_age_seconds
                    .set(state.scale_signal_age_secs().min(i64::MAX as u64) as i64);
                stream = None; // 次周期は get_stream からやり直す
                note_backlog_failure(&state, &mut consecutive_failures, &e.to_string());
            }
        }
    }
}

/// 観測失敗のログ抑制。1 回目は warn、以後は 60 周期に 1 回だけ出す。
///
/// 新規スタックでは「まだ stream / consumer が無い」状態が続くので、抑制が無いと
/// 起動直後のログが警告で埋まり、本当の異常が見えなくなる。
fn note_backlog_failure(state: &AppState, consecutive: &mut u64, error: &str) {
    *consecutive += 1;
    if *consecutive == 1 || (*consecutive).is_multiple_of(60) {
        tracing::warn!(
            error = %error,
            consecutive_failures = *consecutive,
            "failed to observe jetstream backlog; holding the previous signal"
        );
    }
    state
        .metrics()
        .scale_signal_age_seconds
        .set(state.scale_signal_age_secs().min(i64::MAX as u64) as i64);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(name: &str, filters: &[&str], ack: i64) -> PullConfig {
        PullConfig {
            durable_name: Some(name.to_string()),
            filter_subject: if filters.len() == 1 {
                filters[0].to_string()
            } else {
                String::new()
            },
            filter_subjects: if filters.len() == 1 {
                Vec::new()
            } else {
                filters.iter().map(|s| s.to_string()).collect()
            },
            max_ack_pending: ack,
            ..Default::default()
        }
    }

    fn actual_from(desired: &PullConfig) -> async_nats::jetstream::consumer::Config {
        use async_nats::jetstream::consumer::Config;
        Config {
            durable_name: desired.durable_name.clone(),
            filter_subject: desired.filter_subject.clone(),
            filter_subjects: desired.filter_subjects.clone(),
            max_ack_pending: desired.max_ack_pending,
            ack_wait: desired.ack_wait,
            max_deliver: desired.max_deliver,
            backoff: desired.backoff.clone(),
            metadata: desired.metadata.clone(),
            ..Default::default()
        }
    }

    /// 同一設定なら drift なし。
    #[test]
    fn no_drift_when_identical() {
        let d = desired("workers-t-ten_a", &["tenant.ten_a.component.invoke"], 28);
        assert!(!has_drift(&actual_from(&d), &d));
    }

    /// **回帰ガード**: `backoff` を設定すると NATS は `ack_wait` を `backoff[0]` から導出して
    /// 保存する。それを正規化せずに比べると **恒久的に drift と判定され続け**、reconcile が
    /// 毎周期 UPDATE を投げ、lane 世代をバンプし、全 worker に再 discovery を強いる
    /// 無限ループになる（実機で 10 秒ごとに発生しているのを観測して発見した）。
    #[test]
    fn server_derived_ack_wait_from_backoff_is_not_drift() {
        let mut d = desired("workers-t-ten_a", &["tenant.ten_a.component.invoke"], 28);
        d.ack_wait = Duration::from_secs(30);
        d.backoff = vec![
            Duration::from_secs(5),
            Duration::from_secs(15),
            Duration::from_secs(60),
        ];

        let mut a = actual_from(&d);
        // サーバが実際に保存する形（ack_wait = backoff[0]）。
        a.ack_wait = Duration::from_secs(5);

        assert!(
            !has_drift(&a, &d),
            "backoff[0] 由来の ack_wait を drift と誤判定してはならない"
        );
    }

    /// ただし **backoff が空**なら ack_wait はそのまま比較される（正規化で検出力を落とさない）。
    #[test]
    fn ack_wait_drift_still_detected_without_backoff() {
        let mut d = desired("workers-t-ten_a", &["tenant.ten_a.component.invoke"], 28);
        d.ack_wait = Duration::from_secs(30);
        d.backoff = Vec::new();

        let mut a = actual_from(&d);
        a.ack_wait = Duration::from_secs(5);

        assert!(
            has_drift(&a, &d),
            "backoff 無しの ack_wait 差は drift である"
        );
    }

    /// **filter 軸の drift を検出すること**。M7 までの判定にはこの軸が無く、
    /// 誤った filter の consumer が永久に是正されなかった。
    #[test]
    fn detects_filter_subject_drift() {
        let d = desired("workers-t-ten_a", &["tenant.ten_a.component.invoke"], 28);
        let mut a = actual_from(&d);
        a.filter_subject = "tenant.ten_b.component.invoke".to_string();
        assert!(
            has_drift(&a, &d),
            "filter の違いは必ず drift として検出する"
        );
    }

    /// 単一 filter と複数 filter の表現差は drift ではない（集合として比較する）。
    #[test]
    fn single_and_multi_filter_forms_are_equivalent() {
        let d = desired("workers-overflow", &["tenant.ten_a.component.invoke"], 1000);
        let mut a = actual_from(&d);
        // サーバが複数形で返してきたケース。
        a.filter_subject = String::new();
        a.filter_subjects = vec!["tenant.ten_a.component.invoke".to_string()];
        assert!(!has_drift(&a, &d));
    }

    /// overflow lane の filter が 1 本増減したら drift。
    #[test]
    fn detects_overflow_membership_drift() {
        let d = desired(
            "workers-overflow",
            &[
                "tenant.ten_a.component.invoke",
                "tenant.ten_b.component.invoke",
            ],
            1000,
        );
        let mut a = actual_from(&d);
        a.filter_subjects = vec!["tenant.ten_a.component.invoke".to_string()];
        assert!(has_drift(&a, &d));
    }

    /// max_ack_pending の drift（テナントのクォータ変更に追随する経路）。
    #[test]
    fn detects_max_ack_pending_drift() {
        let d = desired("workers-t-ten_a", &["tenant.ten_a.component.invoke"], 28);
        let mut a = actual_from(&d);
        a.max_ack_pending = 1000;
        assert!(has_drift(&a, &d));
    }

    /// metadata（worker へ配る実行クレジット）の drift。
    #[test]
    fn detects_metadata_drift() {
        let mut d = desired("workers-t-ten_a", &["tenant.ten_a.component.invoke"], 28);
        d.metadata
            .insert(META_LANE_CONCURRENCY.to_string(), "4".to_string());
        let mut a = actual_from(&d);
        a.metadata
            .insert(META_LANE_CONCURRENCY.to_string(), "8".to_string());
        assert!(has_drift(&a, &d));

        // サーバ由来の未知キーが増えていても drift とはみなさない。
        let mut a2 = actual_from(&d);
        a2.metadata
            .insert("_nats.server.version".to_string(), "2.10".to_string());
        assert!(!has_drift(&a2, &d));
    }

    /// 管理対象外の consumer を巻き込まない。
    #[test]
    fn only_manages_own_consumers() {
        assert!(is_managed_consumer(faas_shared::LEGACY_SHARED_DURABLE));
        assert!(is_managed_consumer(faas_shared::OVERFLOW_LANE_DURABLE));
        assert!(is_managed_consumer(&faas_shared::lane_durable("ten_a")));
        assert!(!is_managed_consumer("some-other-teams-consumer"));
        assert!(!is_managed_consumer("results-ingestor"));
    }
}
