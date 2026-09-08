use super::limits::MAX_TABLE_ELEMENTS;
use super::*;

use wasmtime::{Memory, MemoryType, Module, Ref, RefType, Table, TableType};

fn limited_store(budget: usize) -> Store<MeteredLimits> {
    let mut store = Store::new(
        &build_engine().unwrap(),
        MeteredLimits::new(budget, Arc::new(AtomicU64::new(0))),
    );
    store.limiter(|limits| limits);
    store
}

#[test]
fn several_memories_cannot_exceed_one_execution_budget() {
    let page = 64 * 1024;
    let mut store = limited_store(3 * page);
    let first = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    let second = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    first.grow(&mut store, 1).unwrap();
    assert!(second.grow(&mut store, 1).is_err());
    assert!(Memory::new(&mut store, MemoryType::new(1, None)).is_err());
    assert_eq!(
        store.data().peak_memory_bytes.load(Ordering::Relaxed),
        (3 * page) as u64
    );
}

#[test]
fn failed_memory_growth_does_not_consume_the_remaining_budget() {
    let page = 64 * 1024;
    let mut store = limited_store(2 * page);
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(1))).unwrap();
    assert!(memory.grow(&mut store, 1).is_err());
    Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    assert!(Memory::new(&mut store, MemoryType::new(1, None)).is_err());
}

#[test]
fn several_tables_share_one_element_budget() {
    let mut store = limited_store(64 * 1024);
    let half = (MAX_TABLE_ELEMENTS / 2) as u32;
    let first = Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, half, None),
        Ref::Func(None),
    )
    .unwrap();
    Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, half, None),
        Ref::Func(None),
    )
    .unwrap();
    assert!(first.grow(&mut store, 1, Ref::Func(None)).is_err());
    assert!(Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, 1, None),
        Ref::Func(None)
    )
    .is_err());
}

#[test]
fn shared_memories_cannot_bypass_the_store_limiter() {
    let engine = build_engine().unwrap();
    assert!(Module::new(&engine, "(module (memory 1 1 shared))").is_err());
}

#[test]
fn preparing_memory_images_does_not_execute_guest_start() {
    let engine = build_engine().unwrap();
    let component = Component::new(
        &engine,
        r#"(component
            (core module $m
                (memory 1)
                (data (i32.const 0) "initial memory")
                (func $start unreachable)
                (start $start))
            (core instance (instantiate $m)))"#,
    )
    .unwrap();
    // Instantiating this component would trap. Host-only preparation must not.
    PreparedComponent::new(component).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_runtime_ticker_interrupts_a_cpu_bound_guest() {
    let engine = build_engine().unwrap();
    let runtime = Runtime::new(engine.clone(), crate::metrics::Metrics::init()).unwrap();
    let module = Module::new(&engine, "(module (func (export \"run\") (loop br 0)))").unwrap();
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX).unwrap();
    install_epoch_deadline(&mut store, Instant::now() + Duration::from_millis(20));
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
        .await
        .unwrap();
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "run")
        .unwrap();
    // A separate fail-safe prevents a broken ticker from hanging the test suite.
    let (finished, cancelled) = std::sync::mpsc::channel();
    let fallback = std::thread::spawn(move || {
        if cancelled.recv_timeout(Duration::from_secs(2)).is_err() {
            engine.increment_epoch();
        }
    });
    let started = Instant::now();
    let error = run.call_async(&mut store, ()).await.unwrap_err();
    let elapsed = started.elapsed();
    let _ = finished.send(());
    fallback.join().unwrap();
    drop(runtime);
    assert!(is_interrupt_trap(&error));
    assert!(
        elapsed < Duration::from_secs(1),
        "shared ticker failed: {elapsed:?}"
    );
}

use wasmtime::{ResourceLimiter, Store};

