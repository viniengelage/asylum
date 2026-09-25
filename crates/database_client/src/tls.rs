use futures::future::BoxFuture;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, InvalidDnsNameError, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};

/// The `sslmode` values libpq understands. tokio-postgres only knows the first three, so the
/// certificate checks of the `verify-*` modes happen in the rustls verifier instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "disable" => Some(Self::Disable),
            // libpq's `allow` only differs from `prefer` in which attempt comes first.
            "allow" | "prefer" => Some(Self::Prefer),
            "require" => Some(Self::Require),
            "verify-ca" => Some(Self::VerifyCa),
            "verify-full" => Some(Self::VerifyFull),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    pub(crate) fn negotiation(self) -> tokio_postgres::config::SslMode {
        match self {
            Self::Disable => tokio_postgres::config::SslMode::Disable,
            Self::Prefer => tokio_postgres::config::SslMode::Prefer,
            Self::Require | Self::VerifyCa | Self::VerifyFull => {
                tokio_postgres::config::SslMode::Require
            }
        }
    }
}

/// Wraps the socket tokio-postgres opens in a rustls session configured for one `sslmode`.
#[derive(Clone)]
pub struct RustlsConnect {
    config: Arc<ClientConfig>,
}

impl RustlsConnect {
    pub fn new(mode: SslMode) -> anyhow::Result<Self> {
        // Also installs the process-wide crypto provider the other configs are built from.
        let platform = http_client_tls::tls_config();
        let provider = platform.crypto_provider().clone();
        let config = match mode {
            SslMode::VerifyFull => platform,
            SslMode::VerifyCa => with_verifier(
                provider.clone(),
                Arc::new(IgnoreHostname {
                    inner: rustls_platform_verifier::Verifier::new(provider)?,
                }),
            )?,
            // Like libpq, `require` encrypts without checking who is on the other end.
            SslMode::Disable | SslMode::Prefer | SslMode::Require => with_verifier(
                provider.clone(),
                Arc::new(AcceptAnyCertificate { provider }),
            )?,
        };
        Ok(Self {
            config: Arc::new(config),
        })
    }
}

fn with_verifier(
    provider: Arc<CryptoProvider>,
    verifier: Arc<dyn ServerCertVerifier>,
) -> anyhow::Result<ClientConfig> {
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

impl<S> MakeTlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsTlsConnect;
    type Error = InvalidDnsNameError;

    fn make_tls_connect(&mut self, domain: &str) -> Result<Self::TlsConnect, Self::Error> {
        Ok(RustlsTlsConnect {
            connector: tokio_rustls::TlsConnector::from(self.config.clone()),
            server_name: ServerName::try_from(domain.to_owned())?,
        })
    }
}

pub struct RustlsTlsConnect {
    connector: tokio_rustls::TlsConnector,
    server_name: ServerName<'static>,
}

impl<S> TlsConnect<S> for RustlsTlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<RustlsStream<S>>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let stream = self.connector.connect(self.server_name, stream).await?;
            Ok(RustlsStream(stream))
        })
    }
}

pub struct RustlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S> TlsStream for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn channel_binding(&self) -> ChannelBinding {
        // Without the server certificate hash tokio-postgres authenticates with plain
        // SCRAM-SHA-256 instead of SCRAM-SHA-256-PLUS, which every server accepts unless it
        // was set up with `channel_binding=require`.
        ChannelBinding::none()
    }
}

impl<S> AsyncRead for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

#[derive(Debug)]
struct AcceptAnyCertificate {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `verify-ca`: the chain must lead to a trusted root, but the name on the certificate may differ
/// from the host we dialed (common behind load balancers and SSH tunnels).
#[derive(Debug)]
struct IgnoreHostname {
    inner: rustls_platform_verifier::Verifier,
}

impl ServerCertVerifier for IgnoreHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            result => result,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}
