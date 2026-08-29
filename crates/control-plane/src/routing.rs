//! canary ルーティングの純関数 (M7a, §6.7 / §15)。
//!
//! **DB / 時刻 / 乱数に依存しない**。分配の数学だけをここに閉じ、DB-free ユニットテストで
//! 全列挙検証する（`cron.rs` / `store.rs` のテスト作法と同じ）。
//!
//! 設計の要点は 2 つ:
//!
//! 1. **乱数を使わない**。ルーティングキーの SHA-256 からバケット（0..=99）を決定的に導出する。
//!    これにより (a) 同一 Idempotency-Key の再送・cron の同一 slot 再発火・chain の再配送が
//!    常に同じ版へ落ち、(b) 分配比率の正しさを DB 無しの全列挙テストで**証明**できる。
//! 2. **端点を特別扱いしない**。選択規則は `bucket < canary_weight` の単一不等式のみ。
//!    `weight == 0` / `weight == 100` の分岐を書くと、そこが「rollback したのに canary へ
//!    流れ続ける」バグの温床になる。

/// version 決定理由（`executions.routing_reason` へそのまま保存する安定文字列）。
///
/// `components` は可変なので `version_id` だけでは「昇格後に stable になった版が canary として
/// 選ばれた実行」を後から区別できない。実行時点のスナップショットとして記録する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingReason {
    Stable,
    Canary,
}

impl RoutingReason {
    /// DB / メトリクスラベルへ渡す安定文字列。`executions_routing_reason_chk`（migration 0009）の
    /// 値域と 1 対 1 に対応する。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Canary => "canary",
        }
    }
}

/// `components` 1 行から解決したルーティング設定（`db::resolve_component_routing` が返す）。
pub struct ComponentRouting {
    pub component_id: String,
    /// stable 側（`components.active_version_id`）。
    pub stable: crate::db::ActiveVersion,
    /// canary 側。soft delete 済み / ポインタ不整合（別 component・別テナント）なら `None`
    /// ＝ fail-safe に全量 stable へ倒れる。
    pub canary: Option<crate::db::ActiveVersion>,
    /// 0..=100。DB の CHECK（migration 0009）で保証されるが、防御的に clamp して読む。
    pub canary_weight: u8,
}

/// ルーティングキーを 0..=99 のバケットへ決定的に写像する。
///
/// ドメイン分離のプレフィクスを入れ、`invoke_request_hash_for` や `cron_idempotency_key` と
/// 同じ入力でも別の値になるようにする（冪等キーの hash と相関させない）。
pub fn routing_bucket(component_id: &str, routing_key: &str) -> u8 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"faas:canary:v1\0");
    h.update(component_id.as_bytes());
    h.update(b"\0");
    h.update(routing_key.as_bytes());
    let d = h.finalize();
    // 先頭 4 バイト（2^32）を 100 で割る剰余バイアスは 2^-32 オーダーで無視できる。
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    (n % 100) as u8
}

/// 唯一の選択規則: `bucket < canary_weight` なら canary、それ以外は stable。
///
/// 端点は特別扱いしない（数学的に自然に成立する）:
///  - weight = 0   → bucket(0..=99) < 0 は常に偽 → 常に stable
///  - weight = 100 → bucket(0..=99) < 100 は常に真 → 常に canary
///  - canary = None（未設定 / soft delete 済み / ポインタ不整合）→ 重みに関わらず stable（fail-safe）
pub fn select_version(
    r: &ComponentRouting,
    bucket: u8,
) -> (&crate::db::ActiveVersion, RoutingReason) {
    match &r.canary {
        Some(c) if bucket < r.canary_weight => (c, RoutingReason::Canary),
        _ => (&r.stable, RoutingReason::Stable),
    }
}

/// `X-Faas-Routing-Key` ヘッダの上限バイト長（sticky ルーティング用の任意キー）。
pub const MAX_ROUTING_KEY_LEN: usize = 128;

