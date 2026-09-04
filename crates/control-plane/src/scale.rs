//! M8 (§5.3): オートスケールの**判断ロジック**。
//!
//! このモジュールは**純関数と小さな状態だけ**で構成する。時計は引数 (`now_secs`) で受け取り、
//! 内部で `Instant::now()` を呼ばない。理由は 2 つある:
//!
//! 1. スケール判断は「backlog がこうなら何台」という単純な算術に見えて、実際には
//!    ヒステリシス・陳腐化・境界クランプが絡む状態機械である。時計を注入できないと
//!    CI で決定的に検証できず、**本番でしか壊れないバグ**が残る。
//! 2. 完了条件（§15 M8）は「レイテンシを劣化させない」であり、その最大の敵は
//!    flapping（増減の振動）である。振動は境界とヒステリシスの相互作用から生まれるので、
//!    そこを全列挙でテストできる形にしておくことが設計要件になる。
//!
//! 実際の観測（JetStream の consumer 走査）と露出（`GET /internal/scale`）は
//! 呼び出し側の責務であり、ここには持ち込まない。

/// スケール方針。すべて env 由来（`Config::from_env` が組み立てる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalePolicy {
    /// worker 1 台が引き受けられる未消化ジョブ数の見積り（desired の分母）。
    /// `WORKER_MAX_CONCURRENCY` と揃えるのが正しいが、**CP は worker の env を知らないので
    /// 検査できない**。既定を一致させ、ズレは gauge と chaos の前提 assert で潰す。
    pub jobs_per_worker: u32,
    /// 下限。`0` で scale-to-zero を許可する（opt-in）。
    pub min_workers: u32,
    /// 上限。
    pub max_workers: u32,
    /// scale-in のヒステリシス（秒）。
    pub scale_in_cooldown_secs: u64,
    /// この秒数より古いシグナルは「信用しない」。
    pub stale_after_secs: u64,
}

/// ポーラが観測した最新のシグナル。
#[derive(Debug, Clone, Copy)]
pub struct ScaleSignal {
    pub backlog: u64,
    /// 最後に**成功した**観測からの経過秒。
    pub age_secs: u64,
    /// 一度でも観測に成功したか。起動直後と「ずっと失敗している」を区別する。
    pub ever_observed: bool,
}

/// desired を出さずに現状維持する理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// まだ一度も観測できていない（起動直後 / NATS へ到達できない）。
    NoSignalYet,
    /// シグナルが古すぎる。
    StaleSignal,
    /// 減らす条件は満たしたが、まだクールダウン中。
    ScaleInCooldown,
}

impl HoldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSignalYet => "no_signal_yet",
            Self::StaleSignal => "stale_signal",
            Self::ScaleInCooldown => "scale_in_cooldown",
        }
    }
}

/// 判断結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub target: u32,
    pub hold: Option<HoldReason>,
}

/// 判断のための持ち越し状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleState {
    target: u32,
    /// `want < target` が続いている起点（秒）。戻ったら `None` に戻す。
    below_since_secs: Option<u64>,
}

impl ScaleState {
    /// **`Default` を導出してはならない。** 初期 target は 0 ではなく `min_workers` である。
    ///
    /// 0 から始めると「CP 再起動直後 + NATS 瞬断」という、実際に起きる組み合わせで
    /// desired 0 が出て**全台停止**する。起動直後は最も情報が無い瞬間なので、
    /// そこでの既定値は「安全側 = 下限」でなければならない。
    pub fn new(p: &ScalePolicy) -> Self {
        Self {
            target: p.min_workers,
            below_since_secs: None,
        }
    }

    /// 現在の目標台数。`ScaleSnapshot` の内部状態を外から読むための唯一の窓口
    /// （テストと将来の診断用。フィールドは private のまま保つ）。
    #[allow(dead_code)]
    pub fn target(&self) -> u32 {
        self.target
    }
}

