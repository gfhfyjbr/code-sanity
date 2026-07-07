//! Minimal blocking client for OpenAI-compatible HTTP endpoints.
//!
//! Two integrations speak this wire format:
//! - the LLM proposal provider (`POST {base}/chat/completions`) — a local
//!   [kou-router](https://github.com/gfhfyjbr/kou-router) gateway, OpenRouter,
//!   or any other OpenAI-compatible endpoint (see `ProviderConfig` presets);
//! - the embedding index (`POST {base}/embeddings`), OpenRouter by default.
//!
//! Privacy boundary: chat requests carry REAL file content (a proposer must see
//! the original terms to propose aliases), so executing the configured endpoint
//! requires the explicit `--allow-provider-endpoint` confirmation. Embedding
//! requests only ever carry sanitized mirror content — the same text any agent
//! already reads.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

/// Refuse to buffer more than this from a response body (ureq's own default is
/// 10 MiB — too small for an embeddings batch of a large file); an oversized
/// body fails the read instead of being silently truncated.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Error bodies only carry a diagnostic; cap the read well below the payload
/// limit (the message itself is truncated to 500 chars anyway).
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// Transient failures (rate limits, gateway hiccups, transport errors) are
/// retried this many times in total, with exponential backoff between tries.
const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

pub struct OpenAiClient {
    base_url: String,
    api_key: Option<String>,
    /// One agent for the client's lifetime: connections are reused instead of
    /// paying a TCP+TLS handshake per request (embed-index posts one request
    /// per batch).
    agent: ureq::Agent,
    extra_headers: Vec<(&'static str, String)>,
}

impl OpenAiClient {
    /// `api_key_env` names the environment variable holding the key; the key
    /// itself must never live in repo-local config. A loopback endpoint (local
    /// kou-router/Ollama gateway) may run keyless — the Authorization header is
    /// simply omitted; for any remote endpoint a missing/empty variable is a
    /// configuration error surfaced here instead of an HTTP 401 mid-run.
    pub fn new(base_url: &str, api_key_env: &str, timeout_secs: u64) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        let api_key = std::env::var(api_key_env)
            .ok()
            .filter(|key| !key.is_empty());
        if api_key.is_none() && !is_loopback(&base_url) {
            bail!(
                "{api_key_env} is not set; {base_url} is a remote endpoint that \
                 requires an API key (export {api_key_env}=... or point base_url \
                 at a local gateway)"
            );
        }
        // OpenRouter's optional attribution headers; keyed off the host so
        // both the chat and the embeddings path get them for free.
        let extra_headers = if base_url.contains("openrouter.ai") {
            let mut headers = vec![("X-Title", "code-sanity".to_string())];
            let repository = env!("CARGO_PKG_REPOSITORY");
            if !repository.is_empty() {
                headers.push(("HTTP-Referer", repository.to_string()));
            }
            headers
        } else {
            Vec::new()
        };
        Ok(Self {
            base_url,
            api_key,
            // http_status_as_error(false): non-2xx arrives on the Ok path with
            // its body intact, so the retry loop can inspect the status and
            // the error report can quote the server's diagnostic.
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(timeout_secs.max(1))))
                .http_status_as_error(false)
                .build()
                .new_agent(),
            extra_headers,
        })
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{path}", self.base_url);
        let payload = body.to_string();
        let mut backoff = INITIAL_BACKOFF;
        for attempt in 1..=MAX_ATTEMPTS {
            let cause = match self.send(&url, &payload) {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let raw = response
                            .into_body()
                            .with_config()
                            .limit(MAX_RESPONSE_BYTES)
                            .read_to_string()
                            .with_context(|| format!("read response body from {url}"))?;
                        return serde_json::from_str(&raw)
                            .with_context(|| format!("parse JSON response from {url}"));
                    }
                    let code = status.as_u16();
                    if !is_retryable_status(code) || attempt == MAX_ATTEMPTS {
                        let detail = response
                            .into_body()
                            .with_config()
                            .limit(MAX_ERROR_BODY_BYTES)
                            .read_to_string()
                            .unwrap_or_default();
                        bail!("{url} returned HTTP {code}: {}", truncate(&detail, 500));
                    }
                    format!("HTTP {code}")
                }
                // Any transport-level failure (timeout, refused connection,
                // broken pipe) is worth the same retries rate limits get.
                Err(err) if attempt < MAX_ATTEMPTS => err.to_string(),
                Err(err) => return Err(err).with_context(|| format!("POST {url}")),
            };
            log::warn!(
                "POST {url} failed (attempt {attempt}/{MAX_ATTEMPTS}), \
                 retrying in {}s: {cause}",
                backoff.as_secs()
            );
            std::thread::sleep(backoff);
            backoff *= 2;
        }
        unreachable!("retry loop exits via return or bail")
    }

    // ureq::Error is large by value; this private helper exists purely so the
    // retry loop can match on it, so boxing would only add noise.
    #[allow(clippy::result_large_err)]
    fn send(
        &self,
        url: &str,
        payload: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let mut request = self
            .agent
            .post(url)
            .header("content-type", "application/json");
        for (name, value) in &self.extra_headers {
            request = request.header(*name, value.as_str());
        }
        if let Some(key) = &self.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        request.send(payload)
    }

    /// One-shot chat completion; returns the first choice's message content.
    pub fn chat(&self, model: &str, system: &str, user: &str) -> Result<String> {
        let body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "temperature": 0,
        });
        let value = self.post("/chat/completions", &body)?;
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                anyhow!(
                    "chat response has no choices[0].message.content: {}",
                    truncate(&value.to_string(), 300)
                )
            })
    }

    /// Embed a batch of inputs; returns one vector per input, in input order
    /// (the OpenAI contract carries an `index` per entry; we honor it).
    pub fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({ "model": model, "input": inputs });
        let value = self.post("/embeddings", &body)?;
        let data = value["data"]
            .as_array()
            .ok_or_else(|| anyhow!("embeddings response has no `data` array"))?;
        if data.len() != inputs.len() {
            bail!(
                "embeddings response has {} vectors for {} inputs",
                data.len(),
                inputs.len()
            );
        }
        let mut out = vec![Vec::new(); inputs.len()];
        for entry in data {
            let index = entry["index"]
                .as_u64()
                .ok_or_else(|| anyhow!("embeddings entry missing `index`"))?
                as usize;
            let vector = entry["embedding"]
                .as_array()
                .ok_or_else(|| anyhow!("embeddings entry missing `embedding`"))?
                .iter()
                .map(|component| {
                    component
                        .as_f64()
                        .map(|value| value as f32)
                        .ok_or_else(|| anyhow!("non-numeric embedding component"))
                })
                .collect::<Result<Vec<f32>>>()?;
            let slot = out
                .get_mut(index)
                .ok_or_else(|| anyhow!("embedding index {index} out of range"))?;
            *slot = vector;
        }
        if let Some(position) = out.iter().position(Vec::is_empty) {
            bail!("embeddings response is missing a vector for input {position}");
        }
        Ok(out)
    }
}

