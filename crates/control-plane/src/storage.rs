//! Object Storage クライアント (M2: MinIO / S3 互換。§3.4)。
//!
//! wasm 本体の保存先であり、worker への配布は短命 presigned GET URL で行う。
//! MinIO は path-style addressing（`http://host/bucket/key`）を要求するため
//! `force_path_style(true)` 相当を有効にし、static credentials で接続する。
//!
//! 設計判断(承認済み): クライアントは `aws-sdk-s3`。Worker へは presigned GET URL
//! を JobMessage に同梱（短命・read-only, 既定 TTL 300秒, §3.4）。

use std::time::Duration;

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_smithy_http_client::tls::{self, rustls_provider::CryptoMode};

use faas_shared::FaasError;

/// MinIO/S3 への薄いラッパ。バケットを内包し、本体保存と presign を提供する。
#[derive(Clone)]
pub struct Storage {
    client: Client,
    bucket: String,
}

impl Storage {
    /// 設定から S3 クライアントを構築する。
    ///
    /// - `endpoint_url` で MinIO を指す
    /// - `Credentials` は static（rotate しない M2 想定）
    /// - `force_path_style(true)` で `host/bucket/key` レイアウト
    ///
    /// TODO(§8): M3 で credential rotation / IRSA 等へ移す。
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Self {
        let creds = Credentials::from_keys(access_key, secret_key, None);

        // HTTP クライアントは rustls-ring を明示注入する。
        // 既定の HTTPS クライアント (rustls-aws-lc) は aws-lc-sys 経由で C コンパイラを
        // 要求するため、当環境ではビルドできない。ring バックエンドへ切り替える。
        let http_client = aws_smithy_http_client::Builder::new()
            .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
            .build_https();

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .endpoint_url(endpoint)
            .credentials_provider(creds)
            .http_client(http_client)
            .force_path_style(true)
            .build();

        Self {
            client: Client::from_conf(config),
            bucket: bucket.to_string(),
        }
    }

