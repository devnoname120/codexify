use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use http::{HeaderMap, StatusCode};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;

use super::error::{ArtifactIngressError, ArtifactIngressResult};
use crate::types::ArtifactIngressConfig;

const MAX_FILE_ID_BYTES: usize = 512;
const MAX_FILE_NAME_BYTES: usize = 1024;
const MAX_MIME_TYPE_BYTES: usize = 255;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiFileParam {
    pub download_url: String,
    pub file_id: String,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
}

impl OpenAiFileParam {
    pub fn parse(value: &Value) -> ArtifactIngressResult<Self> {
        let parsed: Self = serde_json::from_value(value.clone()).map_err(|_| {
            ArtifactIngressError::new(
                "invalid_file_reference",
                "The host-provided file reference does not match the OpenAI native-file schema.",
            )
        })?;
        parsed.validate()?;
        Ok(parsed)
    }

    fn validate(&self) -> ArtifactIngressResult<()> {
        validate_metadata_string(
            &self.file_id,
            MAX_FILE_ID_BYTES,
            "invalid_file_reference",
            "The host-provided file ID is invalid.",
        )?;
        if let Some(value) = &self.mime_type {
            validate_metadata_string(
                value,
                MAX_MIME_TYPE_BYTES,
                "invalid_file_reference",
                "The host-provided MIME type is invalid.",
            )?;
        }
        if let Some(value) = &self.file_name {
            validate_metadata_string(
                value,
                MAX_FILE_NAME_BYTES,
                "invalid_file_reference",
                "The host-provided filename is invalid.",
            )?;
        }
        self.validated_url()?;
        Ok(())
    }

    pub fn validated_url(&self) -> ArtifactIngressResult<Url> {
        let url = Url::parse(&self.download_url).map_err(|_| {
            ArtifactIngressError::new(
                "untrusted_file_url",
                "The host-provided file URL is invalid.",
            )
        })?;
        validate_url_structure(&url)?;
        Ok(url)
    }
}

fn validate_metadata_string(
    value: &str,
    max_bytes: usize,
    code: &'static str,
    message: &'static str,
) -> ArtifactIngressResult<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(|character| character.is_control())
    {
        return Err(ArtifactIngressError::new(code, message));
    }
    Ok(())
}

fn untrusted_file_url() -> ArtifactIngressError {
    ArtifactIngressError::new(
        "untrusted_file_url",
        "The host-provided file URL is outside the configured native-file allowlist.",
    )
}

/// Transport-level checks that hold for every download URL regardless of the
/// configured host allowlist: HTTPS only, no embedded credentials, no fragment.
fn validate_url_structure(url: &Url) -> ArtifactIngressResult<()> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(untrusted_file_url());
    }
    Ok(())
}

/// Full policy check applied to the initial URL and re-applied to every redirect
/// hop. Combines the transport checks with the configured host allowlist.
pub fn validate_download_url(url: &Url, allowed_hosts: &[String]) -> ArtifactIngressResult<()> {
    validate_url_structure(url)?;
    let host = url.host_str().ok_or_else(untrusted_file_url)?;
    match classify_host(host, allowed_hosts) {
        HostMatch::None => Err(untrusted_file_url()),
        // A host named explicitly in the allowlist is trusted as given, including
        // any port and even an internal address the operator deliberately listed.
        HostMatch::Explicit => Ok(()),
        // The wildcard accepts arbitrary public hosts but never internal targets,
        // and only over the standard HTTPS port, so a compromised or injected URL
        // cannot reach cloud metadata, loopback, or private-network services.
        HostMatch::Wildcard => {
            if host_is_internal(host) || url.port().is_some_and(|port| port != 443) {
                return Err(untrusted_file_url());
            }
            Ok(())
        }
    }
}

enum HostMatch {
    Explicit,
    Wildcard,
    None,
}

