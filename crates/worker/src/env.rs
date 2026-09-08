//! ゲストへ注入する環境変数の組み立て (M7b/M7c, §4.4 / §15)。
//!
//! **このファイルは `scripts/rls-lint.sh` の検査 (4) の allowlist に入る**（`Redacted::expose()` を
//! 呼べる worker 側の唯一の場所）。worker/main.rs（1700 行超）を allowlist に入れるとガードが
//! 実質無効になるため、env の組み立てだけをここへ切り出している。
//!
//! ## 不変条件
//!
//! 1. **許可リストが権威**。`component_versions.capabilities.env`（admin 承認）に載っていないキーは、
//!    値が DB に存在しても注入しない。CP 側（job-env 引き換え）でも同じフィルタを掛ける二重防御で、
//!    片側の実装ミスが即漏洩にならないようにする。
//! 2. **上限は `hibana_shared` の定数を共有する**。CP の受付バリデーションと同じ値でここでも clamp する
//!    （DB を直接書き換えられた場合や将来の別経路に対する防御）。
//! 3. **出力順は決定的**（キー名昇順）。同じ入力からは常に同じ `WasiCtx` が組み上がる。
//! 4. `WasiCtx` は実行ごとに構築される（`Component` は sha256 キャッシュで共有されるが `WasiCtx` は
//!    共有されない）ため、**テナント混線は構造的に起きない**。

use std::collections::{BTreeMap, BTreeSet};

use hibana_shared::{
    Redacted, MAX_ENV_VALUE_BYTES, MAX_FUNCTION_ENV_KEYS, MAX_FUNCTION_ENV_TOTAL_BYTES,
};

/// 組み立て結果と、落としたものの内訳（観測用）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuiltEnv {
    /// `WasiCtxBuilder::envs` へそのまま渡すペア列（キー名昇順）。
    pub pairs: Vec<(String, String)>,
    /// 許可リスト外で落としたキー数。
    pub dropped_unapproved: usize,
    /// 上限（件数 / 値長 / 総バイト）で落としたキー数。
    pub dropped_over_limit: usize,
    /// secret 由来のキーが 1 つ以上含まれるか。`true` のとき呼び出し側はゲスト stderr を
    /// 共有ログへ流さない（`inherit_stderr` を使わない）。
    pub has_secret: bool,
}

