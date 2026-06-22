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
}