/// `X-Faas-Routing-Key` を検証する純関数（DB / ストア非依存）。
///
/// 受理するのは **1..=128 バイトの印字可能 ASCII**（0x21..=0x7e）のみ。空・制御文字・空白・
/// 非 ASCII を拒む理由は、この値がバケット導出にしか使われない不透明トークンであり、
/// ログ / ヘッダ経路へ制御文字を持ち込む理由が無いため。
pub fn validate_routing_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_ROUTING_KEY_LEN
        && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ActiveVersion;

    fn version(id: &str) -> ActiveVersion {
        ActiveVersion {
            version_id: id.to_string(),
            version: "1.0.0".to_string(),
            storage_uri: format!("tenants/t/components/{id}.wasm"),
            wasm_sha256: "0".repeat(64),
            max_wall_time_ms: 5_000,
        }
    }

    fn routing(weight: u8, with_canary: bool) -> ComponentRouting {
        ComponentRouting {
            component_id: "cmp_test".to_string(),
            stable: version("ver_stable"),
            canary: with_canary.then(|| version("ver_canary")),
            canary_weight: weight,
        }
    }

    // weight=0（既定・canary 未設定と同義）は bucket を全列挙しても必ず stable。
    // ＝ M6 までと同一の解決になることの根拠。
    #[test]
    fn weight_zero_is_always_stable() {
        let r = routing(0, true);
        for bucket in 0..=99u8 {
            let (v, reason) = select_version(&r, bucket);
            assert_eq!(reason, RoutingReason::Stable, "bucket {bucket}");
            assert_eq!(v.version_id, "ver_stable");
        }
    }

    // weight=100（完全移行）は bucket を全列挙しても必ず canary。
    #[test]
    fn weight_100_is_always_canary() {
        let r = routing(100, true);
        for bucket in 0..=99u8 {
            let (v, reason) = select_version(&r, bucket);
            assert_eq!(reason, RoutingReason::Canary, "bucket {bucket}");
            assert_eq!(v.version_id, "ver_canary");
        }
    }

    // **分配比率の完全証明**: 任意の weight w について、0..=99 の 100 バケットのうち
    // canary になるのはちょうど w 個。完了条件「10% → 50% → 100% の段階移行」はここで閉じる。
    #[test]
    fn weight_equals_canary_bucket_count() {
        for w in 0..=100u8 {
            let r = routing(w, true);
            let canary_count = (0..=99u8)
                .filter(|&b| select_version(&r, b).1 == RoutingReason::Canary)
                .count();
            assert_eq!(
                canary_count, w as usize,
                "weight {w} must route exactly {w}/100 buckets to canary"
            );
        }
    }

    // bucket を固定して weight を 0→100 へ上げたとき、Stable→Canary の遷移は高々 1 回。
    // 逆流（一度 canary になった bucket が weight を上げると stable に戻る）が無いことの固定。
    // rollback（weight を下げる）側も同じ単調性の裏返しで保証される。
    #[test]
    fn selection_is_monotone_in_weight() {
        for bucket in 0..=99u8 {
            let mut transitions = 0;
            let mut prev = RoutingReason::Stable;
            for w in 0..=100u8 {
                let reason = select_version(&routing(w, true), bucket).1;
                if reason != prev {
                    transitions += 1;
                    // 遷移方向は Stable → Canary のみ。
                    assert_eq!(prev, RoutingReason::Stable, "bucket {bucket} flipped back");
                    prev = reason;
                }
            }
            assert!(
                transitions <= 1,
                "bucket {bucket} must flip at most once, got {transitions}"
            );
        }
    }

    // canary ポインタが解決できない（soft delete 済み / 別 component / 別テナント）なら
    // weight に関わらず全量 stable（fail-safe）。
    #[test]
    fn missing_canary_falls_back_to_stable() {
        for w in 0..=100u8 {
            let r = routing(w, false);
            for bucket in 0..=99u8 {
                let (v, reason) = select_version(&r, bucket);
                assert_eq!(reason, RoutingReason::Stable, "weight {w} bucket {bucket}");
                assert_eq!(v.version_id, "ver_stable");
            }
        }
    }

    // 決定性・値域・component ごとの独立性。
    #[test]
    fn bucket_is_deterministic_and_in_range() {
        for i in 0..1_000 {
            let key = format!("key-{i}");
            let a = routing_bucket("cmp_a", &key);
            let b = routing_bucket("cmp_a", &key);
            assert_eq!(a, b, "must be deterministic");
            assert!(a <= 99, "bucket must be in 0..=99, got {a}");
        }
        // 同一 key でも component が違えば独立（1 テナントが複数 component で同じ側に
        // 偏り続けることを避ける）。1000 本引いて 1 本でも違えば独立性は示せる。
        let differs = (0..1_000)
            .map(|i| format!("key-{i}"))
            .any(|k| routing_bucket("cmp_a", &k) != routing_bucket("cmp_b", &k));
        assert!(differs, "bucket must depend on component_id");
    }

    // golden vector: ハッシュ式（ドメインタグ・連結順・先頭 4 バイト BE・mod 100）のどれか
    // 1 つでも変われば落ちる。式を変えると既存の sticky 割当が全部ずれるため、変更は
    // 「ルーティングの再シャッフル」であることを明示的に意識させる。
    #[test]
    fn bucket_matches_golden_vectors() {
        assert_eq!(routing_bucket("cmp_echo", "exec_1"), 96);
        assert_eq!(routing_bucket("cmp_echo", "user-42"), 67);
        assert_eq!(routing_bucket("cmp_other", "user-42"), 85);
    }

    // ドメイン分離プレフィクスが実際に効いていること（冪等キー hash と相関させない回帰ガード）。
    #[test]
    fn domain_prefix_changes_the_bucket() {
        // プレフィクス無し版をテスト内に複製する。
        fn bucket_without_prefix(component_id: &str, routing_key: &str) -> u8 {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(component_id.as_bytes());
            h.update(b"\0");
            h.update(routing_key.as_bytes());
            let d = h.finalize();
            (u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 100) as u8
        }
        let differs = (0..200)
            .map(|i| format!("k{i}"))
            .any(|k| routing_bucket("cmp_echo", &k) != bucket_without_prefix("cmp_echo", &k));
        assert!(
            differs,
            "the domain-separation prefix must actually change the mapping"
        );
    }

    // 分布の一様性。入力は決定的（`k{i}`）なのでフレークしない。
    #[test]
    fn bucket_distribution_is_roughly_uniform() {
        let mut counts = [0usize; 100];
        for i in 0..10_000 {
            counts[routing_bucket("cmp_echo", &format!("k{i}")) as usize] += 1;
        }
        for (bucket, &n) in counts.iter().enumerate() {
            assert!(
                (60..=140).contains(&n),
                "bucket {bucket} got {n} of 10000 (expected 100 ± 40)"
            );
        }
    }

    // `X-Faas-Routing-Key` バリデータの境界。
    #[test]
    fn routing_key_validation() {
        assert!(validate_routing_key("u-42"));
        assert!(validate_routing_key(&"a".repeat(MAX_ROUTING_KEY_LEN)));

        assert!(!validate_routing_key(""), "empty must be rejected");
        assert!(
            !validate_routing_key(&"a".repeat(MAX_ROUTING_KEY_LEN + 1)),
            "over-long must be rejected"
        );
        assert!(
            !validate_routing_key("ユーザ"),
            "non-ASCII must be rejected"
        );
        assert!(!validate_routing_key("a b"), "space must be rejected");
        assert!(
            !validate_routing_key("a\nb"),
            "control char must be rejected"
        );
        assert!(!validate_routing_key("a\0b"), "NUL must be rejected");
    }
}
