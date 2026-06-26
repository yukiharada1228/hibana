//! Cron 式パーサ + 次回発火時刻計算 (M6b, §15)。
//!
//! 標準的な 5 フィールド cron 式（分 時 日 月 曜）を **DB 非依存**でパースし、与えた UTC 時刻より
//! 後の最初の発火時刻を計算する。外部クレートを足さず（cc/aws-lc 回避方針と同様にビルド面を増やさ
//! ない）、chrono の `DateTime<Utc>` 上で 1 分刻みに前進させる単純で検証しやすい実装にする。
//!
//! 対応構文（各フィールド共通）:
//! - `*`            … 取りうる全値
//! - `a`            … 単一値
//! - `a-b`          … 範囲（両端含む）
//! - `a,b,c`        … リスト（各要素はさらに範囲/ステップを取れる）
//! - `*/n` / `a-b/n`… ステップ（基底集合を n 間隔で間引く）
//!
//! フィールドの取りうる範囲: 分 0-59 / 時 0-23 / 日 1-31 / 月 1-12 / 曜 0-6（0=日曜）。
//! 日(DOM)と曜(DOW)はどちらも `*` でなければ **OR**（標準 cron の慣習）で一致を取る。
//! `next_fire_at` は「与えた時刻ちょうど」は含めず、その**次**の一致時刻を返す（前進保証）。
//!
//! Cron 登録時（`POST /cron-jobs`）に `CronSchedule::parse` で構文検証し（不正は 422）、
//! スケジューラ/登録時の初回 `next_fire_at` 計算に `next_after` を使う。

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};

/// パース済み cron 式（5 フィールド）。各フィールドは許可値の集合をビット的に持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: Vec<u32>,       // 0-59
    hours: Vec<u32>,         // 0-23
    days_of_month: Vec<u32>, // 1-31
    months: Vec<u32>,        // 1-12
    days_of_week: Vec<u32>,  // 0-6 (0=Sun)
    /// 日(DOM)が `*` か（DOM/DOW の OR 合成判定に使う）。
    dom_wild: bool,
    /// 曜(DOW)が `*` か（同上）。
    dow_wild: bool,
}

impl CronSchedule {
    /// cron 式をパースする。フィールド数不一致・範囲外・構文不正は `Err(理由)`。
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron expression must have exactly 5 fields (min hour dom month dow), got {}",
                fields.len()
            ));
        }
        let minutes = parse_field(fields[0], 0, 59, "minute")?;
        let hours = parse_field(fields[1], 0, 23, "hour")?;
        let days_of_month = parse_field(fields[2], 1, 31, "day-of-month")?;
        let months = parse_field(fields[3], 1, 12, "month")?;
        let days_of_week = parse_field(fields[4], 0, 6, "day-of-week")?;
        Ok(Self {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            dom_wild: fields[2].trim() == "*",
            dow_wild: fields[4].trim() == "*",
        })
    }

    /// 与えた UTC 時刻 `after` より **後** の最初の発火時刻を返す。
    ///
    /// 秒は無視し（cron は分粒度）、`after` の次の分境界から 1 分刻みに最大 ~4 年走査して一致を探す。
    /// 一致が見つからなければ `None`（理論上「2/30 のような決して来ない日付」だけ。実運用では None に
    /// ならない＝呼び出し側はそれを「無効スケジュール」とみなして良い）。
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // 秒/ナノ秒を落として「次の分」から走査する（after ちょうどは含めない）。
        let start = after
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(after)
            + Duration::minutes(1);

        // 4 年（うるう年を跨いでも DOM/DOW の全組合せを必ず含む）分の分数を上限に走査する。
        const MAX_MINUTES: i64 = 366 * 4 * 24 * 60;
        let mut candidate = start;
        for _ in 0..MAX_MINUTES {
            if self.matches(candidate) {
                return Some(candidate);
            }
            candidate += Duration::minutes(1);
        }
        None
    }

    /// 与えた時刻（分粒度）が一致するか。
    fn matches(&self, t: DateTime<Utc>) -> bool {
        if !self.minutes.contains(&t.minute()) {
            return false;
        }
        if !self.hours.contains(&t.hour()) {
            return false;
        }
        if !self.months.contains(&t.month()) {
            return false;
        }
        // 標準 cron: DOM/DOW がともに非 `*` のときは **OR**（どちらか一致で発火）、
        // 片方が `*` のときは他方のみで判定、両方 `*` なら常に通す。
        let dom_match = self.days_of_month.contains(&t.day());
        // chrono の weekday は Mon=0..Sun=6。cron は Sun=0..Sat=6 なので変換する。
        let cron_dow = t.weekday().num_days_from_sunday();
        let dow_match = self.days_of_week.contains(&cron_dow);
        match (self.dom_wild, self.dow_wild) {
            (true, true) => true,
            (false, true) => dom_match,
            (true, false) => dow_match,
            (false, false) => dom_match || dow_match,
        }
    }
}

