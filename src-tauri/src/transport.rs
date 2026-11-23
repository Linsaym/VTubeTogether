use std::{
    fs,
    io::{ErrorKind, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use quinn::{
    ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig,
};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use rustls::{
    ClientConfig as TlsConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{
        WebPkiSupportedAlgorithms, ring::default_provider, verify_tls12_signature,
        verify_tls13_signature,
    },
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use socket2::{Domain, Protocol, Socket, Type};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

const SERVER_NAME: &str = "vtubetogether.local";
const MAGIC: &[u8; 4] = b"VTT1";
const AUTH_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Invite {
    fingerprint: [u8; 32],
    secret: [u8; 32],
}

impl Invite {
    pub fn parse(code: &str) -> Result<Self> {
        let invalid = "Проверьте код подключения приёмника";
        let code = code.trim().strip_prefix("vtt1_").context(invalid)?;
        ensure!(code.len() == 86, "{invalid}");
        let mut bytes = [0; 64];
        let len = URL_SAFE_NO_PAD
            .decode_slice(code, &mut bytes)
            .context(invalid)?;
        ensure!(len == bytes.len(), "{invalid}");
        Ok(Self {
            fingerprint: bytes[..32].try_into()?,
            secret: bytes[32..].try_into()?,
        })
    }

    fn encode(self) -> String {
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(&self.fingerprint);
        bytes[32..].copy_from_slice(&self.secret);
        format!("vtt1_{}", URL_SAFE_NO_PAD.encode(bytes))
    }
}

pub struct Identity {
    config: ServerConfig,
    invite: Invite,
}

impl Identity {
    pub fn load(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("receiver-identity.v1");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let bytes = Self::generate()?;
                let mut file = tempfile::NamedTempFile::new_in(directory)?;
                file.write_all(&bytes)?;
                file.as_file().sync_all()?;
                match file.persist_noclobber(&path) {
                    Ok(_) => bytes,
                    Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
                        fs::read(&path)?
                    }
                    Err(error) => return Err(error.error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        Self::from_bytes(bytes).context("Не удалось загрузить ключ приёмника")
    }

    fn generate() -> Result<Vec<u8>> {
        let cert = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()])?;
        let key = cert.signing_key.serialize_der();
        let mut secret = [0; 32];
        SystemRandom::new()
            .fill(&mut secret)
            .map_err(|_| anyhow::anyhow!("Не удалось создать код подключения"))?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&secret);
        bytes.extend_from_slice(&u16::try_from(key.len())?.to_be_bytes());
        bytes.extend_from_slice(&key);
        bytes.extend_from_slice(cert.cert.der());
        Ok(bytes)
    }

    fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        ensure!(bytes.len() >= 34, "Повреждён файл ключа");
        let secret = bytes[..32].try_into()?;
        let key_len = u16::from_be_bytes(bytes[32..34].try_into()?) as usize;
        let cert_start = 34 + key_len;
        ensure!(cert_start < bytes.len(), "Повреждён файл ключа");
        let key = PrivatePkcs8KeyDer::from(bytes[34..cert_start].to_vec());
        let der = CertificateDer::from(bytes[cert_start..].to_vec());
        let invite = Invite {
            fingerprint: fingerprint(&der),
            secret,
        };
        let mut config = ServerConfig::with_single_cert(vec![der], key.into())?;
        config.transport_config(transport_config(true));
        Ok(Self { config, invite })
    }

    pub fn code(&self) -> String {
        self.invite.encode()
    }

    pub fn bind(&self, port: u16) -> Result<Endpoint> {
        let socket = dual_stack(port)
            .or_else(|_| std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)))
            .with_context(|| format!("Не удалось открыть порт UDP {port}"))?;
        Ok(Endpoint::new(
            Default::default(),
            Some(self.config.clone()),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?)
    }

    pub async fn authenticate(&self, conn: &Connection) -> Result<SendStream> {
        timeout(AUTH_TIMEOUT, async {
            let (reply, mut request) = conn.accept_bi().await?;
            let mut bytes = [0; 36];
            request.read_exact(&mut bytes).await?;
            ensure!(
                &bytes[..4] == MAGIC && bool::from(bytes[4..].ct_eq(&self.invite.secret)),
                "Неверный код подключения"
            );
            Ok(reply)
        })
        .await
        .context("Истекло время проверки кода")?
    }
}