fn classify_host(host: &str, allowed_hosts: &[String]) -> HostMatch {
    let host = host.to_ascii_lowercase();
    let mut wildcard = false;
    for pattern in allowed_hosts {
        let pattern = pattern.to_ascii_lowercase();
        if pattern == "*" {
            wildcard = true;
        } else if let Some(bare) = pattern.strip_prefix('.') {
            if host == bare || host.ends_with(pattern.as_str()) {
                return HostMatch::Explicit;
            }
        } else if host == pattern {
            return HostMatch::Explicit;
        }
    }
    if wildcard {
        HostMatch::Wildcard
    } else {
        HostMatch::None
    }
}

/// Rejects hosts that address the local machine or a private/reserved network,
/// the SSRF floor enforced whenever a URL is admitted only by the `"*"` wildcard.
/// IP-literal hosts are classified directly; the loopback name families are
/// blocked by name because they resolve to the local host by convention.
fn host_is_internal(host: &str) -> bool {
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<std::net::IpAddr>() {
        return ip_is_internal(ip);
    }
    let lower = host.to_ascii_lowercase();
    lower == "localhost" || lower.ends_with(".localhost")
}

fn ip_is_internal(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || {
                    // Carrier-grade NAT / shared address space, 100.64.0.0/10.
                    let octets = v4.octets();
                    octets[0] == 100 && (64..=127).contains(&octets[1])
                }
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ip_is_internal(std::net::IpAddr::V4(mapped));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique local addresses, fc00::/7.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local unicast, fe80::/10.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[async_trait]
pub(crate) trait FileBody: Send {
    async fn next_chunk(&mut self) -> ArtifactIngressResult<Option<Bytes>>;
}

pub(crate) struct FileHttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Box<dyn FileBody>,
}

#[async_trait]
pub(crate) trait FileHttpClient: Send + Sync {
    async fn get(&self, url: &Url) -> ArtifactIngressResult<FileHttpResponse>;
}

pub(crate) struct ReqwestFileClient {
    client: Client,
}

impl ReqwestFileClient {
    pub fn new(config: &ArtifactIngressConfig) -> ArtifactIngressResult<Self> {
        let request_timeout = Duration::from_millis(config.request_timeout_ms);
        let connect_timeout = request_timeout.min(Duration::from_secs(30));
        let client = crate::tls::client_builder()
            .redirect(Policy::none())
            .no_proxy()
            .https_only(true)
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .user_agent("codexify/native-file-ingress")
            .build()
            .map_err(|_| {
                ArtifactIngressError::new(
                    "file_download_failed",
                    "The native-file download client could not be initialized.",
                )
            })?;
        Ok(Self { client })
    }
}

struct ReqwestFileBody {
    response: reqwest::Response,
}

#[async_trait]
impl FileBody for ReqwestFileBody {
    async fn next_chunk(&mut self) -> ArtifactIngressResult<Option<Bytes>> {
        self.response.chunk().await.map_err(|_| {
            ArtifactIngressError::new(
                "file_download_failed",
                "The native file stream ended with a download error.",
            )
        })
    }
}

#[async_trait]
impl FileHttpClient for ReqwestFileClient {
    async fn get(&self, url: &Url) -> ArtifactIngressResult<FileHttpResponse> {
        let response = self
            .client
            .get(url.clone())
            .header(http::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| {
                ArtifactIngressError::new(
                    "file_download_failed",
                    "The native file could not be downloaded from the OpenAI file service.",
                )
            })?;
        let status = response.status();
        let headers = response.headers().clone();
        Ok(FileHttpResponse {
            status,
            headers,
            body: Box::new(ReqwestFileBody { response }),
        })
    }
}

pub(crate) struct OpenedOpenAiFile {
    pub body: Box<dyn FileBody>,
    pub content_length: Option<u64>,
    pub mime_type: Option<String>,
    pub source_host: String,
}