/// 1 フィールドを `[min, max]` 範囲の許可値ベクタへパースする。
///
/// `*` / 単一値 / 範囲 `a-b` / リスト `a,b` / ステップ `*/n`・`a-b/n` を受け、範囲外・構文不正は
/// `Err`。返値は昇順・重複除去済み（`contains` で参照するため順序自体は意味を持たないが安定化する）。
fn parse_field(field: &str, min: u32, max: u32, name: &str) -> Result<Vec<u32>, String> {
    let field = field.trim();
    if field.is_empty() {
        return Err(format!("{name} field is empty"));
    }
    let mut set: Vec<u32> = Vec::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("{name} field has an empty list element"));
        }
        // ステップ `base/step` を分離する。
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("{name}: invalid step '{s}'"))?;
                if step == 0 {
                    return Err(format!("{name}: step must be >= 1"));
                }
                (b, step)
            }
            None => (part, 1),
        };
        // base が `*` か `a` か `a-b` かを判定し、[lo, hi] を確定する。
        let (lo, hi) = if base == "*" {
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            let lo: u32 = a
                .parse()
                .map_err(|_| format!("{name}: invalid range start '{a}'"))?;
            let hi: u32 = b
                .parse()
                .map_err(|_| format!("{name}: invalid range end '{b}'"))?;
            if lo > hi {
                return Err(format!("{name}: range start {lo} > end {hi}"));
            }
            (lo, hi)
        } else {
            let v: u32 = base
                .parse()
                .map_err(|_| format!("{name}: invalid value '{base}'"))?;
            (v, v)
        };
        if lo < min || hi > max {
            return Err(format!(
                "{name}: value out of range [{min},{max}] (got [{lo},{hi}])"
            ));
        }
        let mut v = lo;
        while v <= hi {
            if !set.contains(&v) {
                set.push(v);
            }
            v += step;
        }
    }
    set.sort_unstable();
    Ok(set)
}

/// 与えた cron 式と「いま」から初回 `next_fire_at` を計算する薄いヘルパ（登録時に使う）。
///
/// `now` を引数で受け取り時計依存を排除してテスト可能にする。パース失敗は `Err`（422 へ）。
pub fn first_fire_after(expr: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let sched = CronSchedule::parse(expr)?;
    sched
        .next_after(now)
        .ok_or_else(|| "cron expression never fires (unsatisfiable date)".to_string())
}

