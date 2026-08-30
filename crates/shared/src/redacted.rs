//! 露出ガード: 秘密値ラッパ `Redacted<T>` (M7-0, §7 / §10)。
//!
//! **このファイルは `scripts/rls-lint.sh` の検査 (4) の allowlist に入る**（`expose()` の
//! 定義そのものがここに在るため）。秘密を実際に取り出す箇所を狭く保つため、型定義だけを
//! ここに置き、他の共有契約（JobMessage 等）とは同居させない。

use serde::Serialize;

/// 秘密値のラッパ。`Debug` / `Display` は常に `"<redacted>"` を出す。
///
/// **`Serialize` を実装しない**のが設計の中心である。うっかり API 応答型のフィールドに入れると
/// **コンパイルエラーになる**（実行時に漏れてから気付くのではなく、型で塞ぐ）。一度だけ返す
/// 正当なケース（login / token 発行）は `#[serde(serialize_with = "faas_shared::expose_once")]` を
/// フィールドに明示する —— この属性は grep 可能であり、「意図的に平文を返している箇所」の
/// 全一覧がレビューできる。
///
/// 平文の取り出しは [`Redacted::expose`] の 1 経路だけ（`scripts/rls-lint.sh` の検査 (4) が
/// 呼び出しファイルを allowlist に限定する）。
///
/// 境界を**構造体宣言側**に書いているのは Rust の制約による: `Drop` の実装には構造体宣言と
/// 同一の境界が要求されるため、`pub struct Redacted<T>(T);` + `impl<T: Zeroize> Drop` は
/// E0367 でコンパイルできない。`String` / `Vec<u8>` / `[u8; 32]` は `Zeroize` 実装済み。
///
/// `Drop` を実装すると値のムーブアウトができなくなるため `into_inner()` は提供しない
/// （`expose(&self) -> &T` のみ）。これは意図した制約であり、平文の取り出し口を 1 本に保つ。
#[derive(Clone)]
pub struct Redacted<T: zeroize::Zeroize>(T);

impl<T: zeroize::Zeroize> Redacted<T> {
    pub fn new(v: T) -> Self {
        Self(v)
    }

    /// 平文を取り出す。**呼び出しは allowlist ファイルのみ**（rls-lint の検査 (4)）。
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: zeroize::Zeroize> std::fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T: zeroize::Zeroize> std::fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T: zeroize::Zeroize> Drop for Redacted<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// 「一度だけ平文で返す」フィールド用の serializer（`#[serde(serialize_with = "...")]`）。
///
/// `Redacted<T>` にうっかり `Serialize` を実装する代わりに、**明示的にこの属性を書いた
/// フィールドだけ**が平文で出る。属性名で grep すれば「意図的に秘密を返す API」の全一覧になる。
/// 対象は発行直後に一度だけ返すもの（ログイン token / API トークン secret）に限ること。
pub fn expose_once<S, T>(v: &Redacted<T>, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: zeroize::Zeroize + Serialize,
{
    v.expose().serialize(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroize;

    /// Debug / Display のどちらにも平文が出ないこと（ログ経路の一次防御）。
    #[test]
    fn debug_and_display_never_reveal_the_secret() {
        let r = Redacted::new("hunter2".to_string());
        let dbg = format!("{r:?}");
        let disp = format!("{r}");
        assert!(!dbg.contains("hunter2"), "Debug leaked the secret: {dbg}");
        assert!(
            !disp.contains("hunter2"),
            "Display leaked the secret: {disp}"
        );
        assert_eq!(dbg, "<redacted>");
        assert_eq!(disp, "<redacted>");
    }

    /// 構造体の中に入れても、その構造体の derive(Debug) 経由で漏れないこと。
    #[test]
    fn nested_in_a_struct_debug_stays_redacted() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            name: &'static str,
            token: Redacted<String>,
        }
        let h = Holder {
            name: "svc",
            token: Redacted::new("s3cr3t".to_string()),
        };
        let out = format!("{h:?}");
        assert!(out.contains("svc"), "non-secret fields must stay visible");
        assert!(!out.contains("s3cr3t"), "nested secret leaked: {out}");
    }

    /// `expose()` は平文をそのまま返す（唯一の取り出し口）。
    #[test]
    fn expose_returns_the_plaintext() {
        let r = Redacted::new(vec![1u8, 2, 3]);
        assert_eq!(r.expose(), &vec![1u8, 2, 3]);
    }

    /// Clone は秘密を複製する（doc の警告どおり）。呼び出し箇所を最小に保つこと。
    #[test]
    fn clone_duplicates_the_value() {
        let r = Redacted::new("dup".to_string());
        let c = r.clone();
        assert_eq!(c.expose(), r.expose());
    }

    /// Drop で zeroize が呼ばれること。`Zeroize` を実装したテスト用型で観測する
    /// （`Redacted` の中身は drop 時に触れないので、外部フラグで確認する）。
    #[test]
    fn drop_zeroizes_the_inner_value() {
        use std::cell::RefCell;

        thread_local! {
            static ZEROIZED: RefCell<bool> = const { RefCell::new(false) };
        }

        struct Spy;
        impl Zeroize for Spy {
            fn zeroize(&mut self) {
                ZEROIZED.with(|z| *z.borrow_mut() = true);
            }
        }

        ZEROIZED.with(|z| *z.borrow_mut() = false);
        {
            let _r = Redacted::new(Spy);
        }
        assert!(
            ZEROIZED.with(|z| *z.borrow()),
            "Drop must zeroize the wrapped secret"
        );
    }

    /// `expose_once` を付けたフィールドだけが平文で serialize される。
    #[test]
    fn expose_once_serializes_the_plaintext_only_where_declared() {
        #[derive(Serialize)]
        struct Resp {
            #[serde(serialize_with = "crate::expose_once")]
            token: Redacted<String>,
            token_id: &'static str,
        }
        let json = serde_json::to_string(&Resp {
            token: Redacted::new("one-time".to_string()),
            token_id: "tok_1",
        })
        .expect("serialize");
        assert!(json.contains("\"token\":\"one-time\""), "got {json}");
        assert!(json.contains("tok_1"));
    }
}
