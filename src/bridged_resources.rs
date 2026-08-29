//! Opaque downstream capabilities for resources returned by bridged MCP tools.
//!
//! An upstream `resource_link` URI is never used as the downstream routing URI. A
//! tool result that contains a `resource_link` is rewritten to a short-lived
//! `codexify://upstream-resource/<token>` capability. A later downstream
//! `resources/read` is routed back to the originating upstream peer and the
//! returned content URIs are rewritten to the opaque downstream capability.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use getrandom::getrandom;
use rmcp::{
    RoleClient,
    model::{
        CacheScope, CancelledNotification, CancelledNotificationParam, ClientRequest,
        ReadResourceRequest, ReadResourceRequestParams, ReadResourceResult, Resource,
        ResourceContents, ServerResult,
    },
    service::{Peer, PeerRequestOptions, ServiceError},
};
use tokio_util::sync::CancellationToken;

use crate::types::ArtifactEgressConfig;

pub const BRIDGED_RESOURCE_URI_PREFIX: &str = "codexify://upstream-resource/";
const TOKEN_BYTES: usize = 32;
const TOKEN_LENGTH: usize = 43;
const TOKEN_ATTEMPTS: usize = 8;
const MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct BridgedResourceError {
    message: String,
}

impl BridgedResourceError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BridgedResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BridgedResourceError {}

#[derive(Clone)]
struct StoredResource {
    peer: Peer<RoleClient>,
    upstream_uri: String,
    upstream_server: String,
    timeout: Option<Duration>,
    expires_at: Instant,
}

#[derive(Default)]
struct StoreState {
    entries: HashMap<String, StoredResource>,
    order: VecDeque<String>,
}

impl StoreState {
    fn prune_expired(&mut self, now: Instant) {
        while let Some(token) = self.order.front().cloned() {
            match self.entries.get(&token) {
                Some(entry) if entry.expires_at > now => break,
                Some(_) => {
                    self.order.pop_front();
                    self.entries.remove(&token);
                }
                None => {
                    self.order.pop_front();
                }
            }
        }
    }

    fn evict_oldest(&mut self) -> bool {
        while let Some(token) = self.order.pop_front() {
            if self.entries.remove(&token).is_some() {
                return true;
            }
        }
        false
    }
}

pub struct BridgedResourceStore {
    config: ArtifactEgressConfig,
    state: Mutex<StoreState>,
}

impl BridgedResourceStore {
    pub fn new(config: ArtifactEgressConfig) -> Self {
        Self {
            config,
            state: Mutex::new(StoreState::default()),
        }
    }

    pub fn register(
        &self,
        peer: Peer<RoleClient>,
        upstream_server: &str,
        timeout: Option<Duration>,
        mut resource: Resource,
    ) -> Result<Resource, BridgedResourceError> {
        if !self.config.enabled {
            return Err(BridgedResourceError::new(
                "bridged resource egress is disabled by artifactEgress.enabled",
            ));
        }
        if resource.uri.is_empty() {
            return Err(BridgedResourceError::new(
                "upstream MCP returned a resource link with an empty URI",
            ));
        }
        if resource
            .size
            .is_some_and(|size| size > self.config.max_file_bytes)
        {
            return Err(BridgedResourceError::new(
                "upstream MCP resource exceeds artifactEgress.maxFileBytes",
            ));
        }
        let descriptor_size = serde_json::to_vec(&resource)
            .map_err(|_| BridgedResourceError::new("upstream MCP resource descriptor is invalid"))?
            .len();
        if descriptor_size > MAX_DESCRIPTOR_BYTES {
            return Err(BridgedResourceError::new(
                "upstream MCP resource descriptor is too large",
            ));
        }

        let now = Instant::now();
        let expires_at = now
            .checked_add(Duration::from_millis(self.config.reference_ttl_ms))
            .ok_or_else(|| BridgedResourceError::new("resource reference TTL is too large"))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.prune_expired(now);

        let token = (0..TOKEN_ATTEMPTS)
            .find_map(|_| {
                let token = generate_token().ok()?;
                (!state.entries.contains_key(&token)).then_some(token)
            })
            .ok_or_else(|| {
                BridgedResourceError::new("could not allocate a bridged resource reference")
            })?;

        while state.entries.len() >= self.config.max_references {
            if !state.evict_oldest() {
                return Err(BridgedResourceError::new(
                    "bridged resource reference cache is full",
                ));
            }
        }

        let upstream_uri = std::mem::replace(
            &mut resource.uri,
            format!("{BRIDGED_RESOURCE_URI_PREFIX}{token}"),
        );
        state.order.push_back(token.clone());
        state.entries.insert(
            token,
            StoredResource {
                peer,
                upstream_uri,
                upstream_server: upstream_server.to_string(),
                timeout,
                expires_at,
            },
        );
        Ok(resource)
    }