/// `next_fire_at`（UTC）から scheduled_slot（unix 秒、分境界）を導出する (M6b)。
///
/// 同一スロット→同一 `cron_idempotency_key` の安定性のため、秒以下を 0 に正規化した分境界の
/// unix 秒を使う（`next_after` は既に分粒度を返すが、防御的に床関数を通す）。
pub fn scheduled_slot_unix(next_fire_at: DateTime<Utc>) -> i64 {
    let floored = next_fire_at
        .with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(next_fire_at);
    floored.timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap()
    }

    #[test]
    fn parse_rejects_wrong_field_count() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("* * * * * *").is_err());
        assert!(CronSchedule::parse("").is_err());
    }

    #[test]
    fn parse_rejects_out_of_range() {
        assert!(CronSchedule::parse("60 * * * *").is_err()); // minute 60
        assert!(CronSchedule::parse("* 24 * * *").is_err()); // hour 24
        assert!(CronSchedule::parse("* * 0 * *").is_err()); // dom 0
        assert!(CronSchedule::parse("* * * 13 *").is_err()); // month 13
        assert!(CronSchedule::parse("* * * * 7").is_err()); // dow 7
    }

    #[test]
    fn parse_rejects_bad_step_and_range() {
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
        assert!(CronSchedule::parse("5-1 * * * *").is_err());
        assert!(CronSchedule::parse("a * * * *").is_err());
    }

    #[test]
    fn every_minute_advances_by_one_minute() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        let now = at(2026, 6, 25, 12, 30);
        assert_eq!(s.next_after(now), Some(at(2026, 6, 25, 12, 31)));
    }

    #[test]
    fn step_minutes_every_five() {
        let s = CronSchedule::parse("*/5 * * * *").unwrap();
        // 12:31 の次の */5 一致は 12:35。
        assert_eq!(
            s.next_after(at(2026, 6, 25, 12, 31)),
            Some(at(2026, 6, 25, 12, 35))
        );
        // 12:35 ちょうどは含めず次の 12:40。
        assert_eq!(
            s.next_after(at(2026, 6, 25, 12, 35)),
            Some(at(2026, 6, 25, 12, 40))
        );
    }

    #[test]
    fn daily_at_fixed_time_rolls_to_next_day() {
        let s = CronSchedule::parse("0 0 * * *").unwrap();
        // 00:00 ちょうどは含めず翌日 00:00。
        assert_eq!(
            s.next_after(at(2026, 6, 25, 0, 0)),
            Some(at(2026, 6, 26, 0, 0))
        );
        // 直前なら同日 00:00 ではなく…23:59 の次は翌 00:00。
        assert_eq!(
            s.next_after(at(2026, 6, 25, 23, 59)),
            Some(at(2026, 6, 26, 0, 0))
        );
    }

    #[test]
    fn list_and_range_minutes() {
        let s = CronSchedule::parse("0,15,30,45 * * * *").unwrap();
        assert_eq!(
            s.next_after(at(2026, 6, 25, 10, 1)),
            Some(at(2026, 6, 25, 10, 15))
        );
        let r = CronSchedule::parse("10-12 * * * *").unwrap();
        assert_eq!(
            r.next_after(at(2026, 6, 25, 10, 10)),
            Some(at(2026, 6, 25, 10, 11))
        );
    }

    #[test]
    fn dom_dow_or_semantics() {
        // 「毎月 1 日 OR 毎週月曜の 00:00」。2026-06-01 は月曜なので両方一致。
        let s = CronSchedule::parse("0 0 1 * 1").unwrap();
        // 5/31(日) の次は 6/1(月, dom=1 かつ dow=Mon)。
        assert_eq!(
            s.next_after(at(2026, 5, 31, 12, 0)),
            Some(at(2026, 6, 1, 0, 0))
        );
        // 6/1 の次は 6/8(月曜, dom!=1 だが dow 一致)。
        assert_eq!(
            s.next_after(at(2026, 6, 1, 0, 0)),
            Some(at(2026, 6, 8, 0, 0))
        );
    }

    #[test]
    fn first_fire_after_parses_and_advances() {
        let next = first_fire_after("*/5 * * * *", at(2026, 6, 25, 9, 2)).unwrap();
        assert_eq!(next, at(2026, 6, 25, 9, 5));
        assert!(first_fire_after("bad expr", at(2026, 6, 25, 9, 2)).is_err());
    }

    #[test]
    fn scheduled_slot_is_minute_floored_unix() {
        let t = at(2026, 6, 25, 9, 5);
        assert_eq!(scheduled_slot_unix(t), t.timestamp());
        // 秒/ナノ秒が乗っていても分境界へ床関数される。
        let t2 = t + Duration::seconds(37);
        assert_eq!(scheduled_slot_unix(t2), t.timestamp());
    }

    #[test]
    fn never_fires_returns_none() {
        // 2 月 30 日は存在しない → None。
        let s = CronSchedule::parse("0 0 30 2 *").unwrap();
        assert_eq!(s.next_after(at(2026, 1, 1, 0, 0)), None);
    }
}
