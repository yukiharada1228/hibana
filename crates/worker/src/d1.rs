//! M14: D1 バインディング（Cloudflare Workers D1 互換, SQLite）の実体。
//!
//! entity = (tenant, name)。実行が最初に D1 を触った時に **Postgres の session advisory lock** を
//! 取得し（単一書き手）、DB ファイル(bytea)をロードして一時ファイルの SQLite として**実行の間だけ
//! 常駐**させる。全クエリは常駐 SQLite に対して実行し、実行終了時に save + unlock する。
//!
//! **安全性**: guest の任意 SQL は rusqlite の authorizer で `ATTACH`/`DETACH` を拒否する
//! （load_extension は既定で無効）。SQLite は Postgres とは別エンジンで、単一ファイルの外へは
//! 出られないため、guest SQL がプラットフォームのデータへ触れることはない。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::hooks::AuthAction;
use rusqlite::hooks::Authorization;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value};
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres, Row as _};

/// (tenant, name) → 安定した advisory lock キー（プロセス跨ぎで一致する必要があるため FNV-1a）。
fn lock_key(tenant: &str, name: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in tenant
        .bytes()
        .chain(b"/".iter().copied())
        .chain(name.bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h as i64
}

/// 実行中に常駐する 1 つの D1 データベースセッション。
pub struct D1Session {
    pg: PoolConnection<Postgres>,
    conn: Arc<Mutex<rusqlite::Connection>>,
    temp_path: PathBuf,
    tenant: String,
    name: String,
    key: i64,
    dirty: bool,
}

fn map_sql_err(e: rusqlite::Error) -> String {
    format!("D1 SQL error: {e}")
}

fn json_to_sqlite(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                S::Integer(i)
            } else {
                S::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => S::Text(s.clone()),
        // 配列/オブジェクトは JSON 文字列として束縛（D1 は基本 primitive）。
        other => S::Text(other.to_string()),
    }
}

fn valueref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Array(b.iter().map(|&x| Value::from(x)).collect()),
    }
}

