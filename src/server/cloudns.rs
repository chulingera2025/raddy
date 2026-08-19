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

//! DNS-01 provider for ClouDNS (cloudns.net) (spec §5.3).
//!
//! ClouDNS authenticates with an `auth-id` + `auth-password` pair (an API user
//! id and password, or a sub-user) rather than a single token, so the
//! `dns_challenge` directive carries two credentials for this provider:
//! `dns_challenge cloudns <auth_id> <auth_password>`. Every API request
//! carries both credentials. See [`super::dns`] for the shared trait.

use std::time::Duration;

use serde_json::{json, Map, Value};
use ureq::Agent;

use super::dns::{DnsError, DnsProvider, DnsRecord};

/// The default ClouDNS API endpoint.
const DEFAULT_BASE_URL: &str = "https://api.cloudns.net/";

/// Build a bounded ureq agent: a hard timeout and a connection cap so a slow or
/// hung API can never wedge the single-threaded ACME issuance worker.
fn bounded_agent() -> Agent {
    Agent::config()
        .timeout(Duration::from_secs(30))
        .max_concurrency(4)
        .build()
}

/// ClouDNS DNS-01 provider (spec §5.3).
#[derive(Clone)]
pub struct CloudnsClient {
    agent: Agent,
    base_url: String,
    auth_id: String,
    auth_password: String,
}

impl CloudnsClient {
    /// Create a client for the default ClouDNS endpoint.
    pub fn new(auth_id: String, auth_password: String) -> Self {
        Self {
            agent: bounded_agent(),
            base_url: DEFAULT_BASE_URL.to_string(),
            auth_id,
            auth_password,
        }
    }

    /// Create a client pointed at an arbitrary base URL (used by tests).
    pub(crate) fn with_base(auth_id: String, auth_password: String, base_url: String) -> Self {
        Self {
            agent: bounded_agent(),
            base_url,
            auth_id,
            auth_password,
        }
    }

    /// Issue a GET to `path` with the auth credentials plus `extra` query
    /// params, decoding the JSON body.
    fn request(&self, path: &str, extra: Map<String, Value>) -> Result<Value, DnsError> {
        let mut pairs: Vec<(&str, String)> = vec![
            ("auth-id", self.auth_id.clone()),
            ("auth-password", self.auth_password.clone()),
        ];
        for (k, v) in extra {
            pairs.push((k.as_str(), v.to_string()));
        }
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .agent
            .get(&url)
            .query(pairs)
            .call()
            .map_err(|e| DnsError::Api(format!("ClouDNS {path} request failed: {e}")))?;
        resp.into_json().map_err(|e| DnsError::Api(format!("ClouDNS {path} returned bad JSON: {e}")))
    }

    /// List the records of `zone`.
    fn list_records(&self, zone: &str) -> Result<Vec<Value>, DnsError> {
        let mut p = Map::new();
        p.insert("domain-name".to_string(), json!(zone));
        let v = self.request("dns/records.json", p)?;
        Ok(v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default())
    }

