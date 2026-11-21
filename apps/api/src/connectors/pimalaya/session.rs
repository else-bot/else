use super::*;

fn sasl(auth: &Auth) -> Sasl {
    if auth.oauth {
        SaslXoauth2Creds {
            username: auth.username.clone(),
            token: auth.secret.to_string().into(),
        }
        .into()
    } else {
        SaslPlainCreds {
            authzid: None,
            authcid: auth.username.clone(),
            passwd: auth.secret.to_string().into(),
        }
        .into()
    }
}
/// One client configuration for the process, so each connection skips
/// loading the platform's roots and can resume an earlier TLS session.
pub(super) fn tls_connector() -> Result<TlsConnector, MailError> {
    static CONFIG: std::sync::OnceLock<Arc<ClientConfig>> = std::sync::OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(TlsConnector::from(config.clone()));
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = Arc::new(ClientConfig::with_platform_verifier().map_err(failure)?);
    Ok(TlsConnector::from(CONFIG.get_or_init(|| config).clone()))
}

pub(super) enum Socket {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}
impl Socket {
    pub(super) async fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = match self {
            Self::Plain(s) => s.read(bytes).await?,
            Self::Tls(s) => s.read(bytes).await?,
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "mail connection closed",
            ));
        }
        Ok(n)
    }
    pub(super) async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.write_all(bytes).await,
            // rustls can still hold the end of the write when the socket is full.
            Self::Tls(s) => {
                s.write_all(bytes).await?;
                s.flush().await
            }
        }
    }
    /// The next chunk of a response, charged to `budget`.
    pub(super) async fn exchange(
        &mut self,
        budget: &mut usize,
        bytes: &mut [u8],
    ) -> Result<Vec<u8>, ReadFailure> {
        let n = self.read(bytes).await.map_err(ReadFailure::Io)?;
        *budget = budget.checked_sub(n).ok_or(ReadFailure::TooLarge)?;
        Ok(bytes[..n].to_vec())
    }
    async fn upgrade(self, connector: &TlsConnector, host: &str) -> Result<Self, MailError> {
        let Self::Plain(socket) = self else {
            return Err(MailError::new(MailErrorCode::MailTlsFailed));
        };
        let name = ServerName::try_from(host.to_owned()).map_err(failure)?;
        Ok(Self::Tls(Box::new(
            connector.connect(name, socket).await.map_err(|error| {
                MailError::new(MailErrorCode::MailTlsFailed).cause(format!(
                    "the secure connection failed ({})",
                    safe_text(&error)
                ))
            })?,
        )))
    }
}

pub(super) enum ReadFailure {
    Io(io::Error),
    /// The response passed `RESPONSE_LIMIT`.
    TooLarge,
}
impl From<ReadFailure> for MailError {
    fn from(failure: ReadFailure) -> Self {
        match failure {
            ReadFailure::Io(error) => io_failure(error),
            ReadFailure::TooLarge => MailError::new(MailErrorCode::MailResponseTooLarge),
        }
    }
}
impl From<ReadFailure> for io::Error {
    fn from(failure: ReadFailure) -> Self {
        match failure {
            ReadFailure::Io(error) => error,
            ReadFailure::TooLarge => {
                io::Error::new(io::ErrorKind::InvalidData, "invalid mail response")
            }
        }
    }
}

/// What an IMAP or SMTP session-open coroutine asks for next.
enum Step<T> {
    Connect { host: String, port: u16, tls: bool },
    Upgrade,
    Tag(Vec<u8>),
    Read,
    Write(Vec<u8>),
    Done(T),
}

/// Runs a session-open coroutine: `step` resumes it with the last read
/// or tag and says what it wants next.
async fn handshake<T>(
    connector: &TlsConnector,
    mut step: impl FnMut(Option<&[u8]>) -> Result<Step<T>, MailError>,
) -> Result<(Socket, T), MailError> {
    let Step::Connect { host, port, tls } = step(None)? else {
        return Err(MailError::new(MailErrorCode::MailTlsFailed));
    };
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(io_failure)?;
    let mut socket = Socket::Plain(tcp);
    if tls {
        socket = socket.upgrade(connector, &host).await?;
    }
    let mut arg: Option<Vec<u8>> = None;
    let mut bytes = [0; 16384];
    let mut budget = RESPONSE_LIMIT;
    loop {
        match step(arg.take().as_deref())? {
            Step::Connect { .. } => return Err(MailError::new(MailErrorCode::MailTlsFailed)),
            Step::Upgrade => socket = socket.upgrade(connector, &host).await?,
            Step::Tag(tag) => arg = Some(tag),
            Step::Read => arg = Some(socket.exchange(&mut budget, &mut bytes).await?),
            Step::Write(bytes) => socket.write(&bytes).await.map_err(io_failure)?,
            Step::Done(opened) => return Ok((socket, opened)),
        }
    }
}

