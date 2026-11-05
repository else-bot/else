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
pub(super) fn tls_connector() -> Result<TlsConnector, MailError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(TlsConnector::from(Arc::new(
        ClientConfig::with_platform_verifier().map_err(failure)?,
    )))
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
            Self::Tls(s) => s.write_all(bytes).await,
        }
    }
    async fn upgrade(self, connector: &TlsConnector, host: &str) -> Result<Self, MailError> {
        let Self::Plain(socket) = self else {
            return Err(MailError::new("mail_tls_failed"));
        };
        let name = ServerName::try_from(host.to_owned()).map_err(failure)?;
        Ok(Self::Tls(Box::new(
            connector.connect(name, socket).await.map_err(failure)?,
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
        let mut parser = Fragmentizer::new((MESSAGE_LIMIT + 16384) as u32);
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
                    return Err(MailError::new("mail_authentication_failed"));
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
                    return Err(MailError::new("mail_authentication_failed"));
                }
                SmtpCoroutineState::Yielded(y) => match y {
                    SmtpSessionOpenYield::WantsTcpConnect { host, port } => {
                        socket = Some(Socket::Plain(
                            TcpStream::connect((host.as_str(), port))
                                .await
                                .map_err(failure)?,
                        ))
                    }
                    SmtpSessionOpenYield::WantsTlsConnect { host, port } => {
                        socket = Some(
                            Socket::Plain(
                                TcpStream::connect((host.as_str(), port))
                                    .await
                                    .map_err(failure)?,
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
                            .map_err(failure)?;
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
                        .map_err(failure)?,
                },
            }
        }
    }
}
impl SmtpClientAsync for Smtp {
    async fn run<C, T, E>(&mut self, mut coroutine: C) -> Result<T, SmtpClientError>
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
                        self.socket.write(&bytes).await?
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
