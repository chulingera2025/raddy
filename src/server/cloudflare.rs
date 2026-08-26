// Copyright (c) 2026 chulingera2025
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Cloudflare DNS-01 challenge provider (spec §5.3).
//!
//! When the Raddyfile sets `dns_challenge cloudflare <token>`, certificate
//! issuance proves domain control by publishing `_acme-challenge.<host>` TXT
//! records through the Cloudflare API v4 instead of answering HTTP-01. This
//! module implements the three API calls involved: finding the zone that owns a
//! hostname, creating a TXT record, and deleting it again after the order.
//!
//! The ACME issuance worker runs on its own single-threaded runtime and
//! serializes orders, so blocking I/O here is acceptable — but every call is
//! *bounded*: the shared ureq [`Agent`] has finite connect/read/write/overall
//! timeouts and a resolver that delegates to the fixed resolver pool, so a
//! hung Cloudflare API (DNS, connect, or an unresponsive server) can never
//! block the single issuance worker (or its DNS-01 cleanup) forever.

use crate::server::dns::{DnsProvider, DnsRecord};
use serde::Deserialize;
use std::time::Duration;

/// Cloudflare API v4 base URL.
const DEFAULT_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Connect timeout for Cloudflare API requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-read/write and overall timeout for Cloudflare API requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A Cloudflare DNS-01 client. The API token is a secret and must have
/// `Zone: DNS: Edit` permission. `Clone` so a [`RecordHandle`] can carry the
/// client needed to remove its TXT record later.
#[derive(Clone)]
pub struct Cloudflare {
    api_token: String,
    base_url: String,
    /// Shared agent with finite timeouts and the bounded resolver, so a hung
    /// API call cannot block the single ACME issuance worker (or its DNS-01
    /// cleanup) indefinitely.
    agent: ureq::Agent,
}

/// A TXT record created by [`Cloudflare::present_txt`], kept so
/// [`DnsRecord::cleanup`] can remove it once the ACME order has been validated.
/// It carries its own client, so the handle is self-sufficient through the
/// [`DnsRecord`] trait.
#[derive(Clone)]
pub struct RecordHandle {
    client: Cloudflare,
    zone_id: String,
    record_id: String,
}

impl DnsRecord for RecordHandle {
    /// Remove the TXT record.
    fn cleanup(self: Box<Self>) -> Result<(), String> {
        self.client.delete_txt(&self.zone_id, &self.record_id)
    }
}

impl DnsProvider for Cloudflare {
    fn present(&self, host: &str, dns_value: &str) -> Result<Box<dyn DnsRecord>, String> {
        Ok(Box::new(self.present_txt(host, dns_value)?))
    }
}

#[cfg(test)]
impl RecordHandle {
    /// The zone the record was created in (test helper).
    fn zone_id(&self) -> &str {
        &self.zone_id
    }
}

impl Cloudflare {
    /// Create a client bound to the Cloudflare production API.
    ///
    /// The API base URL can be overridden with the `RADDY_CLOUDFLARE_API_BASE`
    /// environment variable (used by the test suite to point at a mock).
    pub fn new(api_token: &str) -> Self {
        let base_url = std::env::var("RADDY_CLOUDFLARE_API_BASE")
            .unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        Self::with_agent(
            api_token,
            &base_url,
            build_agent(CONNECT_TIMEOUT, REQUEST_TIMEOUT),
        )
    }

    /// Create a client against an explicit API base URL (test hook).
    pub fn with_base(api_token: &str, base_url: &str) -> Self {
        Self::with_agent(
            api_token,
            base_url,
            build_agent(CONNECT_TIMEOUT, REQUEST_TIMEOUT),
        )
    }

    /// Create a client with explicit API timeouts (test hook).
    #[cfg(test)]
    fn with_timeouts(
        api_token: &str,
        base_url: &str,
        connect: Duration,
        request: Duration,
    ) -> Self {
        Self::with_agent(api_token, base_url, build_agent(connect, request))
    }