/// 平文 config と復号済み secret を許可リストで畳んで、注入するペア列を作る。
///
/// `config` / `secrets` はどちらもキー名 → 値。衝突は CP 側の受付が 409 で防いでいるが、
/// 万一同名が来た場合は **secret を優先**する（「secret を設定したのに平文 config が注入される」
/// という最悪の取り違えを避ける。CP が防いでいるので実際には到達しない防御的分岐）。
pub fn build_env(
    config: &BTreeMap<String, String>,
    secrets: &BTreeMap<String, Redacted<String>>,
    allowed: &BTreeSet<String>,
) -> BuiltEnv {
    let mut out = BuiltEnv::default();

    // secret 優先でマージする（同名は secret が勝つ）。
    let mut merged: BTreeMap<&str, (&str, bool)> = BTreeMap::new();
    for (k, v) in config {
        merged.insert(k.as_str(), (v.as_str(), false));
    }
    for (k, v) in secrets {
        merged.insert(k.as_str(), (v.expose().as_str(), true));
    }

    let mut total = 0usize;
    for (key, (value, is_secret)) in merged {
        // (1) 許可リスト（admin 承認）が権威。
        if !allowed.contains(key) {
            out.dropped_unapproved += 1;
            continue;
        }
        // (2) 上限の防御的 clamp。CP が受付で弾いているので通常は到達しない。
        if value.len() > MAX_ENV_VALUE_BYTES || out.pairs.len() >= MAX_FUNCTION_ENV_KEYS {
            out.dropped_over_limit += 1;
            continue;
        }
        let next_total = total + key.len() + value.len();
        if next_total > MAX_FUNCTION_ENV_TOTAL_BYTES {
            out.dropped_over_limit += 1;
            continue;
        }
        total = next_total;
        out.has_secret |= is_secret;
        out.pairs.push((key.to_string(), value.to_string()));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn sec(pairs: &[(&str, &str)]) -> BTreeMap<String, Redacted<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Redacted::new(v.to_string())))
            .collect()
    }

    fn allow(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// 許可リストに無いキーは、値が存在しても注入されない（許可リストが権威）。
    #[test]
    fn unapproved_keys_are_dropped() {
        let built = build_env(
            &cfg(&[("LOG_LEVEL", "debug"), ("SNEAKY", "nope")]),
            &sec(&[("API_KEY", "s3cr3t")]),
            &allow(&["LOG_LEVEL", "API_KEY"]),
        );
        let keys: Vec<&str> = built.pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["API_KEY", "LOG_LEVEL"]);
        assert_eq!(built.dropped_unapproved, 1);
    }

    /// 許可リストが空（deny-all）なら 1 つも注入しない。
    #[test]
    fn empty_allowlist_injects_nothing() {
        let built = build_env(
            &cfg(&[("LOG_LEVEL", "debug")]),
            &sec(&[("API_KEY", "s3cr3t")]),
            &BTreeSet::new(),
        );
        assert!(built.pairs.is_empty());
        assert!(!built.has_secret);
        assert_eq!(built.dropped_unapproved, 2);
    }

    /// 出力順はキー名昇順で決定的。
    #[test]
    fn output_order_is_deterministic() {
        let built = build_env(
            &cfg(&[("ZULU", "z"), ("ALPHA", "a"), ("MIKE", "m")]),
            &BTreeMap::new(),
            &allow(&["ZULU", "ALPHA", "MIKE"]),
        );
        let keys: Vec<&str> = built.pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["ALPHA", "MIKE", "ZULU"]);
    }

    /// secret が 1 つでも入れば `has_secret` が立つ（stderr 封じ込めの判断材料）。
    #[test]
    fn has_secret_tracks_secret_presence() {
        let only_config = build_env(
            &cfg(&[("LOG_LEVEL", "debug")]),
            &BTreeMap::new(),
            &allow(&["LOG_LEVEL"]),
        );
        assert!(!only_config.has_secret);

        let with_secret = build_env(
            &cfg(&[("LOG_LEVEL", "debug")]),
            &sec(&[("API_KEY", "x")]),
            &allow(&["LOG_LEVEL", "API_KEY"]),
        );
        assert!(with_secret.has_secret);
    }

    /// 同名衝突は secret が勝つ（CP が 409 で防いでいるが防御的に固定する）。
    #[test]
    fn secret_wins_on_key_collision() {
        let built = build_env(
            &cfg(&[("API_KEY", "plaintext-from-config")]),
            &sec(&[("API_KEY", "from-secret")]),
            &allow(&["API_KEY"]),
        );
        assert_eq!(built.pairs, vec![("API_KEY".into(), "from-secret".into())]);
        assert!(built.has_secret);
    }

    /// 値長の上限を超えるものは落とす（DB 直書き等への防御的 clamp）。
    #[test]
    fn oversized_values_are_dropped() {
        let big = "x".repeat(MAX_ENV_VALUE_BYTES + 1);
        let built = build_env(
            &cfg(&[("BIG", big.as_str()), ("OK", "small")]),
            &BTreeMap::new(),
            &allow(&["BIG", "OK"]),
        );
        assert_eq!(built.pairs, vec![("OK".into(), "small".into())]);
        assert_eq!(built.dropped_over_limit, 1);
    }

    /// キー数の上限で打ち切る。
    #[test]
    fn key_count_is_capped() {
        let entries: Vec<(String, String)> = (0..MAX_FUNCTION_ENV_KEYS + 10)
            .map(|i| (format!("K{i:03}"), "v".to_string()))
            .collect();
        let config: BTreeMap<String, String> = entries.iter().cloned().collect();
        let allowed: BTreeSet<String> = entries.iter().map(|(k, _)| k.clone()).collect();
        let built = build_env(&config, &BTreeMap::new(), &allowed);
        assert_eq!(built.pairs.len(), MAX_FUNCTION_ENV_KEYS);
        assert_eq!(built.dropped_over_limit, 10);
    }

    /// 総バイト数の上限で打ち切る。
    #[test]
    fn total_bytes_are_capped() {
        // 1 件あたり約 2 KiB を 32 件 = 64 KiB > 32 KiB。
        let value = "v".repeat(2048);
        let entries: Vec<(String, String)> = (0..32)
            .map(|i| (format!("K{i:02}"), value.clone()))
            .collect();
        let config: BTreeMap<String, String> = entries.iter().cloned().collect();
        let allowed: BTreeSet<String> = entries.iter().map(|(k, _)| k.clone()).collect();
        let built = build_env(&config, &BTreeMap::new(), &allowed);
        let total: usize = built.pairs.iter().map(|(k, v)| k.len() + v.len()).sum();
        assert!(
            total <= MAX_FUNCTION_ENV_TOTAL_BYTES,
            "total {total} must stay within the cap"
        );
        assert!(built.dropped_over_limit > 0);
    }
}
