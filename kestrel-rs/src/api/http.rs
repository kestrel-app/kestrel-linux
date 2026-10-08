//! One HTTP path for every vendor.
//!
//! Reolink's client grew its own request handling because it was the only
//! thing here. Vendors differ mostly in *how a request is authenticated*, and
//! that difference is carried in headers or query parameters rather than in
//! the shape of the call:
//!
//!   * Reolink puts its token in the query string
//!   * ZoneMinder puts its token in a URL parameter
//!   * QNAP puts a session id in a URL parameter
//!   * UniFi needs a cookie *and* a CSRF header
//!   * Frigate has no authentication at all by default
//!
//! So the shared layer takes headers, and each vendor decides what to put in
//! them. Nothing here knows what a camera is.

use std::time::Duration;

use super::error::{Error, Result};

/// A response, kept deliberately small: status, body, and the headers a vendor
/// needs to read back (UniFi's session cookie, for one).
pub struct Response {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Case-insensitive header lookup, because servers disagree on casing.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_str(&self.body)
            .map_err(|err| Error::Protocol(format!("malformed response: {err}")))
    }
}

/// Build an agent with the timeouts a camera system needs.
///
/// Connect and read budgets are separate for the same reason the Reolink
/// client separates them: an unreachable host should fail fast, while a
/// reachable-but-busy one legitimately needs a long window.
///
/// `allow_self_signed` relaxes certificate checking, and is off unless the
/// device has been configured for it. See [`accept_any_certificate`].
pub fn agent(read_timeout: u64, allow_self_signed: bool) -> ureq::Agent {
    let builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(read_timeout));

    if allow_self_signed {
        builder.tls_config(accept_any_certificate()).build()
    } else {
        builder.build()
    }
}

/// An agent for a caller that makes many small requests to one host, and wants
/// to keep the connections rather than pay for them again.
///
/// The default keeps a single idle connection per host, which is right for a
/// camera polled once a second and wrong for the tile pool: six threads
/// fetching hundreds of 256-pixel squares from one service would hand five of
/// every six connections back and open a fresh TCP and TLS session for the next
/// tile. Against a public map service that handshake costs more than the tile
/// does, and it is most of the time a map takes to appear.
pub fn pooled_agent(read_timeout: u64, idle_per_host: usize) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(read_timeout))
        .max_idle_connections_per_host(idle_per_host)
        .build()
}

/// A TLS configuration that does not verify the server's certificate.
///
/// Cameras and NVRs ship self-signed certificates generated at the factory, so
/// the default check — validate against the public CA roots — can never pass.
/// Measured against an RLN36: it presents an X.509 **v1** certificate whose
/// subject is `CN=CERTIFICATE`, which rustls rejects outright with
/// `UnsupportedCertVersion` rather than merely distrusting. Adding it as a root
/// would not help; nothing about it is well-formed enough to verify, and it
/// carries no name matching the address it is reached at.
///
/// So verification is skipped entirely for devices the user has said to trust.
/// That is worth being clear-eyed about: it gives encryption without
/// authentication, so it does not defend against someone able to intercept
/// traffic on the local network. It is strictly better than the alternative it
/// replaces, which is plaintext HTTP carrying the password — but it is not the
/// same as verified TLS, which is why it is off by default and per device
/// rather than a global switch.
pub(crate) fn accept_any_certificate() -> std::sync::Arc<rustls::ClientConfig> {
    #[derive(Debug)]
    struct AcceptAny(rustls::crypto::CryptoProvider);

    impl rustls::client::danger::ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        // The handshake signature cannot be checked either, and this is the
        // reason why: verifying it means extracting the public key, which means
        // parsing the certificate, which is precisely what fails. An RLN36's
        // certificate is X.509 v1 and rustls will not parse a certificate older
        // than v3 by any route. Delegating to `rustls::crypto::verify_tls12_
        // signature` here reproduces the original `UnsupportedCertVersion`
        // failure exactly — measured, not assumed.
        //
        // So a trusted device gets an encrypted connection with no
        // authentication at all: an interceptor could present any certificate
        // and any signature. That is the honest cost of reaching hardware that
        // ships a certificate no modern TLS stack will look at, and it is why
        // this is opt-in per device.
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(provider.clone().into())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAny(provider)))
        .with_no_client_auth();
    std::sync::Arc::new(config)
}

/// The scheme and authority a vendor's paths hang off.
pub fn base_url(host: &str, port: u16, https: bool) -> String {
    let scheme = if https { "https" } else { "http" };
    format!("{scheme}://{host}:{port}")
}