    /// Assemble a client around a ready-made agent (shared constructor).
    fn with_agent(api_token: &str, base_url: &str, agent: ureq::Agent) -> Self {
        Self {
            api_token: api_token.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// Publish the DNS-01 challenge: create a `_acme-challenge.<host>` TXT
    /// record whose content is the key authorization. Returns a handle that
    /// carries the client and removes the record via [`DnsRecord::cleanup`].
    fn present_txt(&self, host: &str, dns_value: &str) -> Result<RecordHandle, String> {
        // ACME represents wildcard identifiers with a leading `*.`; DNS-01
        // proves the base domain, so the wildcard label must not enter either
        // zone discovery or the TXT record name.
        let dns_host = host.strip_prefix("*.").unwrap_or(host);
        let zone_id = self.find_zone_id(dns_host)?;
        let record_name = format!("_acme-challenge.{dns_host}");
        let record_id = self.create_txt(&zone_id, &record_name, dns_value)?;
        Ok(RecordHandle {
            client: self.clone(),
            zone_id,
            record_id,
        })
    }

    /// Find the zone that owns `host` by querying each hostname suffix from
    /// longest to shortest (`sub.example.com` → `example.com` → `com`).
    fn find_zone_id(&self, host: &str) -> Result<String, String> {
        let mut suffix = host.trim_end_matches('.').to_string();
        loop {
            if let Some(id) = self.lookup_zone(&suffix)? {
                return Ok(id);
            }
            match suffix.split_once('.') {
                Some((_, rest)) if !rest.is_empty() => suffix = rest.to_string(),
                _ => break,
            }
        }
        Err(format!(
            "no Cloudflare zone found for '{host}' (is it a zone on this account?)"
        ))
    }

    /// `GET /zones?name=<name>&status=active`; returns the first zone id.
    fn lookup_zone(&self, name: &str) -> Result<Option<String>, String> {
        let url = format!("{}/zones?name={name}&status=active", self.base_url);
        let zones: CfResponse<Vec<CfZone>> = self.request(self.auth_get(&url).call())?;
        Ok(zones.result.into_iter().next().map(|zone| zone.id))
    }

    /// `POST /zones/{zone_id}/dns_records` with a TXT record; returns its id.
    fn create_txt(&self, zone_id: &str, name: &str, content: &str) -> Result<String, String> {
        let url = format!("{}/zones/{zone_id}/dns_records", self.base_url);
        let body = serde_json::json!({
            "type": "TXT",
            "name": name,
            "content": content,
            "ttl": 120,
        });
        let record: CfResponse<CfDnsRecord> =
            self.request(self.auth_post(&url).send_json(&body))?;
        Ok(record.result.id)
    }

    /// `DELETE /zones/{zone_id}/dns_records/{record_id}`.
    fn delete_txt(&self, zone_id: &str, record_id: &str) -> Result<(), String> {
        let url = format!("{}/zones/{zone_id}/dns_records/{record_id}", self.base_url);
        let _: CfResponse<serde_json::Value> = self.request(self.auth_delete(&url).call())?;
        Ok(())
    }

    /// Build a GET request carrying the bearer token.
    fn auth_get(&self, url: &str) -> ureq::Request {
        self.agent.get(url).set("Authorization", &self.bearer())
    }

    /// Build a POST request carrying the bearer token.
    fn auth_post(&self, url: &str) -> ureq::Request {
        self.agent.post(url).set("Authorization", &self.bearer())
    }

    /// Build a DELETE request carrying the bearer token.
    fn auth_delete(&self, url: &str) -> ureq::Request {
        self.agent.delete(url).set("Authorization", &self.bearer())
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.api_token)
    }

    /// Run a request and decode the Cloudflare envelope, surfacing API errors
    /// as a readable message.
    fn request<T: serde::de::DeserializeOwned>(
        &self,
        result: Result<ureq::Response, ureq::Error>,
    ) -> Result<CfResponse<T>, String> {
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(format!("Cloudflare API error (HTTP {code}): {body}"));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(format!("Cloudflare API transport error: {transport}"));
            }
        };
        let envelope: CfResponse<T> = response
            .into_json()
            .map_err(|e| format!("invalid Cloudflare API response: {e}"))?;
        if !envelope.success {
            let messages: Vec<String> = envelope.errors.iter().map(|e| e.message.clone()).collect();
            return Err(if messages.is_empty() {
                "Cloudflare API reported failure".to_string()
            } else {
                format!("Cloudflare API failure: {}", messages.join("; "))
            });
        }
        Ok(envelope)
    }
}

/// Build a ureq [`Agent`] whose requests are fully bounded: finite connect,
/// read, write, and overall timeouts, plus a resolver that delegates to the
/// fixed resolver pool (ureq's own DNS cannot be interrupted by a request
/// timeout). `request` bounds the overall request as well as each read/write.
fn build_agent(connect_timeout: Duration, request_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(connect_timeout)
        .timeout_read(request_timeout)
        .timeout_write(request_timeout)
        .timeout(request_timeout)
        .resolver(crate::config::resolver::agent_resolver)
        .build()
}

/// The Cloudflare API response envelope: `{ success, errors, result }`.
#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
    result: T,
}

#[derive(Debug, Deserialize)]
struct CfError {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct CfZone {
    id: String,
}

#[derive(Deserialize)]
struct CfDnsRecord {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A minimal in-test Cloudflare API server.
    ///
    /// It knows one zone (`example.com`) and records every DNS record it is
    /// asked to create so the test can assert the TXT name and content.
    struct MockApi {
        created: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl MockApi {
        /// Bind a listener, spawn the serving thread, and return the base URL
        /// plus a handle to the list of created records.
        fn start() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
            let addr = listener.local_addr().expect("mock address");
            let created: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
            let api = Self {
                created: created.clone(),
            };
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let Ok(request) = read_request(&mut stream) else {
                        continue;
                    };
                    let (status, body) = api.route(&request);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            (format!("http://{}", addr), created)
        }

