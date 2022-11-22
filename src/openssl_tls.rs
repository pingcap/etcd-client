#![cfg(feature = "tls-openssl")]

use std::{sync::Arc, task::Poll};

use http::{Request, Uri};
use hyper::client::HttpConnector;
use hyper_openssl::HttpsConnector;
use openssl::{
    error::ErrorStack,
    pkey::PKey,
    ssl::{SslConnector, SslMethod},
    x509::X509,
};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    body::BoxBody,
    transport::{Channel, Endpoint},
};
use tower::{balance::p2c::Balance, buffer::Buffer, discover::Change, load::Load, Service};

use super::error::Result;

pub type SslConnectorBuilder = openssl::ssl::SslConnectorBuilder;
pub type OpenSslResult<T> = std::result::Result<T, ErrorStack>;
// Below are some type alias for make clearer types.
pub type TonicRequest = Request<BoxBody>;
pub type Buffered<T> = Buffer<T, TonicRequest>;
pub type Balanced<T> = Balance<T, TonicRequest>;
pub type OpenSslChannel = Buffered<Balanced<OpenSslDiscover<Uri>>>;
/// OpenSslDiscover is the backend for balanced channel based on OpenSSL transports.
/// Because `Channel::balance` doesn't allow us to provide custom connector, we must implement ourselves' balancer...
pub type OpenSslDiscover<K> = ReceiverStream<Result<Change<K, FairLoaded<Channel>>>>;

#[derive(Clone)]
pub struct OpenSslConnector(HttpsConnector<HttpConnector>);

impl std::fmt::Debug for OpenSslConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OpenSslConnector").finish()
    }
}

impl OpenSslConnector {
    pub fn create_default() -> OpenSslResult<Self> {
        let conf = OpenSslClientConfig::default();
        conf.build()
    }
}

#[cfg(feature = "tls")]
compile_error!(concat!(
    "**You should only enable one of `tls` and `tls-openssl`.** Reason: ",
    "For now, `tls-openssl` would take over the transport layer (sockets) to implement TLS based connection. ",
    "As a result, once using with `tonic`'s internal TLS implementation (which based on `rustls`), ", 
    "we may create TLS tunnels over TLS tunnels or directly fail because of some sorts of misconfiguration.")
);

/// `FairLoaded` is a simple wrapper over channels that provides nothing about work load.
/// (Which would lead to a complete "fair" scheduling?)
#[repr(transparent)]
pub struct FairLoaded<S>(S);

impl<S> Load for FairLoaded<S> {
    type Metric = usize;

    fn load(&self) -> Self::Metric {
        0
    }
}

impl<R, S: Service<R>> Service<R> for FairLoaded<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        self.0.call(req)
    }
}

/// Create a balanced channel using the OpenSSL config.
pub fn balanced_channel(
    connector: OpenSslConnector,
) -> Result<(OpenSslChannel, Sender<Change<Uri, Endpoint>>)> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let tls_conn = create_openssl_discover(connector, rx);
    let balance = Balance::new(tls_conn);
    // Note: the buffer should already be configured when creating the internal channels,
    // we wrap this in the buffer is just for making them `Clone`.
    let buffered = Buffer::new(balance, 1024);

    Ok((buffered, tx))
}

/// Create a connector which dials TLS connections by openssl.
fn create_openssl_connector(builder: SslConnectorBuilder) -> OpenSslResult<OpenSslConnector> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    let https = HttpsConnector::with_connector(http, builder)?;
    Ok(OpenSslConnector(https))
}

/// Create a discover which mapping Endpoints into SSL connections.
/// Because this would fully take over the transport layer by a security channel,
/// you should NOT enable `tonic/ssl` feature (or tonic may try to create SSL session over the security transport...).
fn create_openssl_discover<K: Send + 'static>(
    connector: OpenSslConnector,
    mut incoming: Receiver<Change<K, Endpoint>>,
) -> OpenSslDiscover<K> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let fut = async move {
        while let Some(x) = incoming.recv().await {
            let r = async {
                match x {
                    Change::Insert(name, e) => {
                        let chan = e.connect_with_connector(connector.clone().0).await?;
                        Ok(Change::Insert(name, FairLoaded(chan)))
                    }
                    Change::Remove(name) => Ok(Change::Remove(name)),
                }
            }
            .await;
            if tx.send(r).await.is_err() {
                return;
            }
        }
    };
    tokio::task::spawn(fut);
    ReceiverStream::new(rx)
}

/// The configuration type for a openssl connection.
/// For best flexibility, we are making it a callback over `SslConnectorBuilder`.
/// Which allows users to fine-tweaking the detail of the SSL connection.
/// This isn't `Clone` due to the implementation.
pub struct OpenSslClientConfig(OpenSslResult<SslConnectorBuilder>);

impl Default for OpenSslClientConfig {
    fn default() -> Self {
        let get_builder = || {
            let mut b = SslConnector::builder(SslMethod::tls_client())?;
            // It seems gRPC doesn't support upgrade to HTTP/2,
            // if we haven't specified the protocol by ALPN, it would return a `GONE`.
            // "h2" is the ALPN name for HTTP/2, see:
            // https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids
            b.set_alpn_protos(b"\x02h2")?;
            OpenSslResult::Ok(b)
        };
        Self(get_builder())
    }
}

impl std::fmt::Debug for OpenSslClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OpenSslClientConfig")
            .field(&"callbacks")
            .finish()
    }
}

impl OpenSslClientConfig {
    /// Manually modify the SslConnectorBuilder by a pure function.
    pub fn manually(
        mut self,
        f: impl FnOnce(&mut SslConnectorBuilder) -> OpenSslResult<()>,
    ) -> Self {
        Self(self.0.and_then(|mut b| {
            f(&mut b)?;
            Ok(b)
        }))
    }

    /// Add a CA into the cert storage via the binary of the PEM file of CA cert.
    /// If the argument is empty, do nothing.
    pub fn ca_cert_pem(self, s: &[u8]) -> Self {
        if s.is_empty() {
            return self;
        }
        self.manually(move |cb| {
            let ca = X509::from_pem(&s)?;
            cb.cert_store_mut().add_cert(ca)?;
            Ok(())
        })
    }

    /// Add a client cert for the request.
    /// If any of the argument is empty, do nothing.
    pub fn client_cert_pem_and_key(self, cert_pem: &[u8], key_pem: &[u8]) -> Self {
        if cert_pem.is_empty() || key_pem.is_empty() {
            return self;
        }
        self.manually(|cb| {
            let client = X509::from_pem(&cert_pem)?;
            let client_key = PKey::private_key_from_pem(&key_pem)?;
            cb.set_certificate(&client)?;
            cb.set_private_key(&client_key)?;
            Ok(())
        })
    }

    pub(crate) fn build(self) -> OpenSslResult<OpenSslConnector> {
        self.0.and_then(|x| create_openssl_connector(x))
    }
}