    pub async fn read_resource(
        &self,
        uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<ReadResourceResult>, BridgedResourceError> {
        let Some(token) = parse_token(uri) else {
            return Ok(None);
        };
        let entry = {
            let now = Instant::now();
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.prune_expired(now);
            let Some(entry) = state.entries.get(token).cloned() else {
                return Ok(None);
            };
            entry
        };

        let mut result = read_upstream_resource(&entry, cancellation).await?;
        validate_and_rewrite_contents(&mut result, uri, self.config.max_file_bytes)?;
        let remaining = entry.expires_at.saturating_duration_since(Instant::now());
        let remaining_ttl_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
        result.ttl_ms = Some(
            result
                .ttl_ms
                .map_or(remaining_ttl_ms, |ttl| ttl.min(remaining_ttl_ms)),
        );
        // The downstream capability is scoped to this Codexify process and may
        // route through an authorization-bearing upstream peer. Never widen it
        // to an upstream `public` cache scope.
        result.cache_scope = Some(CacheScope::Private);
        Ok(Some(result))
    }
}

async fn read_upstream_resource(
    entry: &StoredResource,
    cancellation: &CancellationToken,
) -> Result<ReadResourceResult, BridgedResourceError> {
    if cancellation.is_cancelled() {
        return Err(BridgedResourceError::new(
            "bridged resource read was cancelled by the downstream client",
        ));
    }

    let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(
        ReadResourceRequestParams::new(entry.upstream_uri.clone()),
    ));
    let options = entry
        .timeout
        .map(PeerRequestOptions::with_timeout)
        .unwrap_or_else(PeerRequestOptions::no_options);
    let handle = entry
        .peer
        .send_cancellable_request(request, options)
        .await
        .map_err(|error| {
            tracing::debug!(
                upstream_mcp = %entry.upstream_server,
                error = %error,
                "could not start bridged resources/read"
            );
            BridgedResourceError::new(format!(
                "upstream MCP '{}' could not start the resource read",
                entry.upstream_server
            ))
        })?;

    let request_id = handle.id.clone();
    let cancel_peer = entry.peer.clone();
    let response = handle.await_response();
    tokio::pin!(response);
    let response = tokio::select! {
        response = &mut response => response,
        _ = cancellation.cancelled() => {
            let notification = CancelledNotification::new(CancelledNotificationParam::new(
                Some(request_id),
                Some("downstream resource read cancelled".to_string()),
            ));
            if let Err(error) = cancel_peer.send_notification(notification.into()).await {
                tracing::debug!("could not forward upstream MCP resource cancellation: {error}");
            }
            return Err(BridgedResourceError::new(
                "bridged resource read was cancelled by the downstream client",
            ));
        }
    };

    let response = match response {
        Ok(response) => response,
        Err(ServiceError::Timeout { timeout }) => {
            return Err(BridgedResourceError::new(format!(
                "upstream MCP '{}' resource read timed out after {}s",
                entry.upstream_server,
                timeout.as_secs_f64()
            )));
        }
        Err(error) => {
            tracing::debug!(
                upstream_mcp = %entry.upstream_server,
                error = %error,
                "bridged upstream resources/read failed"
            );
            return Err(BridgedResourceError::new(format!(
                "upstream MCP '{}' resource read failed",
                entry.upstream_server
            )));
        }
    };

    match response {
        ServerResult::ReadResourceResult(result) => Ok(result),
        ServerResult::InputRequiredResult(_) => Err(BridgedResourceError::new(format!(
            "upstream MCP '{}' requested interactive input while reading a resource, which Codexify cannot provide",
            entry.upstream_server
        ))),
        _ => Err(BridgedResourceError::new(format!(
            "upstream MCP '{}' returned an unexpected response to resources/read",
            entry.upstream_server
        ))),
    }
}