fn dual_stack(port: u16) -> std::io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(false)?;
    socket.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
    Ok(socket.into())
}

fn transport_config(server: bool) -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    config
        .max_idle_timeout(Some(quinn::VarInt::from_u32(6_000).into()))
        .keep_alive_interval(Some(Duration::from_secs(1)))
        .max_concurrent_bidi_streams(u32::from(server).into())
        .max_concurrent_uni_streams(u32::from(server).into())
        .datagram_receive_buffer_size(None)
        .stream_receive_window(quinn::VarInt::from_u32(384 * 1024))
        .receive_window(quinn::VarInt::from_u32(512 * 1024))
        .send_window(256 * 1024);
    Arc::new(config)
}

fn fingerprint(cert: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, cert)
        .as_ref()
        .try_into()
        .expect("SHA-256 length")
}

#[derive(Debug)]
struct PinnedServer {
    fingerprint: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(msg, cert, sig, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(msg, cert, sig, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

pub fn client(ip: IpAddr, invite: Invite) -> Result<Endpoint> {
    let local = if ip.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    let mut endpoint = Endpoint::client(SocketAddr::new(local, 0))?;
    let provider = default_provider();
    let verifier = PinnedServer {
        fingerprint: invite.fingerprint,
        algorithms: provider.signature_verification_algorithms,
    };
    let tls = TlsConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let mut config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
    ));
    config.transport_config(transport_config(false));
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

pub async fn connect(endpoint: &Endpoint, addr: SocketAddr, invite: Invite) -> Result<Connection> {
    let conn = timeout(AUTH_TIMEOUT, endpoint.connect(addr, SERVER_NAME)?)
        .await
        .context("Приёмник не ответил")?
        .context("Не удалось установить QUIC-соединение. Проверьте адрес, UDP-порт и код")?;
    let auth = timeout(AUTH_TIMEOUT, async {
        let (mut request, mut reply) = conn.open_bi().await?;
        request.write_all(MAGIC).await?;
        request.write_all(&invite.secret).await?;
        request.finish()?;
        ensure!(reply.read_u8().await? == 1, "Приёмник отклонил подключение");
        Ok(())
    })
    .await;
    match auth {
        Ok(Ok(())) => Ok(conn),
        failure => {
            conn.close(1u32.into(), b"authentication failed");
            failure.context("Приёмник не подтвердил код подключения")??;
            bail!("Приёмник отклонил подключение")
        }
    }
}

pub struct VideoFrame {
    pub key: bool,
    pub data: Vec<u8>,
}

pub async fn write_frame(stream: &mut SendStream, frame: &VideoFrame) -> Result<()> {
    ensure!(
        !frame.data.is_empty() && frame.data.len() <= MAX_FRAME,
        "Недопустимый размер кадра"
    );
    timeout(Duration::from_millis(700), async {
        stream.write_u32(frame.data.len() as u32).await?;
        stream.write_u8(u8::from(frame.key)).await?;
        stream.write_all(&frame.data).await?;
        Ok(())
    })
    .await
    .context("Сеть не успевает передавать видео")?
}

pub async fn read_frame(stream: &mut RecvStream) -> Result<VideoFrame> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        (1..=MAX_FRAME).contains(&length),
        "Недопустимый размер кадра"
    );
    let key = match stream.read_u8().await? {
        0 => false,
        1 => true,
        _ => bail!("Недопустимый заголовок кадра"),
    };
    let mut data = vec![0; length];
    stream.read_exact(&mut data).await?;
    Ok(VideoFrame { key, data })
}