/// backlog から desired worker 数を決める。
///
/// 規則は §5.3 の R0〜R6。**あらゆる return 経路が R0（境界クランプ）を通る**ことが
/// 事後条件であり、hold 経路も例外にしない。
pub fn decide(p: &ScalePolicy, sig: &ScaleSignal, st: &mut ScaleState, now_secs: u64) -> Decision {
    // R0 をあらゆる経路で通すためのヘルパー。early return のたびにクランプを書くと
    // 必ずどこかで書き忘れるので、出口を 1 つに絞る。
    let clamp = |v: u32| v.clamp(p.min_workers, p.max_workers.max(p.min_workers));

    // R1: 一度も観測できていないなら、下限に張り付けて hold する。
    if !sig.ever_observed {
        st.target = clamp(p.min_workers);
        st.below_since_secs = None;
        return Decision {
            target: st.target,
            hold: Some(HoldReason::NoSignalYet),
        };
    }

    // R2: シグナルが古い。**現在の target を保持する**（0 に落とさない）。
    // 直前まで 4 台で回っていた系が NATS 瞬断で全台停止する、という事故を防ぐ。
    if sig.age_secs > p.stale_after_secs {
        st.target = clamp(st.target);
        st.below_since_secs = None;
        return Decision {
            target: st.target,
            hold: Some(HoldReason::StaleSignal),
        };
    }

    // R3: 生の目標。`div_ceil` なので backlog が 1 でも 1 台は要ると判断する。
    let raw = sig.backlog.div_ceil(u64::from(p.jobs_per_worker.max(1)));
    let want = clamp(u32::try_from(raw).unwrap_or(u32::MAX));

    if want > st.target {
        // R4: scale-out は即時。遅らせることはレイテンシへの直撃であり、
        // 増やしすぎのコストは後で減らせば済む（非対称なので非対称に扱う）。
        st.target = want;
        st.below_since_secs = None;
        return Decision {
            target: st.target,
            hold: None,
        };
    }

    if want < st.target {
        // R5: scale-in はヒステリシス。`below_since` を立て、cooldown を満たして初めて反映する。
        let since = *st.below_since_secs.get_or_insert(now_secs);
        if now_secs.saturating_sub(since) >= p.scale_in_cooldown_secs {
            st.target = want;
            st.below_since_secs = None;
            return Decision {
                target: st.target,
                hold: None,
            };
        }
        // まだ待つ。desired 自体は出す（= 現在の target）。止めるのではなく「減らさない」。
        st.target = clamp(st.target);
        return Decision {
            target: st.target,
            hold: Some(HoldReason::ScaleInCooldown),
        };
    }

    // want == target。揺り戻したので cooldown の計時をやめる。
    st.below_since_secs = None;
    st.target = clamp(st.target);
    Decision {
        target: st.target,
        hold: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(min: u32, max: u32) -> ScalePolicy {
        ScalePolicy {
            jobs_per_worker: 32,
            min_workers: min,
            max_workers: max,
            scale_in_cooldown_secs: 60,
            stale_after_secs: 30,
        }
    }

    fn fresh(backlog: u64) -> ScaleSignal {
        ScaleSignal {
            backlog,
            age_secs: 0,
            ever_observed: true,
        }
    }

    /// backlog が増えて desired が減ることは絶対に無い。
    /// これが破れると「負荷が上がったのに台数を減らす」= 完了条件を機構自身が破る。
    #[test]
    fn desired_is_monotone_in_backlog() {
        let p = policy(0, 64);
        let mut last = 0;
        for backlog in 0..=4096u64 {
            // 各点を独立に評価する（履歴の影響を排除して、写像そのものの単調性を見る）。
            let mut st = ScaleState::new(&p);
            st.target = 0;
            let d = decide(&p, &fresh(backlog), &mut st, 0);
            assert!(d.target >= last, "backlog={backlog}: {} < {last}", d.target);
            last = d.target;
        }
    }

    /// R0: **hold 経路を含む**あらゆる出口で `min <= target <= max`。
    #[test]
    fn desired_never_leaves_bounds() {
        for (min, max) in [(0u32, 1u32), (0, 4), (1, 4), (2, 2), (3, 8)] {
            let p = policy(min, max);
            for backlog in [0u64, 1, 31, 32, 33, 1000, u64::MAX] {
                for (age, ever) in [(0u64, true), (0, false), (999, true)] {
                    let mut st = ScaleState::new(&p);
                    let sig = ScaleSignal {
                        backlog,
                        age_secs: age,
                        ever_observed: ever,
                    };
                    let d = decide(&p, &sig, &mut st, 0);
                    assert!(
                        d.target >= min && d.target <= max,
                        "min={min} max={max} backlog={backlog} age={age} ever={ever} -> {}",
                        d.target
                    );
                }
            }
        }
    }

    /// `jobs_per_worker` の倍数ちょうどと +1 の境界で必ず 1 台増える（div_ceil の意味論）。
    #[test]
    fn ceil_boundary_increments_at_each_multiple() {
        let p = policy(0, 1024);
        let jpw = u64::from(p.jobs_per_worker);
        for k in 0..32u64 {
            let mut a = ScaleState::new(&p);
            a.target = 0;
            let at_multiple = decide(&p, &fresh(k * jpw), &mut a, 0).target;

            let mut b = ScaleState::new(&p);
            b.target = 0;
            let past_multiple = decide(&p, &fresh(k * jpw + 1), &mut b, 0).target;

            assert_eq!(
                past_multiple,
                at_multiple + 1,
                "k={k}: {at_multiple} -> {past_multiple}"
            );
        }
    }

    /// R4: 増やす側に cooldown は無い。
    #[test]
    fn scale_out_is_immediate() {
        let p = policy(1, 8);
        let mut st = ScaleState::new(&p);
        let d = decide(&p, &fresh(256), &mut st, 0);
        assert_eq!(d.target, 8);
        assert_eq!(d.hold, None);
    }

    /// R5: 減らす側は cooldown を満たすまで反映しない。途中で戻ったら計時もリセットされる。
    #[test]
    fn scale_in_requires_cooldown() {
        let p = policy(0, 8);
        let mut st = ScaleState::new(&p);

        assert_eq!(decide(&p, &fresh(256), &mut st, 0).target, 8);

        // backlog が消えても、cooldown 未満では減らない。
        let d = decide(&p, &fresh(0), &mut st, 10);
        assert_eq!(d.target, 8);
        assert_eq!(d.hold, Some(HoldReason::ScaleInCooldown));

        // 途中で負荷が戻ると計時はリセットされる。
        assert_eq!(decide(&p, &fresh(256), &mut st, 20).target, 8);

        // 改めて下がり始め、cooldown を満たして初めて反映される。
        assert_eq!(
            decide(&p, &fresh(0), &mut st, 30).hold,
            Some(HoldReason::ScaleInCooldown)
        );
        let d = decide(&p, &fresh(0), &mut st, 30 + 60);
        assert_eq!(d.target, 0);
        assert_eq!(d.hold, None);
    }

    /// `ScaleState::new` の初期 target は 0 ではなく `min_workers`。
    #[test]
    fn initial_state_targets_min_workers() {
        for min in [0u32, 1, 3] {
            let p = policy(min, 8);
            assert_eq!(ScaleState::new(&p).target(), min);
        }
    }

    /// **回帰ガード**: 「CP 再起動直後 + NATS 断」が全台停止に化けないこと。
    /// この 1 本が M8 の運用リスクの中心である。
    #[test]
    fn fresh_state_with_stale_signal_returns_min_not_zero() {
        let p = policy(2, 8);
        let mut st = ScaleState::new(&p);

        // 一度も観測できていない。
        let d = decide(
            &p,
            &ScaleSignal {
                backlog: 0,
                age_secs: 0,
                ever_observed: false,
            },
            &mut st,
            0,
        );
        assert_eq!(d.target, 2);
        assert_eq!(d.hold, Some(HoldReason::NoSignalYet));

        // 観測できたが古い。
        let d = decide(
            &p,
            &ScaleSignal {
                backlog: 0,
                age_secs: 9999,
                ever_observed: true,
            },
            &mut st,
            100,
        );
        assert_eq!(d.target, 2);
        assert_eq!(d.hold, Some(HoldReason::StaleSignal));
    }

    /// R0 と R2 の相互作用: stale hold でも下限を割らない。
    #[test]
    fn stale_hold_never_returns_below_min_workers() {
        let p = policy(3, 8);
        let mut st = ScaleState::new(&p);
        st.target = 0; // 不正な内部状態を人為的に作っても、出口のクランプが直す。
        let d = decide(
            &p,
            &ScaleSignal {
                backlog: 0,
                age_secs: p.stale_after_secs + 1,
                ever_observed: true,
            },
            &mut st,
            0,
        );
        assert_eq!(d.target, 3);
    }

    /// backlog が u64 の極大でも panic せず上限に張り付く（`u32::try_from` の失敗経路）。
    #[test]
    fn absurd_backlog_saturates_at_max_workers() {
        let p = policy(1, 4);
        let mut st = ScaleState::new(&p);
        assert_eq!(decide(&p, &fresh(u64::MAX), &mut st, 0).target, 4);
    }
}
