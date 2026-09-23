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
//! 2. **上限は `hibana_shared` の定数を共有する**。上限超過は実行前に拒否し、
//!    許可された設定を部分的に省略したままゲストを実行しない。
//! 3. **出力順は決定的**（キー名昇順）。同じ入力からは常に同じ `WasiCtx` が組み上がる。
//! 4. `WasiCtx` は実行ごとに構築される（`Component` は sha256 キャッシュで共有されるが `WasiCtx` は
//!    共有されない）ため、**テナント混線は構造的に起きない**。

use std::collections::{BTreeMap, BTreeSet};

use hibana_shared::{
    Redacted, MAX_ENV_VALUE_BYTES, MAX_FUNCTION_ENV_KEYS, MAX_FUNCTION_ENV_TOTAL_BYTES,
};

/// 組み立て結果と、落としたものの内訳（観測用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltEnv {
    /// `WasiCtxBuilder::envs` へそのまま渡すペア列（キー名昇順）。
    pub pairs: Vec<(String, String)>,
    /// 許可リスト外で落としたキー数。
    pub dropped_unapproved: usize,
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
) -> Result<BuiltEnv, &'static str> {
    let mut out = BuiltEnv::default();

    // secret 優先でマージする（同名は secret が勝つ）。
    let mut merged: BTreeMap<&str, &str> = BTreeMap::new();
    for (k, v) in config {
        merged.insert(k.as_str(), v.as_str());
    }
    for (k, v) in secrets {
        merged.insert(k.as_str(), v.expose().as_str());
    }

    let mut total = 0usize;
    for (key, value) in merged {
        // (1) 許可リスト（admin 承認）が権威。
        if !allowed.contains(key) {
            out.dropped_unapproved += 1;
            continue;
        }
        // Reject the entire environment, including legacy or corrupted data.
        // Never include names, values or Secret lengths in the diagnostic.
        if value.len() > MAX_ENV_VALUE_BYTES || out.pairs.len() >= MAX_FUNCTION_ENV_KEYS {
            return Err("Environment exceeds the per-value or entry-count limit");
        }
        let next_total = total + key.len() + value.len();
        if next_total > MAX_FUNCTION_ENV_TOTAL_BYTES {
            return Err("Vars and Secrets together exceed the environment size limit");
        }
        total = next_total;
        out.pairs.push((key.to_string(), value.to_string()));
    }

    Ok(out)
}

/// Compare test fixtures at the existing plaintext boundary without logging them.
#[cfg(test)]
pub(crate) fn assert_secret_eq(actual: &Redacted<String>, expected: &str) {
    assert!(actual.expose() == expected, "secret does not match fixture");
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
        )
        .unwrap();
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
        )
        .unwrap();
        assert!(built.pairs.is_empty());
        assert_eq!(built.dropped_unapproved, 2);
    }

    /// 出力順はキー名昇順で決定的。
    #[test]
    fn output_order_is_deterministic() {
        let built = build_env(
            &cfg(&[("ZULU", "z"), ("ALPHA", "a"), ("MIKE", "m")]),
            &BTreeMap::new(),
            &allow(&["ZULU", "ALPHA", "MIKE"]),
        )
        .unwrap();
        let keys: Vec<&str> = built.pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["ALPHA", "MIKE", "ZULU"]);
    }

    /// 同名衝突は secret が勝つ（CP が 409 で防いでいるが防御的に固定する）。
    #[test]
    fn secret_wins_on_key_collision() {
        let built = build_env(
            &cfg(&[("API_KEY", "plaintext-from-config")]),
            &sec(&[("API_KEY", "from-secret")]),
            &allow(&["API_KEY"]),
        )
        .unwrap();
        assert_eq!(built.pairs, vec![("API_KEY".into(), "from-secret".into())]);
    }

    /// 上限超過時は、残りの設定だけで実行を続けない。
    #[test]
    fn oversized_values_reject_the_environment() {
        let big = "x".repeat(MAX_ENV_VALUE_BYTES + 1);
        let built = build_env(
            &cfg(&[("BIG", big.as_str()), ("OK", "small")]),
            &BTreeMap::new(),
            &allow(&["BIG", "OK"]),
        );
        assert!(built.is_err());
    }

    /// キー数の上限は config と Secret の合計。
    #[test]
    fn key_count_is_capped() {
        let entries: Vec<(String, String)> = (0..MAX_FUNCTION_ENV_KEYS + 10)
            .map(|i| (format!("K{i:03}"), "v".to_string()))
            .collect();
        let config: BTreeMap<String, String> = entries.iter().cloned().collect();
        let allowed: BTreeSet<String> = entries.iter().map(|(k, _)| k.clone()).collect();
        let built = build_env(&config, &BTreeMap::new(), &allowed);
        assert!(built.is_err());
    }

    /// 総バイト数を超えた設定は部分的に注入しない。
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
        assert!(built.is_err());
    }

    #[test]
    fn combined_vars_and_secrets_use_bytes_and_accept_the_exact_limit() {
        let vars: BTreeMap<_, _> = (0..8)
            .map(|i| (format!("A{i}"), "あ".repeat(1333)))
            .collect();
        let vars_bytes: usize = vars.iter().map(|(k, v)| k.len() + v.len()).sum();
        let remaining = MAX_FUNCTION_ENV_TOTAL_BYTES - vars_bytes - "Z_TOKEN".len();
        let allowed = vars.keys().cloned().chain(["Z_TOKEN".into()]).collect();
        let at_limit = sec(&[("Z_TOKEN", &"s".repeat(remaining))]);
        let built = build_env(&vars, &at_limit, &allowed).unwrap();
        assert_eq!(built.pairs.len(), 9);
        let oversized = sec(&[("Z_TOKEN", &"s".repeat(remaining + 1))]);
        let error = build_env(&vars, &oversized, &allowed).unwrap_err();
        assert_eq!(
            error,
            "Vars and Secrets together exceed the environment size limit"
        );
        // Unapproved oversized entries cannot deny an otherwise valid invocation.
        assert!(build_env(&vars, &oversized, &vars.keys().cloned().collect()).is_ok());
    }
}