#[tokio::test]
async fn one_store_timeout_does_not_interrupt_another_store() {
    let engine = build_engine().unwrap();
    let module = wasmtime::Module::new(&engine, r#"(module (func (export "run")))"#).unwrap();
    let mut expired = Store::new(&engine, ());
    let mut healthy = Store::new(&engine, ());
    for store in [&mut expired, &mut healthy] {
        store.set_fuel(u64::MAX).unwrap();
    }
    install_epoch_deadline(&mut expired, Instant::now());
    install_epoch_deadline(&mut healthy, Instant::now() + Duration::from_secs(60));
    let short = wasmtime::Instance::new_async(&mut expired, &module, &[])
        .await
        .unwrap();
    let long = wasmtime::Instance::new_async(&mut healthy, &module, &[])
        .await
        .unwrap();
    engine.increment_epoch();
    let short = short.get_typed_func::<(), ()>(&mut expired, "run").unwrap();
    let long = long.get_typed_func::<(), ()>(&mut healthy, "run").unwrap();
    assert!(short.call_async(&mut expired, ()).await.is_err());
    long.call_async(&mut healthy, ()).await.unwrap();
}

/// M4b (§4.3): `ResourceLimits::max_execution_time()` は `max_execution_time_ms` を
/// `Duration` として返し、worker が `tokio::time::timeout(...)` でホスト+ゲスト総時間を
/// 覆うときの境界を決める。既定（5000ms）と任意値の両方で正しいことを担保する。
#[test]
fn resource_limits_max_execution_time_matches_ms() {
    // 既定は ResourceLimits の DEFAULT_MAX_EXECUTION_TIME_MS（5000ms）由来。
    let d = ResourceLimits::default();
    assert_eq!(d.max_execution_time(), Duration::from_millis(5000));
    // 任意値も同じ単位で。
    let custom = ResourceLimits {
        max_execution_time_ms: 250,
        ..ResourceLimits::default()
    };
    assert_eq!(custom.max_execution_time(), Duration::from_millis(250));
}

/// M4b (§4.3): tokio::time::timeout(max_execution_time) で「ホスト関数中で詰まる」
/// パスをモデル化する。WASI のブロッキングホスト関数を呼んだまま帰ってこない future を
/// `tokio::time::sleep` で代用し、worker の `run_component` が `Err(_elapsed)` 経路を
/// 辿って `ExecError::Timeout` を返すことを future の合成で再現する。
///
/// 実 wasm を要さずに timeout 分岐を行使できるのが要点（M4b の e2e は live worker +
/// 永久ブロックする component が必要なため #[ignore] で別途用意する）。
#[tokio::test(start_paused = true)]
async fn tokio_timeout_short_circuits_host_blocking_future() {
    let limits = ResourceLimits {
        max_wall_time_ms: 10,
        max_execution_time_ms: 50,
        ..ResourceLimits::default()
    };
    // host 側 blocking の代理: 100ms スリープする future（exec_timeout=50ms を超える）。
    let blocking = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok::<_, ExecError>(serde_json::Value::Null)
    };
    let timed = tokio::time::timeout(limits.max_execution_time(), blocking).await;
    // 超過は `Err(_elapsed)`。run_component はこれを `ExecError::Timeout` へ写像する。
    assert!(
        timed.is_err(),
        "tokio::time::timeout must trip on a host-blocking future longer than max_execution_time"
    );
}

/// M4b (§4.3): 実時間より短い処理は当然 timeout 内に収まる（false-positive 回帰ガード）。
#[tokio::test(start_paused = true)]
async fn tokio_timeout_passes_fast_future() {
    let limits = ResourceLimits {
        max_wall_time_ms: 10,
        max_execution_time_ms: 100,
        ..ResourceLimits::default()
    };
    let fast = async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok::<serde_json::Value, ExecError>(serde_json::json!({"ok": true}))
    };
    let timed = tokio::time::timeout(limits.max_execution_time(), fast).await;
    assert!(timed.is_ok(), "fast future must not trip the timeout");
    match timed.unwrap() {
        Ok(v) => assert_eq!(v, serde_json::json!({"ok": true})),
        Err(_) => panic!("inner future must succeed"),
    }
}

/// M4b (§4.3) trap 分類: epoch 中断は `Trap::Interrupt` で `Timeout` に倒れる。
#[test]
fn interrupt_trap_is_recognized() {
    let err: anyhow::Error = anyhow::Error::new(wasmtime::Trap::Interrupt);
    assert!(is_interrupt_trap(&err));
    assert!(!is_out_of_fuel_trap(&err));
}