/// Perform a request.
///
/// `body` is sent as-is when present; the caller sets Content-Type through
/// `headers` because vendors disagree (JSON for UniFi, form encoding for
/// ZoneMinder).
pub fn request(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    body: Option<&str>,
    headers: &[(String, String)],
) -> Result<Response> {
    let mut req = agent.request(method, url);
    for (name, value) in headers {
        req = req.set(name, value);
    }

    let outcome = match body {
        Some(payload) => req.send_string(payload),
        None => req.call(),
    };

    // A 4xx or 5xx is a response, not a transport failure: vendors put useful
    // detail in the body, and some use status codes as protocol (UniFi answers
    // 499 when an account needs two-factor). Only a genuine transport error is
    // raised here.
    let response = match outcome {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(err) => return Err(Error::connection(err.to_string())),
    };

    let status = response.status();
    let headers = response
        .headers_names()
        .into_iter()
        .filter_map(|name| {
            response
                .header(&name)
                .map(|value| (name.clone(), value.to_string()))
        })
        .collect();
    let body = response.into_string().unwrap_or_default();

    Ok(Response {
        status,
        body,
        headers,
    })
}

/// Perform a request whose reply is not text.
///
/// [`request`] reads the body through `into_string`, which is right for every
/// vendor's JSON and destroys a PNG: invalid UTF-8 comes back as replacement
/// characters, so the bytes that reach the caller are not the bytes the server
/// sent. The radar layers are images, so they come through here instead.
///
/// Capped rather than read unbounded — this points at hosts outside the local
/// network, and a map service having a bad day should not be able to hand the
/// process an arbitrarily large allocation. A radar sweep is tens of kilobytes;
/// a basemap at full window size is a few hundred.
pub fn request_bytes(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<(u16, Vec<u8>)> {
    let mut req = agent.get(url);
    for (name, value) in headers {
        req = req.set(name, value);
    }

    let response = match req.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(_, response)) => response,
        Err(err) => return Err(Error::connection(err.to_string())),
    };

    let status = response.status();
    let mut body = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(response.into_reader(), limit as u64),
        &mut body,
    )
    .map_err(|err| Error::connection(err.to_string()))?;

    Ok((status, body))
}

/// Percent-encode a path or query component.
///
/// Camera identifiers are not always tidy: Frigate names them, QNAP and UniFi
/// use GUIDs, and a name can contain a space.
pub fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Turn a failed response into something worth showing a person.
pub fn describe(response: &Response, what: &str) -> Error {
    match response.status {
        401 | 403 => Error::auth(format!("{what}: not authorised — check the username and password")),
        404 => Error::Protocol(format!(
            "{what}: not found — check the address, and that this is the service you think it is"
        )),
        status => Error::Protocol(format!("{what}: HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_ignores_case() {
        let response = Response {
            status: 200,
            body: String::new(),
            headers: vec![("Set-Cookie".into(), "TOKEN=abc".into())],
        };
        assert_eq!(response.header("set-cookie"), Some("TOKEN=abc"));
        assert_eq!(response.header("SET-COOKIE"), Some("TOKEN=abc"));
        assert_eq!(response.header("nothing"), None);
    }

    #[test]
    fn status_ranges() {
        let make = |status| Response {
            status,
            body: String::new(),
            headers: Vec::new(),
        };
        assert!(make(200).ok());
        assert!(make(204).ok());
        assert!(!make(401).ok());
        assert!(!make(500).ok());
    }

    /// The whole point of the setting, against a device that actually presents
    /// a self-signed certificate. Ignored by default; run with the address:
    ///   KESTREL_TEST_HTTPS=192.0.2.242 cargo test -- --ignored self_signed
    #[test]
    #[ignore]
    fn self_signed_certificates_need_permission() {
        let Ok(host) = std::env::var("KESTREL_TEST_HTTPS") else {
            eprintln!("KESTREL_TEST_HTTPS not set");
            return;
        };
        let url = format!("https://{host}/");

        // Off: the default check cannot pass, and must not.
        let strict = request(&agent(8, false), "GET", &url, None, &[]);
        match &strict {
            Ok(response) => println!("  strict:  ACCEPTED HTTP {}", response.status),
            Err(err) => println!("  strict:  REJECTED {err}"),
        }
        assert!(strict.is_err(), "an unverifiable certificate must be refused");

        // On: the device is reachable.
        let relaxed = request(&agent(8, true), "GET", &url, None, &[]);
        match &relaxed {
            Ok(response) => println!("  trusted: ACCEPTED HTTP {}", response.status),
            Err(err) => println!("  trusted: REJECTED {err}"),
        }
        assert!(
            relaxed.is_ok(),
            "with permission given, the device should answer"
        );
    }

    #[test]
    fn identifiers_survive_encoding() {
        // Frigate names cameras; a name with a space must not break the URL.
        assert_eq!(encode("front door"), "front%20door");
        assert_eq!(encode("a1b2-c3"), "a1b2%2Dc3");
    }
}