fn validate_and_rewrite_contents(
    result: &mut ReadResourceResult,
    public_uri: &str,
    max_file_bytes: u64,
) -> Result<(), BridgedResourceError> {
    let mut total_bytes = 0u64;
    for content in &mut result.contents {
        let bytes = match content {
            ResourceContents::TextResourceContents { uri, text, .. } => {
                *uri = public_uri.to_string();
                u64::try_from(text.len()).unwrap_or(u64::MAX)
            }
            ResourceContents::BlobResourceContents { uri, blob, .. } => {
                *uri = public_uri.to_string();
                base64_decoded_len(blob)?
            }
            _ => {
                return Err(BridgedResourceError::new(
                    "upstream MCP returned an unsupported resource content type",
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(bytes);
        if total_bytes > max_file_bytes {
            return Err(BridgedResourceError::new(
                "upstream MCP resource exceeds artifactEgress.maxFileBytes",
            ));
        }
    }
    Ok(())
}

fn base64_decoded_len(blob: &str) -> Result<u64, BridgedResourceError> {
    let bytes = blob.as_bytes();
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return Err(BridgedResourceError::new(
            "upstream MCP returned invalid base64 resource content",
        ));
    }
    let unpadded_len = bytes.len().saturating_sub(padding);
    if bytes[..unpadded_len]
        .iter()
        .any(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/'))
        || bytes[..unpadded_len].contains(&b'=')
        || unpadded_len % 4 == 1
        || (padding > 0 && !bytes.len().is_multiple_of(4))
        || (padding == 1 && unpadded_len % 4 != 3)
        || (padding == 2 && unpadded_len % 4 != 2)
    {
        return Err(BridgedResourceError::new(
            "upstream MCP returned invalid base64 resource content",
        ));
    }

    let full_quads = unpadded_len / 4;
    let remainder_bytes = match unpadded_len % 4 {
        0 => 0usize,
        2 => 1,
        3 => 2,
        _ => unreachable!("remainder 1 rejected above"),
    };
    let decoded = full_quads
        .checked_mul(3)
        .and_then(|value| value.checked_add(remainder_bytes))
        .ok_or_else(|| BridgedResourceError::new("upstream MCP resource is too large"))?;
    u64::try_from(decoded)
        .map_err(|_| BridgedResourceError::new("upstream MCP resource is too large"))
}

fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_token(uri: &str) -> Option<&str> {
    let token = uri.strip_prefix(BRIDGED_RESOURCE_URI_PREFIX)?;
    (token.len() == TOKEN_LENGTH
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_content_uris_without_exposing_the_upstream_uri() {
        let mut result = ReadResourceResult::new(vec![
            ResourceContents::text("hello", "private://upstream/text"),
            ResourceContents::blob("AAEC/w==", "private://upstream/blob")
                .with_mime_type("application/octet-stream"),
        ]);

        validate_and_rewrite_contents(
            &mut result,
            "codexify://upstream-resource/public-token",
            1024,
        )
        .unwrap();

        for content in result.contents {
            match content {
                ResourceContents::TextResourceContents { uri, .. }
                | ResourceContents::BlobResourceContents { uri, .. } => {
                    assert_eq!(uri, "codexify://upstream-resource/public-token");
                    assert!(!uri.contains("private://"));
                }
                _ => panic!("unexpected resource content variant"),
            }
        }
    }

    #[test]
    fn enforces_the_egress_limit_on_actual_resource_contents() {
        let mut text = ReadResourceResult::new(vec![ResourceContents::text(
            "12345",
            "private://upstream/text",
        )]);
        assert!(
            validate_and_rewrite_contents(&mut text, "codexify://upstream-resource/test", 4)
                .unwrap_err()
                .to_string()
                .contains("maxFileBytes")
        );

        let mut blob = ReadResourceResult::new(vec![ResourceContents::blob(
            "AAEC/w==",
            "private://upstream/blob",
        )]);
        assert!(
            validate_and_rewrite_contents(&mut blob, "codexify://upstream-resource/test", 3)
                .unwrap_err()
                .to_string()
                .contains("maxFileBytes")
        );
        assert_eq!(base64_decoded_len("AAEC/w==").unwrap(), 4);
        assert_eq!(base64_decoded_len("aA").unwrap(), 1);
        assert!(base64_decoded_len("not base64!").is_err());
    }

    #[test]
    fn only_well_formed_opaque_capabilities_are_recognized() {
        let token = "abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE";
        assert_eq!(token.len(), TOKEN_LENGTH);
        assert_eq!(
            parse_token(&format!("{BRIDGED_RESOURCE_URI_PREFIX}{token}")),
            Some(token)
        );
        assert!(parse_token("fixture://artifact/report.bin").is_none());
        assert!(parse_token(&format!("{BRIDGED_RESOURCE_URI_PREFIX}short")).is_none());
    }
}