/// M4b (§4.3) trap 分類: fuel 超過は `Trap::OutOfFuel` で `Failed` に倒れる
/// （timeout ではない。決定性 fuel 切れは「リソース超過」分類）。
#[test]
fn out_of_fuel_trap_is_recognized() {
    let err: anyhow::Error = anyhow::Error::new(wasmtime::Trap::OutOfFuel);
    assert!(is_out_of_fuel_trap(&err));
    assert!(!is_interrupt_trap(&err));
}

/// M4b: 関係のない trap（メモリ越境等）はどちらの分類にも該当しない。
/// `run_component` はこのケースで「elapsed >= wall なら timeout / それ以外は failed」と
/// 補助判定する（ticker race 対策）。
#[test]
fn unrelated_trap_is_not_interrupt_or_fuel() {
    let err: anyhow::Error = anyhow::Error::new(wasmtime::Trap::MemoryOutOfBounds);
    assert!(!is_interrupt_trap(&err));
    assert!(!is_out_of_fuel_trap(&err));
}

/// M5 (§15): `fuel_consumed` は fuel 有効時に `set - remaining` を返す。
/// fuel 無効化時（`max_fuel=None` → `set=u64::MAX`）は意味を持たないので 0 に倒す
/// （rollup の cpu_fuel_used が天文学的値で汚染されるのを防ぐ一次防御）。`remaining > set`
/// は理論上起きないが `saturating_sub` で 0 を返すことを担保する。
#[test]
fn fuel_consumed_disabled_enabled_and_saturating() {
    // fuel 無効化（fuel_enabled=false）: set が u64::MAX でも 0。
    assert_eq!(fuel_consumed(u64::MAX, 12_345, false), 0);
    assert_eq!(fuel_consumed(1_000_000, 0, false), 0);
    // fuel 有効: 消費 = set - remaining。
    assert_eq!(fuel_consumed(1_000_000, 250_000, true), 750_000);
    // 全消費（残 0）。
    assert_eq!(fuel_consumed(1_000_000, 0, true), 1_000_000);
    // 一切消費せず（残 == set）。
    assert_eq!(fuel_consumed(1_000_000, 1_000_000, true), 0);
    // remaining > set（防御的）: saturating_sub で 0。
    assert_eq!(fuel_consumed(100, 500, true), 0);
}

/// M5 (§15): `duration_to_millis` は `Duration` をミリ秒へ飽和変換する。
/// 非現実的に巨大な Duration（u64 ミリ秒上限超え）でも `u64::MAX` に飽和し、
/// オーバーフローや wrap を起こさない。
#[test]
fn duration_to_millis_saturates() {
    assert_eq!(duration_to_millis(Duration::from_millis(0)), 0);
    assert_eq!(duration_to_millis(Duration::from_millis(250)), 250);
    assert_eq!(duration_to_millis(Duration::from_secs(5)), 5_000);
    // u64::MAX ミリ秒を超える Duration は u64::MAX に飽和する。
    let huge = Duration::from_secs(u64::MAX);
    assert_eq!(duration_to_millis(huge), u64::MAX);
}

/// 許可された線形メモリの合計を計量し、上限超過の拒否でpeakを更新しない。
#[test]
fn metered_limits_records_peak_memory() {
    let peak = Arc::new(AtomicU64::new(0));
    let mut limits = MeteredLimits::new(1024, Arc::clone(&peak));
    // 上限内の成長: 許可され peak=512。
    assert!(limits.memory_growing(0, 512, Some(1024)).unwrap());
    assert_eq!(peak.load(Ordering::Relaxed), 512);
    // さらに大きい成長（上限ちょうど）: 許可され peak=1024 に更新。
    assert!(limits.memory_growing(512, 1024, Some(1024)).unwrap());
    assert_eq!(peak.load(Ordering::Relaxed), 1024);
    // 上限超過の成長: 拒否（false）され peak は 1024 のまま（fetch_max は呼ばれない）。
    assert!(!limits.memory_growing(1024, 2048, Some(1024)).unwrap());
    assert_eq!(peak.load(Ordering::Relaxed), 1024);
}