pub(crate) async fn open_openai_file(
    client: &dyn FileHttpClient,
    file: &OpenAiFileParam,
    config: &ArtifactIngressConfig,
) -> ArtifactIngressResult<OpenedOpenAiFile> {
    let mut url = file.validated_url()?;
    validate_download_url(&url, &config.allowed_hosts)?;
    let mut redirects = 0_u8;

    loop {
        let response = client.get(&url).await?;
        if is_redirect(response.status) {
            if redirects >= config.max_redirects {
                return Err(ArtifactIngressError::new(
                    "file_redirect_invalid",
                    "The native file download exceeded the allowed redirect limit.",
                ));
            }
            let location = response
                .headers
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ArtifactIngressError::new(
                        "file_redirect_invalid",
                        "The native file download returned an invalid redirect.",
                    )
                })?;
            let redirected = url.join(location).map_err(|_| {
                ArtifactIngressError::new(
                    "file_redirect_invalid",
                    "The native file download returned an invalid redirect.",
                )
            })?;
            validate_download_url(&redirected, &config.allowed_hosts)?;
            url = redirected;
            redirects += 1;
            continue;
        }

        if response.status != StatusCode::OK {
            return Err(ArtifactIngressError::new(
                "file_download_failed",
                format!(
                    "The OpenAI file service returned HTTP status {}.",
                    response.status.as_u16()
                ),
            ));
        }

        let content_length = parse_content_length(&response.headers)?;
        if content_length.is_some_and(|length| length > config.max_file_bytes) {
            return Err(ArtifactIngressError::new(
                "file_too_large",
                format!(
                    "The native file exceeds the configured {} byte limit.",
                    config.max_file_bytes
                ),
            ));
        }
        let mime_type = file
            .mime_type
            .clone()
            .or_else(|| response_mime_type(&response.headers));
        let source_host = url
            .host_str()
            .expect("validated OpenAI file URL must have a hostname")
            .to_string();
        return Ok(OpenedOpenAiFile {
            body: response.body,
            content_length,
            mime_type,
            source_host,
        });
    }
}

fn parse_content_length(headers: &HeaderMap) -> ArtifactIngressResult<Option<u64>> {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return Ok(None);
    };
    let text = value.to_str().map_err(|_| {
        ArtifactIngressError::new(
            "file_download_failed",
            "The native file response contained an invalid content length.",
        )
    })?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ArtifactIngressError::new(
            "file_download_failed",
            "The native file response contained an invalid content length.",
        ));
    }
    let length = text.parse::<u64>().map_err(|_| {
        ArtifactIngressError::new(
            "file_download_failed",
            "The native file response contained an invalid content length.",
        )
    })?;
    Ok(Some(length))
}

