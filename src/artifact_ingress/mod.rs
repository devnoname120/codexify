mod error;
mod openai_file;
mod workspace_publish;

use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

pub use error::{ArtifactIngressError, ArtifactIngressResult};
pub use openai_file::OpenAiFileParam;

use openai_file::{FileHttpClient, ReqwestFileClient, open_openai_file};
use workspace_publish::{ArtifactDestination, PendingWorkspaceFile};

use crate::types::{AppConfig, ArtifactIngressConfig};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReceipt {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

pub async fn import_openai_file_before(
    config: &AppConfig,
    file: &OpenAiFileParam,
    destination: &str,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> ArtifactIngressResult<ImportReceipt> {
    let client = ReqwestFileClient::new(&config.artifact_ingress)?;
    import_openai_file_with_client_before(
        &client,
        &config.artifact_ingress,
        &config.work_dir,
        file,
        destination,
        deadline,
        cancellation,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn import_openai_file_with_client(
    client: &dyn FileHttpClient,
    ingress: &ArtifactIngressConfig,
    work_dir: &std::path::Path,
    file: &OpenAiFileParam,
    destination: &str,
) -> ArtifactIngressResult<ImportReceipt> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(ingress.request_timeout_ms))
        .ok_or_else(|| {
            ArtifactIngressError::new(
                "artifact_ingress_invalid",
                "The configured request timeout is too large for this platform.",
            )
        })?;
    import_openai_file_with_client_before(
        client,
        ingress,
        work_dir,
        file,
        destination,
        deadline,
        &CancellationToken::new(),
    )
    .await
}

async fn import_openai_file_with_client_before(
    client: &dyn FileHttpClient,
    ingress: &ArtifactIngressConfig,
    work_dir: &std::path::Path,
    file: &OpenAiFileParam,
    destination: &str,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> ArtifactIngressResult<ImportReceipt> {
    let destination = ArtifactDestination::parse(destination)?;
    let mut opened = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(cancellation_error()),
        result = timeout_at(deadline, open_openai_file(client, file, ingress)) => {
            result.map_err(|_| deadline_error())??
        }
    };
    tracing::debug!(
        source_host = %opened.source_host,
        "validated native-file source"
    );
    let mut pending = PendingWorkspaceFile::create_before(
        work_dir.to_path_buf(),
        destination,
        deadline,
        cancellation,
    )
    .await?;
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    let idle_timeout = Duration::from_millis(ingress.idle_timeout_ms);

    loop {
        let now = Instant::now();
        let idle_deadline = now
            .checked_add(idle_timeout)
            .unwrap_or(deadline)
            .min(deadline);
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancellation_error()),
            result = timeout_at(idle_deadline, opened.body.next_chunk()) => {
                result.map_err(|_| {
                    if idle_deadline == deadline {
                        deadline_error()
                    } else {
                        ArtifactIngressError::new(
                            "file_stream_timed_out",
                            "The native file stream stopped producing data before completion.",
                        )
                    }
                })??
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk_len = u64::try_from(chunk.len()).map_err(|_| {
            ArtifactIngressError::new(
                "file_too_large",
                "The native file chunk could not be represented safely.",
            )
        })?;
        let next_size = written.checked_add(chunk_len).ok_or_else(|| {
            ArtifactIngressError::new(
                "file_too_large",
                "The native file exceeds the supported size range.",
            )
        })?;
        if next_size > ingress.max_file_bytes {
            return Err(ArtifactIngressError::new(
                "file_too_large",
                format!(
                    "The native file exceeds the configured {} byte limit.",
                    ingress.max_file_bytes
                ),
            ));
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancellation_error()),
            result = timeout_at(deadline, pending.write_chunk(&chunk)) => {
                result.map_err(|_| deadline_error())??;
            }
        }
        hasher.update(&chunk);
        written = next_size;
    }

    if opened
        .content_length
        .is_some_and(|expected| expected != written)
    {
        return Err(ArtifactIngressError::new(
            "file_size_mismatch",
            "The downloaded byte count did not match the response content length.",
        ));
    }

    let path = pending
        .publish_before(written, deadline, cancellation)
        .await?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(ImportReceipt {
        path,
        bytes: written,
        sha256: format!("sha256:{hex}"),
        source: "openai_file",
        mime_type: opened.mime_type,
    })
}

fn deadline_error() -> ArtifactIngressError {
    ArtifactIngressError::new(
        "file_import_timed_out",
        "The native-file import exceeded the configured request timeout.",
    )
}

fn cancellation_error() -> ArtifactIngressError {
    ArtifactIngressError::new(
        "file_import_cancelled",
        "Native file ingress was cancelled by the MCP client.",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use bytes::Bytes;
    use http::header::{CONTENT_LENGTH, CONTENT_TYPE};
    use http::{HeaderMap, StatusCode};
    use reqwest::Url;

    use super::*;
    use crate::artifact_ingress::openai_file::{FileBody, FileHttpResponse};

    enum BodyStep {
        Chunk(&'static [u8]),
        Error,
        Stall,
        End,
    }

    struct ScriptedBody {
        steps: VecDeque<BodyStep>,
    }

    #[async_trait]
    impl FileBody for ScriptedBody {
        async fn next_chunk(&mut self) -> ArtifactIngressResult<Option<Bytes>> {
            match self.steps.pop_front().unwrap_or(BodyStep::End) {
                BodyStep::Chunk(bytes) => Ok(Some(Bytes::from_static(bytes))),
                BodyStep::Error => Err(ArtifactIngressError::new(
                    "file_download_failed",
                    "Synthetic stream failure.",
                )),
                BodyStep::Stall => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(None)
                }
                BodyStep::End => Ok(None),
            }
        }
    }

    struct SingleResponseClient {
        response: Mutex<Option<FileHttpResponse>>,
    }

    #[async_trait]
    impl FileHttpClient for SingleResponseClient {
        async fn get(&self, _url: &Url) -> ArtifactIngressResult<FileHttpResponse> {
            self.response.lock().unwrap().take().ok_or_else(|| {
                ArtifactIngressError::new("test_error", "Synthetic response already consumed.")
            })
        }
    }

    fn client(steps: Vec<BodyStep>, content_length: Option<u64>) -> SingleResponseClient {
        let mut headers = HeaderMap::new();
        if let Some(content_length) = content_length {
            headers.insert(CONTENT_LENGTH, content_length.to_string().parse().unwrap());
        }
        headers.insert(CONTENT_TYPE, "application/octet-stream".parse().unwrap());
        SingleResponseClient {
            response: Mutex::new(Some(FileHttpResponse {
                status: StatusCode::OK,
                headers,
                body: Box::new(ScriptedBody {
                    steps: steps.into(),
                }),
            })),
        }
    }

    fn reference() -> OpenAiFileParam {
        OpenAiFileParam {
            download_url: "https://files.oaiusercontent.com/object?signature=secret".to_string(),
            file_id: "file_secret".to_string(),
            mime_type: None,
            file_name: Some("input.bin".to_string()),
        }
    }

    fn partial_count(root: &std::path::Path) -> usize {
        std::fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".codexify-import-"))
            })
            .count()
    }

    async fn wait_for_partial_cleanup(root: &std::path::Path) {
        for _ in 0..100 {
            if partial_count(root) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("native-file partial remained in {}", root.display());
    }

    #[tokio::test]
    async fn streams_exact_bytes_and_returns_an_integrity_receipt() {
        let root = tempfile::tempdir().unwrap();
        let client = client(
            vec![BodyStep::Chunk(b"hello "), BodyStep::Chunk(b"world")],
            Some(11),
        );
        let receipt = import_openai_file_with_client(
            &client,
            &ArtifactIngressConfig::default(),
            root.path(),
            &reference(),
            "fixture.bin",
        )
        .await
        .unwrap();

        assert_eq!(receipt.path, "fixture.bin");
        assert_eq!(receipt.bytes, 11);
        assert_eq!(
            receipt.sha256,
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(
            receipt.mime_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(
            std::fs::read(root.path().join("fixture.bin")).unwrap(),
            b"hello world"
        );
    }

    #[tokio::test]
    async fn streamed_oversize_removes_the_partial_and_final() {
        let root = tempfile::tempdir().unwrap();
        let client = client(vec![BodyStep::Chunk(b"1234"), BodyStep::Chunk(b"56")], None);
        let config = ArtifactIngressConfig {
            max_file_bytes: 5,
            ..Default::default()
        };

        let error = import_openai_file_with_client(
            &client,
            &config,
            root.path(),
            &reference(),
            "too-large.bin",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "file_too_large");
        wait_for_partial_cleanup(root.path()).await;
        assert!(!root.path().join("too-large.bin").exists());
    }

    #[tokio::test]
    async fn response_length_mismatch_removes_the_partial() {
        let root = tempfile::tempdir().unwrap();
        let client = client(vec![BodyStep::Chunk(b"short")], Some(10));

        let error = import_openai_file_with_client(
            &client,
            &ArtifactIngressConfig::default(),
            root.path(),
            &reference(),
            "mismatch.bin",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "file_size_mismatch");
        wait_for_partial_cleanup(root.path()).await;
    }

    #[tokio::test]
    async fn stream_error_removes_the_partial() {
        let root = tempfile::tempdir().unwrap();
        let client = client(vec![BodyStep::Chunk(b"partial"), BodyStep::Error], None);

        let error = import_openai_file_with_client(
            &client,
            &ArtifactIngressConfig::default(),
            root.path(),
            &reference(),
            "failed.bin",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "file_download_failed");
        wait_for_partial_cleanup(root.path()).await;
        assert!(!root.path().join("failed.bin").exists());
    }

    #[tokio::test]
    async fn idle_timeout_cancels_the_stream_and_removes_the_partial() {
        let root = tempfile::tempdir().unwrap();
        let client = client(vec![BodyStep::Chunk(b"partial"), BodyStep::Stall], None);
        let config = ArtifactIngressConfig {
            request_timeout_ms: 5_000,
            idle_timeout_ms: 10,
            ..Default::default()
        };

        let error = import_openai_file_with_client(
            &client,
            &config,
            root.path(),
            &reference(),
            "stalled.bin",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "file_stream_timed_out");
        wait_for_partial_cleanup(root.path()).await;
        assert!(!root.path().join("stalled.bin").exists());
    }

    #[tokio::test]
    async fn mcp_cancellation_stops_the_stream_and_removes_the_partial() {
        let root = tempfile::tempdir().unwrap();
        let client = client(vec![BodyStep::Chunk(b"partial"), BodyStep::Stall], None);
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        let config = ArtifactIngressConfig::default();
        let reference = reference();
        let cancel_task = async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            canceller.cancel();
        };
        let (result, ()) = tokio::join!(
            import_openai_file_with_client_before(
                &client,
                &config,
                root.path(),
                &reference,
                "cancelled.bin",
                deadline,
                &cancellation,
            ),
            cancel_task,
        );

        assert_eq!(result.unwrap_err().code(), "file_import_cancelled");
        wait_for_partial_cleanup(root.path()).await;
        assert!(!root.path().join("cancelled.bin").exists());
    }

    #[tokio::test]
    async fn invalid_destination_is_rejected_before_network_access() {
        struct PanickingClient;

        #[async_trait]
        impl FileHttpClient for PanickingClient {
            async fn get(&self, _url: &Url) -> ArtifactIngressResult<FileHttpResponse> {
                panic!("network should not be reached for an invalid destination")
            }
        }

        let root = tempfile::tempdir().unwrap();
        let error = import_openai_file_with_client(
            &PanickingClient,
            &ArtifactIngressConfig::default(),
            root.path(),
            &reference(),
            "../outside.bin",
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "destination_invalid");
    }
}