impl D1Session {
    /// セッションを開く: 接続確保 → advisory lock → GUC → DB ロード → 一時ファイルの SQLite を開く。
    pub async fn open(pool: &PgPool, tenant: &str, name: &str) -> Result<D1Session, String> {
        let key = lock_key(tenant, name);
        let mut pg = pool
            .acquire()
            .await
            .map_err(|e| format!("d1: acquire conn: {e}"))?;
        // 単一書き手（同一 DB を触る他実行は待つ）。接続保持中は保持され、flush で解放。
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(key)
            .execute(&mut *pg)
            .await
            .map_err(|e| format!("d1: advisory_lock: {e}"))?;
        // RLS 用 GUC（load/save をこの接続で行う）。
        sqlx::query("SELECT set_config('app.tenant_id', $1, false)")
            .bind(tenant)
            .execute(&mut *pg)
            .await
            .map_err(|e| format!("d1: set guc: {e}"))?;

        let existing: Option<Vec<u8>> =
            sqlx::query("SELECT data FROM d1_databases WHERE tenant_id=$1 AND name=$2")
                .bind(tenant)
                .bind(name)
                .fetch_optional(&mut *pg)
                .await
                .map_err(|e| format!("d1: load: {e}"))?
                .map(|r| r.get::<Vec<u8>, _>("data"));

        // 一時ファイルに DB を展開して開く（空なら新規 DB）。
        let temp_path = std::env::temp_dir().join(format!(
            "hibana-d1-{}-{}.sqlite",
            std::process::id(),
            key as u64
        ));
        if let Some(bytes) = &existing {
            if !bytes.is_empty() {
                std::fs::write(&temp_path, bytes).map_err(|e| format!("d1: temp write: {e}"))?;
            } else {
                let _ = std::fs::remove_file(&temp_path);
            }
        } else {
            let _ = std::fs::remove_file(&temp_path);
        }

        let conn =
            rusqlite::Connection::open(&temp_path).map_err(|e| format!("d1: open sqlite: {e}"))?;
        // sandbox: ATTACH/DETACH を拒否（load_extension は既定で無効）。単一ファイルの外へ出さない。
        conn.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
            match ctx.action {
                AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
                _ => Authorization::Allow,
            }
        }));

        Ok(D1Session {
            pg,
            conn: Arc::new(Mutex::new(conn)),
            temp_path,
            tenant: tenant.to_string(),
            name: name.to_string(),
            key,
            dirty: false,
        })
    }

    /// 1 文を実行して D1 形式の結果 JSON を返す（`{results, success, meta}`）。
    pub async fn query(&mut self, sql: String, params: Vec<Value>) -> Result<Value, String> {
        self.dirty = true; // 書き込みかどうかを厳密判定しないので保守的に dirty（save は実行末 1 回）。
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || run_one(&conn.lock().unwrap(), &sql, &params))
            .await
            .map_err(|e| format!("d1: join: {e}"))?
    }

    /// 複数文を 1 トランザクションで実行（Workers `batch()` 相当。原子的）。
    pub async fn batch(&mut self, stmts: Vec<(String, Vec<Value>)>) -> Result<Value, String> {
        self.dirty = true;
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().unwrap();
            c.execute_batch("BEGIN").map_err(map_sql_err)?;
            let mut out = Vec::new();
            for (sql, params) in &stmts {
                match run_one(&c, sql, params) {
                    Ok(v) => out.push(v),
                    Err(e) => {
                        let _ = c.execute_batch("ROLLBACK");
                        return Err(e);
                    }
                }
            }
            c.execute_batch("COMMIT").map_err(map_sql_err)?;
            Ok(Value::Array(out))
        })
        .await
        .map_err(|e| format!("d1: join: {e}"))?
    }

    /// スクリプト（複数文・パラメータ無し）を実行（Workers `exec()` 相当）。
    pub async fn exec(&mut self, sql: String) -> Result<Value, String> {
        self.dirty = true;
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            conn.lock()
                .unwrap()
                .execute_batch(&sql)
                .map_err(map_sql_err)?;
            Ok(serde_json::json!({ "success": true }))
        })
        .await
        .map_err(|e| format!("d1: join: {e}"))?
    }

    /// 保存（dirty のときのみ）＋ advisory lock 解放。接続は pool へ戻す。
    pub async fn flush(mut self) -> Result<(), String> {
        if self.dirty {
            // 接続を閉じてファイルを確定させてから読む。
            {
                let conn = Arc::clone(&self.conn);
                // Mutex 内の Connection を drop（close）するために置き換える。
                let dummy = rusqlite::Connection::open_in_memory().map_err(|e| e.to_string())?;
                let old = std::mem::replace(&mut *conn.lock().unwrap(), dummy);
                drop(old);
            }
            let bytes = std::fs::read(&self.temp_path).unwrap_or_default();
            sqlx::query(
                "INSERT INTO d1_databases (tenant_id, name, data, updated_at) VALUES ($1,$2,$3, now()) \
                 ON CONFLICT (tenant_id, name) DO UPDATE SET data=EXCLUDED.data, updated_at=now()",
            )
            .bind(&self.tenant)
            .bind(&self.name)
            .bind(&bytes)
            .execute(&mut *self.pg)
            .await
            .map_err(|e| format!("d1: save: {e}"))?;
        }
        // 明示解放（after_release backstop もあるが正常時はここで）。
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut *self.pg)
            .await;
        let _ = std::fs::remove_file(&self.temp_path);
        Ok(())
    }
}

impl Drop for D1Session {
    fn drop(&mut self) {
        // flush されずに落ちた場合の掃除（lock は pool の after_release が解放する）。
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

/// 1 文を実行して `{results, success, meta}` を返す。SELECT でなくても query() で step する。
fn run_one(conn: &rusqlite::Connection, sql: &str, params: &[Value]) -> Result<Value, String> {
    let mut stmt = conn.prepare(sql).map_err(map_sql_err)?;
    let ncol = stmt.column_count();
    let col_names: Vec<String> = (0..ncol)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();
    let sqlite_params: Vec<rusqlite::types::Value> = params.iter().map(json_to_sqlite).collect();
    let param_refs: Vec<&dyn rusqlite::ToSql> = sqlite_params
        .iter()
        .map(|v| v as &dyn rusqlite::ToSql)
        .collect();

    let mut rows = stmt.query(param_refs.as_slice()).map_err(map_sql_err)?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().map_err(map_sql_err)? {
        let mut obj = Map::new();
        for (i, name) in col_names.iter().enumerate() {
            let vr = row.get_ref(i).map_err(map_sql_err)?;
            obj.insert(name.clone(), valueref_to_json(vr));
        }
        results.push(Value::Object(obj));
    }
    drop(rows);
    drop(stmt);

    let changes = conn.changes();
    let last_id = conn.last_insert_rowid();
    Ok(serde_json::json!({
        "results": results,
        "success": true,
        "meta": {
            "changes": changes,
            "last_row_id": last_id,
            "rows_read": results.len(),
            "rows_written": changes,
        }
    }))
}