        fn route(&self, request: &MockRequest) -> (&'static str, String) {
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", path) if path.starts_with("/zones") => {
                    let name = query_param(path, "name");
                    let json = if name.as_deref() == Some("example.com") {
                        r#"{"success":true,"result":[{"id":"zone_example"}]}"#
                    } else {
                        r#"{"success":true,"result":[]}"#
                    };
                    ("200 OK", json.to_string())
                }
                ("POST", path) if path.starts_with("/zones/") && path.ends_with("/dns_records") => {
                    let body: serde_json::Value =
                        serde_json::from_str(&request.body).unwrap_or_default();
                    self.created.lock().unwrap().push(body);
                    let id = format!("record_{}", self.created.lock().unwrap().len());
                    (
                        "200 OK",
                        serde_json::json!({ "success": true, "result": { "id": id } }).to_string(),
                    )
                }
                ("DELETE", path) if path.contains("/dns_records/") => {
                    let id = path.rsplit('/').next().unwrap_or_default();
                    (
                        "200 OK",
                        serde_json::json!({ "success": true, "result": { "id": id } }).to_string(),
                    )
                }
                _ => (
                    "404 Not Found",
                    r#"{"success":false,"errors":[{"message":"not found"}]}"#.to_string(),
                ),
            }
        }
    }

    /// One parsed HTTP request from the mock listener.
    struct MockRequest {
        method: String,
        path: String,
        body: String,
    }

    fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<MockRequest> {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();

        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        Ok(MockRequest {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn query_param(path: &str, key: &str) -> Option<String> {
        path.split('?').nth(1).and_then(|query| {
            query.split('&').find_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                match (kv.next(), kv.next()) {
                    (Some(k), Some(v)) if k == key => Some(v.to_string()),
                    _ => None,
                }
            })
        })
    }

    #[test]
    fn present_creates_txt_record_and_cleanup_deletes_it() {
        let (base, created) = MockApi::start();
        let client = Cloudflare::with_base("test-token", &base);

        let handle = client
            .present_txt("example.com", "token.thumbprint")
            .unwrap();
        // The zone was found and the record created.
        assert_eq!(handle.zone_id(), "zone_example");

        let records = created.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "TXT");
        assert_eq!(records[0]["name"], "_acme-challenge.example.com");
        assert_eq!(records[0]["content"], "token.thumbprint");
        drop(records);

        // Cleanup issues a DELETE for the created record.
        Box::new(handle).cleanup().unwrap();
    }

    #[test]
    fn finds_zone_by_stripping_subdomains() {
        let (base, created) = MockApi::start();
        let client = Cloudflare::with_base("test-token", &base);

        // `sub.example.com` is not a zone; discovery falls back to `example.com`.
        let handle = client.present_txt("sub.example.com", "ka").unwrap();
        assert_eq!(handle.zone_id(), "zone_example");

        let records = created.lock().unwrap();
        assert_eq!(records[0]["name"], "_acme-challenge.sub.example.com");
    }

    #[test]
    fn wildcard_dns01_uses_the_base_domain_record_name() {
        let (base, created) = MockApi::start();
        let client = Cloudflare::with_base("test-token", &base);

        client.present_txt("*.example.com", "ka").unwrap();

        let records = created.lock().unwrap();
        assert_eq!(records[0]["name"], "_acme-challenge.example.com");
    }

    #[test]
    fn errors_when_no_zone_owns_the_host() {
        let (base, _) = MockApi::start();
        let client = Cloudflare::with_base("test-token", &base);
        let err = client.present_txt("other.org", "ka").err().unwrap();
        assert!(err.contains("no Cloudflare zone found"), "got: {err}");
    }

    #[test]
    fn request_times_out_when_server_withholds_response() {
        // A server that accepts the connection and reads the request but never
        // sends a response. The client's finite request timeout must surface as
        // a transport error promptly, never hanging the test (or, in
        // production, the single ACME issuance worker and its DNS-01 cleanup).
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = listener.local_addr().expect("mock address");
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Read the request so the client has fully sent it, then withhold
            // the response until the test releases us.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = release_rx.recv();
        });

        let client = Cloudflare::with_timeouts(
            "test-token",
            &format!("http://{addr}"),
            Duration::from_millis(100),
            Duration::from_millis(200),
        );
        let start = std::time::Instant::now();
        let err = client.present_txt("example.com", "ka").err().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "a withheld response must time out, not hang (took {elapsed:?})"
        );
        assert!(
            err.contains("transport"),
            "expected a transport error, got: {err}"
        );

        let _ = release_tx.send(());
        server.join().expect("server thread joins");
    }
}
