use super::*;

fn protocol_io() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid mail response")
}
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
    async fn upgrade(self, connector: &TlsConnector, host: &str) -> Result<Self, MailError> {
        let Self::Plain(socket) = self else {
            return Err(MailError::new("mail_tls_failed"));
        };
        let name = ServerName::try_from(host.to_owned()).map_err(failure)?;
        Ok(Self::Tls(Box::new(
            connector.connect(name, socket).await.map_err(|error| {
                MailError::new("mail_tls_failed").cause(format!(
                    "the secure connection failed ({})",
                    safe_text(&error)
                ))
            })?,
        )))
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
        .map_err(|_| MailError::new("mail_timeout"))?
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
        let mut socket: Option<Socket> = None;
        let mut arg: Option<Vec<u8>> = None;
        let mut bytes = [0; 16384];
        let mut budget = RESPONSE_LIMIT;
        loop {
            let state = session.resume(&mut parser, arg.as_deref());
            arg = None;
            match state {
                ImapCoroutineState::Complete(Ok(data)) => {
                    return Ok(Self {
                        socket: socket.ok_or_else(|| MailError::new("mail_tls_failed"))?,
                        fragmentizer: parser,
                        tags,
                        capabilities: data.capability,
                    });
                }
                ImapCoroutineState::Complete(Err(_)) => {
                    return Err(MailError::new(AUTHENTICATION_FAILED));
                }
                ImapCoroutineState::Yielded(y) => match y {
                    ImapSessionOpenYield::WantsTcpConnect { host, port } => {
                        socket = Some(Socket::Plain(
                            TcpStream::connect((host.as_str(), port))
                                .await
                                .map_err(failure)?,
                        ));
                    }
                    ImapSessionOpenYield::WantsTlsConnect { host, port } => {
                        socket = Some(
                            Socket::Plain(
                                TcpStream::connect((host.as_str(), port))
                                    .await
                                    .map_err(failure)?,
                            )
                            .upgrade(connector, &host)
                            .await?,
                        );
                    }
                    ImapSessionOpenYield::WantsTlsUpgrade => {
                        socket = Some(
                            socket
                                .take()
                                .ok_or_else(|| MailError::new("mail_tls_failed"))?
                                .upgrade(connector, &endpoint.host)
                                .await?,
                        );
                    }
                    ImapSessionOpenYield::WantsUnixConnect(_) => {
                        return Err(MailError::new("mail_tls_failed"));
                    }
                    ImapSessionOpenYield::WantsTag => {
                        arg = Some(tags.generate().as_ref().as_bytes().to_vec())
                    }
                    ImapSessionOpenYield::WantsRead => {
                        let n = socket
                            .as_mut()
                            .ok_or_else(|| MailError::new("mail_tls_failed"))?
                            .read(&mut bytes)
                            .await
                            .map_err(failure)?;
                        budget = budget
                            .checked_sub(n)
                            .ok_or_else(|| MailError::new("mail_response_too_large"))?;
                        arg = Some(bytes[..n].to_vec());
                    }
                    ImapSessionOpenYield::WantsWrite(bytes) => socket
                        .as_mut()
                        .ok_or_else(|| MailError::new("mail_tls_failed"))?
                        .write(&bytes)
                        .await
                        .map_err(failure)?,
                },
            }
        }
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
                        let n = self.socket.read(&mut bytes).await?;
                        budget = budget.checked_sub(n).ok_or_else(protocol_io)?;
                        arg = Some(bytes[..n].to_vec());
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
        .map_err(|_| MailError::new("mail_timeout"))?
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
        let mut socket: Option<Socket> = None;
        let mut arg: Option<Vec<u8>> = None;
        let mut bytes = [0; 16384];
        let mut budget = RESPONSE_LIMIT;
        loop {
            let state = session.resume(arg.as_deref());
            arg = None;
            match state {
                SmtpCoroutineState::Complete(Ok(_)) => {
                    return Ok(Self {
                        socket: socket.ok_or_else(|| MailError::new("mail_tls_failed"))?,
                    });
                }
                SmtpCoroutineState::Complete(Err(_)) => {
                    return Err(MailError::new(AUTHENTICATION_FAILED));
                }
                SmtpCoroutineState::Yielded(y) => match y {
                    SmtpSessionOpenYield::WantsTcpConnect { host, port } => {
                        socket = Some(Socket::Plain(
                            TcpStream::connect((host.as_str(), port))
                                .await
                                .map_err(io_failure)?,
                        ))
                    }
                    SmtpSessionOpenYield::WantsTlsConnect { host, port } => {
                        socket = Some(
                            Socket::Plain(
                                TcpStream::connect((host.as_str(), port))
                                    .await
                                    .map_err(io_failure)?,
                            )
                            .upgrade(connector, &host)
                            .await?,
                        )
                    }
                    SmtpSessionOpenYield::WantsTlsUpgrade => {
                        socket = Some(
                            socket
                                .take()
                                .ok_or_else(|| MailError::new("mail_tls_failed"))?
                                .upgrade(connector, &endpoint.host)
                                .await?,
                        )
                    }
                    SmtpSessionOpenYield::WantsUnixConnect(_) => {
                        return Err(MailError::new("mail_tls_failed"));
                    }
                    SmtpSessionOpenYield::WantsRead => {
                        let n = socket
                            .as_mut()
                            .ok_or_else(|| MailError::new("mail_tls_failed"))?
                            .read(&mut bytes)
                            .await
                            .map_err(io_failure)?;
                        budget = budget
                            .checked_sub(n)
                            .ok_or_else(|| MailError::new("mail_response_too_large"))?;
                        arg = Some(bytes[..n].to_vec());
                    }
                    SmtpSessionOpenYield::WantsWrite(bytes) => socket
                        .as_mut()
                        .ok_or_else(|| MailError::new("mail_tls_failed"))?
                        .write(&bytes)
                        .await
                        .map_err(io_failure)?,
                },
            }
        }
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
                        let n = self.socket.read(&mut bytes).await?;
                        budget = budget.checked_sub(n).ok_or_else(protocol_io)?;
                        arg = Some(bytes[..n].to_vec());
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