    /// オブジェクトを保存する（本体アップロード, §6.2）。
    pub async fn put_object(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), FaasError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| FaasError::Internal(format!("s3 put_object failed: {e}")))?;
        Ok(())
    }

    /// 短命 presigned GET URL を生成する（worker への配布用, §3.4）。
    ///
    /// read-only・TTL 付き。worker はこの URL から本体を取得する。
    pub async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, FaasError> {
        let presign_config = PresigningConfig::expires_in(ttl)
            .map_err(|e| FaasError::Internal(format!("invalid presign ttl: {e}")))?;

        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presign_config)
            .await
            .map_err(|e| FaasError::Internal(format!("s3 presign_get failed: {e}")))?;

        Ok(req.uri().to_string())
    }

    /// 短命 presigned **PUT** URL を生成する（大入力アップロード用, §3.4 / §5.2 / §6.4）。
    ///
    /// `POST /uploads` がクライアントへ返す「単一キー限定・短 TTL」の書き込み資格。presigned URL は
    /// 署名対象が `(method=PUT, bucket, key)` に固定されるため、クライアントは **この 1 オブジェクト
    /// キーにしか PUT できない**（別キー・別テナントへは署名が一致せず拒否される）。GET（read-only,
    /// `presign_get`）と対称の write 版で、TTL は invoke の wasm presign と同条件の短命にする。
    pub async fn presign_put(&self, key: &str, ttl: Duration) -> Result<String, FaasError> {
        let presign_config = PresigningConfig::expires_in(ttl)
            .map_err(|e| FaasError::Internal(format!("invalid presign ttl: {e}")))?;

        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presign_config)
            .await
            .map_err(|e| FaasError::Internal(format!("s3 presign_put failed: {e}")))?;

        Ok(req.uri().to_string())
    }

    // --- M13: R2 バインディング（オブジェクトストレージ）本体。worker は keyless なので、
    //     R2 の実 I/O は CP が S3 クライアントで代行する（worker→CP 内部→MinIO）。
    //     キーは呼び出し側が `r2/{tenant}/{bucket}/{key}` を構築して渡す（tenant 境界）。

    /// R2 オブジェクトを保存する（content-type + カスタムメタデータつき）。
    pub async fn r2_put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        metadata: std::collections::HashMap<String, String>,
    ) -> Result<R2Meta, FaasError> {
        let etag = format!("\"{}\"", hex_sha256(&bytes));
        let size = bytes.len() as i64;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .set_metadata(Some(metadata.clone()));
        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        req.send()
            .await
            .map_err(|e| FaasError::Internal(format!("s3 r2_put failed: {e}")))?;
        Ok(R2Meta {
            size,
            etag,
            content_type: content_type.map(|s| s.to_string()),
            metadata,
        })
    }

    /// R2 オブジェクトを取得する（本体 + メタデータ）。存在しなければ `None`。
    pub async fn r2_get(&self, key: &str) -> Result<Option<R2Object>, FaasError> {
        let resp = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let se = e.into_service_error();
                if se.is_no_such_key() {
                    return Ok(None);
                }
                return Err(FaasError::Internal(format!("s3 r2_get failed: {se}")));
            }
        };
        let content_type = resp.content_type().map(|s| s.to_string());
        let etag = resp.e_tag().map(|s| s.to_string()).unwrap_or_default();
        let metadata = resp.metadata().cloned().unwrap_or_default();
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| FaasError::Internal(format!("s3 r2_get read body: {e}")))?
            .into_bytes()
            .to_vec();
        let size = bytes.len() as i64;
        Ok(Some(R2Object {
            bytes,
            meta: R2Meta {
                size,
                etag,
                content_type,
                metadata,
            },
        }))
    }

    /// R2 オブジェクトのメタデータのみ取得する（本体を読まない）。存在しなければ `None`。
    pub async fn r2_head(&self, key: &str) -> Result<Option<R2Meta>, FaasError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(r) => Ok(Some(R2Meta {
                size: r.content_length().unwrap_or(0),
                etag: r.e_tag().map(|s| s.to_string()).unwrap_or_default(),
                content_type: r.content_type().map(|s| s.to_string()),
                metadata: r.metadata().cloned().unwrap_or_default(),
            })),
            Err(e) => {
                let se = e.into_service_error();
                if se.is_not_found() {
                    Ok(None)
                } else {
                    Err(FaasError::Internal(format!("s3 r2_head failed: {se}")))
                }
            }
        }
    }

    /// R2 オブジェクトを削除する（存在しなくても成功扱い）。
    pub async fn r2_delete(&self, key: &str) -> Result<(), FaasError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| FaasError::Internal(format!("s3 r2_delete failed: {e}")))?;
        Ok(())
    }

    /// R2 の prefix 一覧（key/size/etag）。`key_prefix` は `r2/{tenant}/{bucket}/` まで。
    /// 戻り値の key は `strip` を取り除いた相対キー。
    pub async fn r2_list(
        &self,
        key_prefix: &str,
        strip: &str,
        limit: i32,
    ) -> Result<Vec<R2ListItem>, FaasError> {
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(key_prefix)
            .max_keys(limit.clamp(1, 1000))
            .send()
            .await
            .map_err(|e| FaasError::Internal(format!("s3 r2_list failed: {e}")))?;
        let mut out = Vec::new();
        for obj in resp.contents() {
            let full = obj.key().unwrap_or_default();
            let rel = full.strip_prefix(strip).unwrap_or(full).to_string();
            out.push(R2ListItem {
                key: rel,
                size: obj.size().unwrap_or(0),
                etag: obj.e_tag().map(|s| s.to_string()).unwrap_or_default(),
            });
        }
        Ok(out)
    }
}

/// R2 オブジェクトのメタデータ。
#[derive(Debug, Clone)]
pub struct R2Meta {
    pub size: i64,
    pub etag: String,
    pub content_type: Option<String>,
    pub metadata: std::collections::HashMap<String, String>,
}

/// R2 オブジェクト（本体 + メタデータ）。
pub struct R2Object {
    pub bytes: Vec<u8>,
    pub meta: R2Meta,
}

/// R2 list の 1 件。
pub struct R2ListItem {
    pub key: String,
    pub size: i64,
    pub etag: String,
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in d {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}