/// Worth a retry: rate limits and gateway hiccups clear on their own.
/// Anything else (4xx, auth) is permanent.
fn is_retryable_status(code: u16) -> bool {
    matches!(code, 429 | 502 | 503 | 504)
}

/// Loopback endpoints (a local gateway) may legitimately run keyless; anything
/// else without a key would only fail later with a confusing 401.
fn is_loopback(base_url: &str) -> bool {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(v6) = host.strip_prefix('[') {
        v6.split(']').next().unwrap_or(v6)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::{OpenAiClient, is_loopback, truncate};

    #[test]
    fn truncate_respects_char_boundaries() {
        let text = "приватный контекст";
        let cut = truncate(text, 5);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= 5 + '…'.len_utf8());
    }

    #[test]
    fn loopback_detection_covers_common_shapes() {
        assert!(is_loopback("http://127.0.0.1:20128/v1"));
        assert!(is_loopback("http://localhost:8080"));
        assert!(is_loopback("http://[::1]:11434/v1"));
        assert!(!is_loopback("https://openrouter.ai/api/v1"));
        assert!(!is_loopback("http://192.168.1.10:8080/v1"));
    }

    #[test]
    fn preflight_requires_key_for_remote_endpoints_only() {
        // `.err()` instead of `.unwrap_err()`: OpenAiClient deliberately has
        // no Debug impl (it holds the API key).
        let err = OpenAiClient::new(
            "https://openrouter.ai/api/v1",
            "CODE_SANITY_TEST_KEY_UNSET",
            10,
        )
        .err()
        .expect("remote endpoint without a key must fail preflight");
        let message = err.to_string();
        assert!(message.contains("CODE_SANITY_TEST_KEY_UNSET"));
        assert!(message.contains("openrouter.ai"));
        assert!(
            OpenAiClient::new("http://127.0.0.1:1/v1", "CODE_SANITY_TEST_KEY_UNSET", 10).is_ok()
        );
    }
}