pub(super) struct Imap {
    pub(super) socket: Socket,
    pub(super) fragmentizer: Fragmentizer,
    pub(super) tags: TagGenerator,
    pub(super) capabilities: Vec<Capability<'static>>,
}
impl Imap {
    pub(super) async fn open(endpoint: &Endpoint, auth: &Auth) -> Result<Self, MailError> {
        timeout(
            COMMAND_TIMEOUT,
            Self::connect(endpoint, auth, &tls_connector()?),
        )
        .await
        .map_err(|_| MailError::new(MailErrorCode::MailTimeout))?
    }
    pub(super) async fn connect(
        endpoint: &Endpoint,
        auth: &Auth,
        connector: &TlsConnector,
    ) -> Result<Self, MailError> {
        let transport = if endpoint.start_tls {
            ImapSessionTransport::Tcp {
                host: endpoint.host.clone(),
                port: endpoint.port,
            }
        } else {
            ImapSessionTransport::Tls {
                host: endpoint.host.clone(),
                port: endpoint.port,
            }
        };
        let mut session = ImapSessionOpen::new(
            transport,
            Some(sasl(auth)),
            ImapSessionOpenOptions {
                starttls: endpoint.start_tls,
                ..Default::default()
            },
        );
        let mut parser = Fragmentizer::new((LITERAL_LIMIT + 16384) as u32);
        let mut tags = TagGenerator::new();
        let (socket, capabilities) = handshake(connector, |arg| {
            Ok(match session.resume(&mut parser, arg) {
                ImapCoroutineState::Complete(Ok(data)) => Step::Done(data.capability),
                ImapCoroutineState::Complete(Err(_)) => {
                    return Err(MailError::new(MailErrorCode::MailAuthenticationFailed));
                }
                ImapCoroutineState::Yielded(y) => match y {
                    ImapSessionOpenYield::WantsTcpConnect { host, port } => Step::Connect {
                        host,
                        port,
                        tls: false,
                    },
                    ImapSessionOpenYield::WantsTlsConnect { host, port } => Step::Connect {
                        host,
                        port,
                        tls: true,
                    },
                    ImapSessionOpenYield::WantsTlsUpgrade => Step::Upgrade,
                    ImapSessionOpenYield::WantsUnixConnect(_) => {
                        return Err(MailError::new(MailErrorCode::MailTlsFailed));
                    }
                    ImapSessionOpenYield::WantsTag => {
                        Step::Tag(tags.generate().as_ref().as_bytes().to_vec())
                    }
                    ImapSessionOpenYield::WantsRead => Step::Read,
                    ImapSessionOpenYield::WantsWrite(bytes) => Step::Write(bytes),
                },
            })
        })
        .await?;
        Ok(Self {
            socket,
            fragmentizer: parser,
            tags,
            capabilities,
        })
    }
    pub(super) fn has(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.to_string().eq_ignore_ascii_case(name))
    }
}
impl ImapClientAsync for Imap {
    async fn run<C, T, E>(&mut self, mut coroutine: C) -> Result<T, ImapClientError>
    where
        C: ImapCoroutine<Yield = ImapYield, Return = Result<T, E>> + Send,
        T: Send,
        E: Send,
        ImapClientError: From<E>,
    {
        timeout(COMMAND_TIMEOUT, async {
            let mut arg: Option<Vec<u8>> = None;
            let mut bytes = [0; 16384];
            let mut budget = RESPONSE_LIMIT;
            loop {
                let state = coroutine.resume(&mut self.fragmentizer, arg.as_deref());
                arg = None;
                match state {
                    ImapCoroutineState::Complete(r) => return r.map_err(Into::into),
                    ImapCoroutineState::Yielded(ImapYield::WantsTag) => {
                        arg = Some(self.tags.generate().as_ref().as_bytes().to_vec())
                    }
                    ImapCoroutineState::Yielded(ImapYield::WantsWrite(bytes)) => {
                        self.socket.write(&bytes).await?
                    }
                    ImapCoroutineState::Yielded(ImapYield::WantsRead) => {
                        arg = Some(
                            self.socket
                                .exchange(&mut budget, &mut bytes)
                                .await
                                .map_err(io::Error::from)?,
                        );
                    }
                }
            }
        })
        .await
        .map_err(|_| ImapClientError::Io(io::Error::new(io::ErrorKind::TimedOut, "mail timeout")))?
    }
}