    /// Resolve the owning zone for `host` by suffix match: walk the host's
    /// labels longest-to-shortest and pick the first suffix the API accepts.
    fn zone_for(&self, host: &str) -> Result<String, DnsError> {
        let labels: Vec<&str> = host.split('.').collect();
        for start in 0..labels.len() {
            let candidate = labels[start..].join(".");
            if candidate.is_empty() {
                continue;
            }
            match self.list_records(&candidate) {
                Ok(_) => return Ok(candidate),
                Err(DnsError::Api(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(DnsError::Api(format!("no ClouDNS zone found for {host}")))
    }
}

impl DnsProvider for CloudnsClient {
    /// Publish the `_acme-challenge.<host>` TXT record and return a handle.
    fn present(&self, host: &str, key_authorization: &str) -> Result<Box<dyn DnsRecord>, DnsError> {
        let zone = self.zone_for(host)?;
        let challenge = format!("_acme-challenge.{host}");
        let mut params = Map::new();
        params.insert("domain-name".to_string(), json!(zone));
        params.insert("host".to_string(), json!(challenge));
        params.insert("record".to_string(), json!(key_authorization));
        params.insert("record-type".to_string(), json!("TXT"));
        params.insert("ttl".to_string(), json!(300));
        let v = self.request("dns/add-record.json", params)?;
        let id = v
            .get("data")
            .and_then(|d| d.get("record"))
            .and_then(|r| r.get("id"))
            .and_then(|i| i.as_u64())
            .or_else(|| v.get("data").and_then(|d| d.get("id")).and_then(|i| i.as_u64()))
            .ok_or_else(|| DnsError::Api("ClouDNS add-record: missing record id".to_string()))?;
        Ok(Box::new(CloudnsRecord {
            client: std::sync::Arc::new(self.clone()),
            zone,
            id,
        }))
    }
}

/// A published ClouDNS record; cleaning it up removes the challenge.
struct CloudnsRecord {
    client: std::sync::Arc<CloudnsClient>,
    zone: String,
    id: u64,
}

impl DnsRecord for CloudnsRecord {
    fn cleanup(self: Box<Self>) -> Result<(), DnsError> {
        let mut p = Map::new();
        p.insert("domain-name".to_string(), json!(self.zone));
        p.insert("record-id".to_string(), json!(self.id));
        self.client.request("dns/delete-record.json", p)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    /// A mock ClouDNS API: `example.com` is a valid zone; the rest 404s. It
    /// records every request line so tests can assert on the query params.
    fn spawn_mock() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut buf = [0u8; 4096];
                let Ok(n) = stream.read(&mut buf) else {
                    continue;
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let first_line = req.lines().next().unwrap_or("").to_string();
                seen_clone.lock().unwrap().push(first_line.clone());
                let (status, body) = if first_line.contains("/dns/records.json") {
                    if first_line.contains("domain-name=example.com") {
                        (
                            "200 OK",
                            r#"{"data":[{"id":1,"type":"TXT","host":"foo","record":"bar"}]}"#,
                        )
                    } else {
                        ("404 Not Found", r#"{"message":"zone not found"}"#)
                    }
                } else if first_line.contains("/dns/add-record.json") {
                    ("200 OK", r#"{"data":{"record":{"id":12345}}}"#)
                } else if first_line.contains("/dns/delete-record.json") {
                    ("200 OK", r#"{"data":{"deleted":true}}"#)
                } else {
                    ("404 Not Found", "{}")
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{addr}/"), seen)
    }

    #[test]
    fn present_publishes_and_cleanup_deletes() {
        let (base, seen) = spawn_mock();
        let client = CloudnsClient::with_base("123".to_string(), "secret".to_string(), base);
        let rec = client.present("api.example.com", "key-auth-value").unwrap();

        let reqs: Vec<String> = seen.lock().unwrap().clone();
        let add = reqs
            .iter()
            .find(|l| l.contains("/dns/add-record.json"))
            .expect("add-record was called");
        assert!(add.contains("domain-name=example.com"));
        assert!(add.contains("host=_acme-challenge.api.example.com"));
        assert!(add.contains("record=key-auth-value"));
        assert!(add.contains("record-type=TXT"));
        assert!(add.contains("auth-id=123"));
        assert!(add.contains("auth-password=secret"));

        rec.cleanup().unwrap();
        let reqs: Vec<String> = seen.lock().unwrap().clone();
        let del = reqs
            .iter()
            .find(|l| l.contains("/dns/delete-record.json"))
            .expect("delete-record was called");
        assert!(del.contains("domain-name=example.com"));
        assert!(del.contains("record-id=12345"));
    }

    #[test]
    fn present_errors_without_a_zone() {
        let (base, _) = spawn_mock();
        let client = CloudnsClient::with_base("123".to_string(), "secret".to_string(), base);
        assert!(client.present("nope.invalid", "k").is_err());
    }
}
