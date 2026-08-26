# Pingora 0.8.1 capability research

Scope: feasibility against this repository’s locked Pingora base, not against
current `main`. `Cargo.toml` requests Pingora 0.8 and the lockfile resolves the
Pingora crates to 0.8.1 ([manifest](https://github.com/chulingera2025/raddy/blob/24e2bbf9a5174d337924d034518e162a34186c65/Cargo.toml#L33),
[lockfile](https://github.com/chulingera2025/raddy/blob/24e2bbf9a5174d337924d034518e162a34186c65/Cargo.lock#L1728-L1742)).

## Raddy v0.3.5 implementation status

The application-level items in this report are now implemented and covered by
focused tests: upstream `h2://` and prior-knowledge `h2c://`, multi-domain
site blocks, IPv4/IPv6 HTTP listeners and site Host parsing, exact-plus
one-label wildcard matching for HTTP/TLS/L4 SNI, and static/internal TLS
termination for raw TCP. TLS-ALPN-01 is implemented with an OpenSSL ALPN
selector, a ClientHello marker, and a temporary RFC 8737 certificate.

Transparent TCP remains a Linux integration rather than a Pingora-native
feature: raddy supplies the `IP_TRANSPARENT` listener and source-bound
connector, then reuses Pingora’s `ServerApp` and socket digest. UDP upgrade is
implemented as an raddy-specific handoff protocol that transfers the listener,
connected flow sockets, and bounded metadata; it is not supplied by Pingora.
QUIC/HTTP/3 termination remains outside Pingora 0.8.1. The existing UDP proxy
can provide datagram passthrough, while a terminating HTTP/3 deployment needs a
separate QUIC service or sidecar.

Verdicts use three boundaries: **native** means Pingora 0.8.1 exposes the
needed transport/protocol primitive; **application** means it can be built
above Pingora without changing Pingora; **custom integration** means it needs
a separate protocol stack, Linux networking setup, or a Pingora fork for
first-class support. The implementation-status section above records what
raddy has since built on those boundaries; the table remains the capability
analysis of the locked Pingora base.

| Feature | Verdict | Feasibility on this base |
| --- | --- | --- |
| TLS-ALPN-01 | **Application with a low-level TLS hook** | Feasible on the OpenSSL backend, but not as a simple use of Pingora’s high-level `enable_h2()` helper. The implementation needs a mixed ALPN selector (`acme-tls/1` plus normal H2/H1), a way to identify the ACME ClientHello before certificate selection, and the special ACME certificate extensions. RFC 8737 requires TCP/443, the validated SNI, the single `acme-tls/1` ALPN value, and the ACME certificate extensions ([RFC 8737 §3](https://www.rfc-editor.org/rfc/rfc8737.html#section-3)). Pingora exposes resumable certificate callbacks and custom ALPN settings ([TLS accept API](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/mod.rs#L44-L89), [TLS settings](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/tls/boringssl_openssl/mod.rs#L90-L130)), while the underlying OpenSSL builder exposes ClientHello and ALPN callbacks ([ClientHello callback](https://docs.rs/openssl/0.10.81/openssl/ssl/struct.SslAcceptorBuilder.html#method.set_client_hello_callback), [ALPN callback](https://docs.rs/openssl/0.10.81/openssl/ssl/struct.SslAcceptorBuilder.html#method.set_alpn_select_callback)). Pingora’s `TlsAccept` trait itself only exposes certificate and post-handshake callbacks, so a low-level application wrapper may work without a source fork, but a small Pingora API extension/fork is the maintainable option. Do not rely on `selected_alpn_protocol()` being populated at certificate-callback time without testing the exact handshake ordering. |
| Upstream HTTP/2 | **Native + application** | Pingora’s proxy supports HTTP/1.x and HTTP/2, and its upstream connector chooses H2 when `PeerOptions` permits it ([proxy features](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-proxy/src/lib.rs#L17-L34), [connector selection](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/connectors/http/mod.rs#L61-L130)). Raddy now exposes `h2://` and sets `peer.options.set_http_version(2, 2)` for those peers; the path is an application change, not a fork. |
| Upstream h2c | **Native + application** | Pingora’s H2 connector explicitly supports plaintext prior-knowledge H2: when no ALPN exists and the peer requires HTTP/2, it proceeds to the H2 handshake and trusts the caller that the server speaks h2c ([connector](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/connectors/http/v2.rs#L251-L294)). RFC 9113 says the old HTTP/1.1 `Upgrade: h2c` mechanism is obsolete; use the cleartext connection preface instead ([RFC 9113 §§3.1, 3.3](https://www.rfc-editor.org/rfc/rfc9113.html#section-3.1)). Raddy now exposes `h2c://` and sets the minimum HTTP version to 2. |
| Multi-domain site blocks | **Application** | Pingora has no site-block configuration abstraction; its programmable HTTP proxy is the seam. Raddy now expands comma-separated site keys into independently addressable sites and serves them through one shared HTTP listener. |
| HTTP site IPv6 | **Native primitive + application** | Pingora’s listener accepts IPv4 or IPv6 TCP address strings and has an `ipv6_only` socket option ([listener address/bind](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/l4.rs#L53-L103), [IPv4/IPv6 bind](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/l4.rs#L223-L276)). Raddy now binds HTTP/TLS listeners on `[::]:<port>` with explicit dual-stack behavior and accepts bracketed IPv6 site Host values. |
| Wildcard SNI | **Application** | SNI itself carries the concrete hostname, not a wildcard ([RFC 6066 §3](https://www.rfc-editor.org/rfc/rfc6066.html#section-3)). Raddy now applies exact-first, longest-suffix, one-label wildcard lookup for certificates, HTTP sites, and L4 SNI routes; `*.example.com` matches `a.example.com`, not the apex or `a.b.example.com` ([RFC 6125 §6.4.3](https://www.rfc-editor.org/rfc/rfc6125.html#section-6.4.3)). |
| TLS termination for L4 | **Application** | Pingora’s listening service applies TLS before handing the resulting byte stream to `ServerApp` ([accept/handshake path](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/services/listening.rs#L184-L233)); `TlsSettings` supports callbacks and custom ALPN ([TLS settings](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/tls/boringssl_openssl/mod.rs#L90-L148)). Raddy uses `add_tls_with_settings` plus a static-certificate callback for `tls internal` and static L4 TCP certificates, then reuses the raw relay on the decrypted stream. |
| Transparent proxying | **Custom integration** | Pingora exposes Linux original-destination lookup through `SocketDigest::original_dst()`/`SO_ORIGINAL_DST` ([socket digest](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/protocols/digest.rs#L64-L76), [original-destination lookup](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/protocols/l4/ext.rs#L420-L446)). Raddy now supplies a privileged Linux transparent listener and source-bound connector around Pingora’s `ServerApp`; it still requires `IP_TRANSPARENT`, netfilter TPROXY rules, policy routing, and `CAP_NET_ADMIN` ([kernel TPROXY guide](https://www.kernel.org/doc/html/latest/networking/tproxy.html#transparent-proxy-support)). |
| QUIC / HTTP/3 | **Custom integration** | Pingora 0.8.1’s workspace and core listener API contain TCP/UDS listeners and H1/H2/TLS components, with no QUIC transport or UDP listener ([workspace](https://github.com/cloudflare/pingora/blob/0.8.1/Cargo.toml#L6-L29), [server address](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/l4.rs#L53-L59)). HTTP/3 is HTTP semantics mapped onto QUIC and uses UDP endpoints ([RFC 9114 §§1.2, 3.1](https://www.rfc-editor.org/rfc/rfc9114.html#section-1.2)). A separate QUIC/HTTP/3 service can run beside Pingora, but Pingora-integrated QUIC requires a new transport/service layer or fork. |
| Lossless UDP zero-downtime upgrade | **Custom integration; not native** | Pingora’s upgrade mechanism transfers listening FDs, but its standard listener reconstruction supports only TCP and UDS ([FD transfer](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/server/transfer_fd/mod.rs#L31-L82), [listener reconstruction](https://github.com/cloudflare/pingora/blob/0.8.1/pingora-core/src/listeners/l4.rs#L198-L220)). Raddy adds a separate Linux handoff that transfers the UDP listener, connected upstream flow sockets, and bounded flow metadata through SCM_RIGHTS chunks. The receive queue remains attached to the transferred fd; the guarantee is application-owned, not supplied by Pingora. |

## Practical conclusion

The low-risk application work is upstream H2/h2c configuration, IPv6 binds,
wildcard matching, and TLS termination on L4. TLS-ALPN-01 is feasible through
the OpenSSL builder callbacks, but remains outside Pingora’s high-level
callback contract and should stay isolated behind tests. Transparent proxying
and UDP lossless upgrade are also feasible as raddy-owned Linux integrations,
with explicit privilege and handoff protocols. QUIC/HTTP-3 termination is the
remaining separate transport boundary.

## Recommended order for raddy

1. Upstream HTTP/2, then prior-knowledge h2c if a real deployment requires it.
2. HTTP IPv6 listener/upstream support with explicit dual-stack conflict rules.
3. Multi-domain blocks and wildcard SNI matching, with wildcard ACME limited to
   DNS-01.
4. L4 TLS termination using Pingora’s existing TLS listener and `ServerApp`
   seams.
5. TLS-ALPN-01 as an isolated OpenSSL callback path with strict challenge
   cleanup and port-443 validation.
6. Transparent proxying and lossless UDP upgrade as separate Linux/kernel
   integrations with explicit capability and rollback checks.
7. QUIC/HTTP-3 as a separate protocol service or sidecar; do not treat UDP
   datagram passthrough as HTTP/3 termination.