fn response_mime_type(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    let mime_type = value.split(';').next()?.trim();
    if mime_type.is_empty()
        || mime_type.len() > MAX_MIME_TYPE_BYTES
        || mime_type.chars().any(|character| character.is_control())
    {
        return None;
    }
    Some(mime_type.to_string())
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct FakeBody {
        chunks: VecDeque<ArtifactIngressResult<Option<Bytes>>>,
    }

    #[async_trait]
    impl FileBody for FakeBody {
        async fn next_chunk(&mut self) -> ArtifactIngressResult<Option<Bytes>> {
            self.chunks.pop_front().unwrap_or(Ok(None))
        }
    }

    struct FakeClient {
        responses: Mutex<VecDeque<FileHttpResponse>>,
        urls: Mutex<Vec<String>>,
    }

    impl FakeClient {
        fn new(responses: Vec<FileHttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                urls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl FileHttpClient for FakeClient {
        async fn get(&self, url: &Url) -> ArtifactIngressResult<FileHttpResponse> {
            self.urls.lock().unwrap().push(url.as_str().to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ArtifactIngressError::new("test_error", "No fake response remains."))
        }
    }

    fn response(status: StatusCode, headers: HeaderMap) -> FileHttpResponse {
        FileHttpResponse {
            status,
            headers,
            body: Box::new(FakeBody {
                chunks: VecDeque::new(),
            }),
        }
    }

    fn file(url: &str) -> OpenAiFileParam {
        OpenAiFileParam {
            download_url: url.to_string(),
            file_id: "file_test".to_string(),
            mime_type: None,
            file_name: None,
        }
    }

    fn config() -> ArtifactIngressConfig {
        ArtifactIngressConfig::default()
    }

    fn hosts(patterns: &[&str]) -> Vec<String> {
        patterns.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn wildcard_allows_public_hosts_but_never_internal_ones() {
        let allowed = hosts(&["*"]);
        for url in [
            "https://files.oaiusercontent.com/download?sig=secret",
            "https://cdn.example.org/object",
            "https://example.com/object",
            "https://files.oaiusercontent.com:443/download",
        ] {
            assert!(
                validate_download_url(&Url::parse(url).unwrap(), &allowed).is_ok(),
                "{url}"
            );
        }

        for url in [
            // Transport rules hold under the wildcard too.
            "http://example.com/download",
            "https://user@example.com/download",
            "https://example.com/download#fragment",
            "https://example.com:8443/download",
            // Internal / reserved targets are the SSRF floor the wildcard keeps.
            "https://127.0.0.1/download",
            "https://localhost/download",
            "https://host.localhost/download",
            "https://169.254.169.254/latest/meta-data/",
            "https://10.0.0.5/download",
            "https://192.168.1.10/download",
            "https://172.16.9.9/download",
            "https://100.64.1.1/download",
            "https://[::1]/download",
            "https://[fe80::1]/download",
            "https://[fc00::1]/download",
            "https://[::ffff:127.0.0.1]/download",
        ] {
            let error = validate_download_url(&Url::parse(url).unwrap(), &allowed).unwrap_err();
            assert_eq!(error.code(), "untrusted_file_url", "{url}");
        }
    }

    #[test]
    fn explicit_allowlist_restricts_to_named_hosts() {
        let allowed = hosts(&[".oaiusercontent.com", "files.example.com"]);
        for url in [
            "https://files.oaiusercontent.com/download",
            "https://nested.file-service.oaiusercontent.com/download",
            "https://oaiusercontent.com/download",
            "https://files.example.com/object",
            // An explicitly named host may use a non-standard port.
            "https://files.example.com:8443/object",
        ] {
            assert!(
                validate_download_url(&Url::parse(url).unwrap(), &allowed).is_ok(),
                "{url}"
            );
        }

        for url in [
            "https://example.com/download",
            "https://files.oaiusercontent.com.evil.example/download",
            "https://notoaiusercontent.com/download",
            "https://other.example.com/object",
        ] {
            let error = validate_download_url(&Url::parse(url).unwrap(), &allowed).unwrap_err();
            assert_eq!(error.code(), "untrusted_file_url", "{url}");
        }
    }

    #[test]
    fn an_explicitly_named_internal_host_is_trusted_as_given() {
        // Naming an internal host opts into it deliberately; the wildcard floor
        // does not apply to hosts the operator listed by name.
        let allowed = hosts(&["10.0.0.5", "internal.corp"]);
        for url in [
            "https://10.0.0.5/object",
            "https://10.0.0.5:8443/object",
            "https://internal.corp/object",
        ] {
            assert!(
                validate_download_url(&Url::parse(url).unwrap(), &allowed).is_ok(),
                "{url}"
            );
        }
    }

    #[test]
    fn native_file_reference_is_strict_and_bounded() {
        let parsed = OpenAiFileParam::parse(&serde_json::json!({
            "download_url": "https://files.oaiusercontent.com/object",
            "file_id": "file_abc",
            "mime_type": "application/pdf",
            "file_name": "input.pdf"
        }))
        .unwrap();
        assert_eq!(parsed.file_name.as_deref(), Some("input.pdf"));

        for value in [
            serde_json::json!({
                "download_url": "https://files.oaiusercontent.com/object",
                "file_id": "file_abc",
                "extra": true
            }),
            serde_json::json!({
                "download_url": "https://files.oaiusercontent.com/object",
                "file_id": ""
            }),
            serde_json::json!({
                "download_url": "https://files.oaiusercontent.com/object",
                "file_id": "file_abc",
                "file_name": "bad\nname"
            }),
        ] {
            assert_eq!(
                OpenAiFileParam::parse(&value).unwrap_err().code(),
                "invalid_file_reference"
            );
        }
    }

    #[tokio::test]
    async fn follows_only_revalidated_redirects() {
        let mut redirect_headers = HeaderMap::new();
        redirect_headers.insert(LOCATION, "/next".parse().unwrap());
        let mut final_headers = HeaderMap::new();
        final_headers.insert(CONTENT_LENGTH, "4".parse().unwrap());
        final_headers.insert(CONTENT_TYPE, "text/plain; charset=utf-8".parse().unwrap());
        let client = FakeClient::new(vec![
            response(StatusCode::TEMPORARY_REDIRECT, redirect_headers),
            response(StatusCode::OK, final_headers),
        ]);

        let opened = open_openai_file(
            &client,
            &file("https://files.oaiusercontent.com/start"),
            &config(),
        )
        .await
        .unwrap();
        assert_eq!(opened.content_length, Some(4));
        assert_eq!(opened.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(
            client.urls.lock().unwrap().as_slice(),
            [
                "https://files.oaiusercontent.com/start",
                "https://files.oaiusercontent.com/next"
            ]
        );
    }

    #[tokio::test]
    async fn rejects_redirects_outside_a_restricted_allowlist() {
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, "https://example.com/stolen".parse().unwrap());
        let client = FakeClient::new(vec![response(StatusCode::FOUND, headers)]);
        let mut config = config();
        config.allowed_hosts = hosts(&[".oaiusercontent.com"]);

        let error = open_openai_file(
            &client,
            &file("https://files.oaiusercontent.com/start"),
            &config,
        )
        .await
        .err()
        .expect("redirect outside the restricted allowlist must fail");
        assert_eq!(error.code(), "untrusted_file_url");
        assert!(!error.to_string().contains("stolen"));
    }

    #[tokio::test]
    async fn wildcard_still_rejects_a_redirect_to_an_internal_address() {
        let mut headers = HeaderMap::new();
        headers.insert(
            LOCATION,
            "https://169.254.169.254/latest/meta-data/".parse().unwrap(),
        );
        let client = FakeClient::new(vec![response(StatusCode::FOUND, headers)]);

        // Default config is the "*" wildcard; the redirect revalidation must still
        // enforce the internal-address floor on every hop.
        let error = open_openai_file(&client, &file("https://cdn.example.org/start"), &config())
            .await
            .err()
            .expect("redirect to an internal address must fail even under the wildcard");
        assert_eq!(error.code(), "untrusted_file_url");
    }

    #[tokio::test]
    async fn enforces_the_redirect_limit() {
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, "/again".parse().unwrap());
        let client = FakeClient::new(vec![response(StatusCode::FOUND, headers)]);
        let mut config = config();
        config.max_redirects = 0;

        let error = open_openai_file(
            &client,
            &file("https://files.oaiusercontent.com/start"),
            &config,
        )
        .await
        .err()
        .expect("redirect limit must be enforced");
        assert_eq!(error.code(), "file_redirect_invalid");
    }

    #[tokio::test]
    async fn rejects_declared_oversize_before_streaming() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "11".parse().unwrap());
        let client = FakeClient::new(vec![response(StatusCode::OK, headers)]);
        let mut config = config();
        config.max_file_bytes = 10;

        let error = open_openai_file(
            &client,
            &file("https://files.oaiusercontent.com/object"),
            &config,
        )
        .await
        .err()
        .expect("declared oversize must fail before streaming");
        assert_eq!(error.code(), "file_too_large");
    }

    #[tokio::test]
    async fn errors_do_not_echo_signed_urls_or_file_ids() {
        let client = FakeClient::new(vec![response(StatusCode::FORBIDDEN, HeaderMap::new())]);
        let mut reference =
            file("https://files.oaiusercontent.com/object?signature=do-not-log-this");
        reference.file_id = "file_do_not_log_this".to_string();

        let error = open_openai_file(&client, &reference, &config())
            .await
            .err()
            .expect("non-success response must fail")
            .to_string();
        assert!(!error.contains("signature"));
        assert!(!error.contains("file_do_not_log_this"));
        assert!(error.contains("403"));
    }
}