pub(super) struct Smtp {
    pub(super) socket: Socket,
}
impl Smtp {
    pub(super) async fn open(endpoint: &Endpoint, auth: &Auth) -> Result<Self, MailError> {
        timeout(
            COMMAND_TIMEOUT,
            Self::connect(endpoint, auth, &tls_connector()?),
        )
        .await
        .map_err(|_| MailError::new(MailErrorCode::MailTimeout))?
    }
    pub(super) async fn connect(
        endpoint: &Endpoint,
        auth: &Auth,
        connector: &TlsConnector,
    ) -> Result<Self, MailError> {
        let transport = if endpoint.start_tls {
            SmtpSessionTransport::Tcp {
                host: endpoint.host.clone(),
                port: endpoint.port,
            }
        } else {
            SmtpSessionTransport::Tls {
                host: endpoint.host.clone(),
                port: endpoint.port,
            }
        };
        let domain = SmtpEhloDomain::SmtpDomain(SmtpDomain(Cow::Borrowed("localhost")));
        let mut session = SmtpSessionOpen::new(
            transport,
            domain,
            Some(sasl(auth)),
            SmtpSessionOpenOptions {
                starttls: endpoint.start_tls,
            },
        );
        let (socket, ()) = handshake(connector, |arg| {
            Ok(match session.resume(arg) {
                SmtpCoroutineState::Complete(Ok(_)) => Step::Done(()),
                SmtpCoroutineState::Complete(Err(_)) => {
                    return Err(MailError::new(MailErrorCode::MailAuthenticationFailed));
                }
                SmtpCoroutineState::Yielded(y) => match y {
                    SmtpSessionOpenYield::WantsTcpConnect { host, port } => Step::Connect {
                        host,
                        port,
                        tls: false,
                    },
                    SmtpSessionOpenYield::WantsTlsConnect { host, port } => Step::Connect {
                        host,
                        port,
                        tls: true,
                    },
                    SmtpSessionOpenYield::WantsTlsUpgrade => Step::Upgrade,
                    SmtpSessionOpenYield::WantsUnixConnect(_) => {
                        return Err(MailError::new(MailErrorCode::MailTlsFailed));
                    }
                    SmtpSessionOpenYield::WantsRead => Step::Read,
                    SmtpSessionOpenYield::WantsWrite(bytes) => Step::Write(bytes),
                },
            })
        })
        .await?;
        Ok(Self { socket })
    }
}
impl Smtp {
    /// MAIL FROM, one RCPT TO per recipient, then DATA. `phase` reaches
    /// `Reply` once the whole message is written, and `Refused` when the
    /// server then refuses it.
    pub(super) async fn transmit(
        &mut self,
        from: SmtpReversePath<'static>,
        recipients: Vec<SmtpForwardPath<'static>>,
        message: Vec<u8>,
        phase: &mut SendPhase,
    ) -> Result<(), SmtpClientError> {
        *phase = SendPhase::Envelope;
        self.mail(from, Vec::new()).await?;
        for recipient in recipients {
            self.rcpt(recipient, Vec::new()).await?;
        }
        *phase = SendPhase::Data;
        // SmtpData writes its command, then the whole message in one write.
        // The phase moves as that write completes, so a caller that times
        // this out still sees it.
        let mut writes = 0;
        let result = self
            .pump(SmtpData::new(message), || {
                writes += 1;
                if writes == 2 {
                    *phase = SendPhase::Reply;
                }
            })
            .await;
        if let Err(SmtpClientError::Data(SmtpDataError::BodyRejected { .. })) = &result {
            *phase = SendPhase::Refused;
        }
        result
    }

    async fn pump<C, T, E>(
        &mut self,
        mut coroutine: C,
        mut written: impl FnMut() + Send,
    ) -> Result<T, SmtpClientError>
    where
        C: SmtpCoroutine<Yield = SmtpYield, Return = Result<T, E>> + Send,
        T: Send,
        E: Send,
        SmtpClientError: From<E>,
    {
        timeout(COMMAND_TIMEOUT, async {
            let mut arg: Option<Vec<u8>> = None;
            let mut bytes = [0; 16384];
            let mut budget = RESPONSE_LIMIT;
            loop {
                let state = coroutine.resume(arg.as_deref());
                arg = None;
                match state {
                    SmtpCoroutineState::Complete(r) => return r.map_err(Into::into),
                    SmtpCoroutineState::Yielded(SmtpYield::WantsWrite(bytes)) => {
                        self.socket.write(&bytes).await?;
                        written();
                    }
                    SmtpCoroutineState::Yielded(SmtpYield::WantsRead) => {
                        arg = Some(
                            self.socket
                                .exchange(&mut budget, &mut bytes)
                                .await
                                .map_err(io::Error::from)?,
                        );
                    }
                }
            }
        })
        .await
        .map_err(|_| SmtpClientError::Io(io::Error::new(io::ErrorKind::TimedOut, "mail timeout")))?
    }
}
impl SmtpClientAsync for Smtp {
    async fn run<C, T, E>(&mut self, coroutine: C) -> Result<T, SmtpClientError>
    where
        C: SmtpCoroutine<Yield = SmtpYield, Return = Result<T, E>> + Send,
        T: Send,
        E: Send,
        SmtpClientError: From<E>,
    {
        self.pump(coroutine, || {}).await
    }
}
