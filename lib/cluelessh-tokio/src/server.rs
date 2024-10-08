use cluelessh_connection::{ChannelKind, ChannelNumber, ChannelOperation};
use cluelessh_keys::public::PublicKey;
use cluelessh_transport::server::{KeyExchangeParameters, KeyExchangeResponse};
use futures::future::BoxFuture;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use cluelessh_protocol::{
    auth::{AuthOption, CheckPublicKey, VerifyPassword, VerifySignature},
    ChannelUpdateKind, SshStatus,
};
use eyre::{eyre, ContextCompat, OptionExt, Result, WrapErr};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::info;

use crate::{Channel, ChannelState, PendingChannel};

pub struct ServerListener {
    listener: TcpListener,
    auth_verify: ServerAuth,
    transport_config: cluelessh_transport::server::ServerConfig, // TODO ratelimits etc
}

pub struct ServerConnection<S> {
    stream: Pin<Box<S>>,
    peer_addr: SocketAddr,
    buf: [u8; 1024],

    proto: cluelessh_protocol::ServerConnection,
    operations_send: tokio::sync::mpsc::Sender<Operation>,
    operations_recv: tokio::sync::mpsc::Receiver<Operation>,

    /// Cloned and passed on to channels.
    channel_ops_send: tokio::sync::mpsc::Sender<ChannelOperation>,
    channel_ops_recv: tokio::sync::mpsc::Receiver<ChannelOperation>,

    channels: HashMap<ChannelNumber, ChannelState>,

    /// New channels opened by the peer.
    new_channels: VecDeque<Channel>,

    signature_in_progress: bool,
    auth_verify: ServerAuth,
}

enum Operation {
    VerifyPassword(String, Result<bool>),
    CheckPubkey(Result<bool>, PublicKey),
    VerifySignature(String, Result<bool>),
    KeyExchangeResponseReceived(Result<KeyExchangeResponse>),
}

pub type AuthFn<A, R> = Arc<dyn Fn(A) -> BoxFuture<'static, R> + Send + Sync>;

#[derive(Clone)]
pub struct ServerAuth {
    pub verify_password: Option<AuthFn<VerifyPassword, Result<bool>>>,
    pub verify_signature: Option<AuthFn<VerifySignature, Result<bool>>>,
    pub check_pubkey: Option<AuthFn<CheckPublicKey, Result<bool>>>,
    pub do_key_exchange: AuthFn<KeyExchangeParameters, Result<KeyExchangeResponse>>,
    pub auth_banner: Option<String>,
}
fn _assert_send_sync() {
    fn send<T: Send + Sync>() {}
    send::<ServerAuth>();
}

pub struct SignWithHostKey {
    pub hash: [u8; 32],
    pub public_key: PublicKey,
}

pub enum Error {
    SshStatus(SshStatus),
    ServerError(eyre::Report),
}
impl From<eyre::Report> for Error {
    fn from(value: eyre::Report) -> Self {
        Self::ServerError(value)
    }
}

impl ServerListener {
    pub fn new(
        listener: TcpListener,
        auth_verify: ServerAuth,
        transport_config: cluelessh_transport::server::ServerConfig,
    ) -> Self {
        Self {
            listener,
            auth_verify,
            transport_config,
        }
    }

    pub async fn accept(&mut self) -> Result<ServerConnection<TcpStream>> {
        let (conn, peer_addr) = self.listener.accept().await?;

        Ok(ServerConnection::new(
            conn,
            peer_addr,
            self.auth_verify.clone(),
            self.transport_config.clone(),
        ))
    }
}

impl<S: AsyncRead + AsyncWrite> ServerConnection<S> {
    pub fn new(
        stream: S,
        peer_addr: SocketAddr,
        auth_verify: ServerAuth,
        transport_config: cluelessh_transport::server::ServerConfig,
    ) -> Self {
        let (operations_send, operations_recv) = tokio::sync::mpsc::channel(15);
        let (channel_ops_send, channel_ops_recv) = tokio::sync::mpsc::channel(15);

        let mut options = HashSet::new();
        if auth_verify.verify_password.is_some() {
            options.insert(AuthOption::Password);
        }
        if auth_verify.verify_signature.is_some() {
            options.insert(AuthOption::PublicKey);
        }

        if options.is_empty() {
            panic!("no auth options provided");
        }
        assert_eq!(
            auth_verify.check_pubkey.is_some(),
            auth_verify.verify_signature.is_some(),
            "Public key auth only partially supported"
        );

        Self {
            stream: Box::pin(stream),
            peer_addr,
            buf: [0; 1024],
            operations_send,
            operations_recv,
            channel_ops_send,
            channel_ops_recv,
            channels: HashMap::new(),
            proto: cluelessh_protocol::ServerConnection::new(
                cluelessh_transport::server::ServerConnection::new(
                    cluelessh_protocol::OsRng,
                    transport_config,
                ),
                options,
                auth_verify.auth_banner.clone(),
            ),
            new_channels: VecDeque::new(),
            auth_verify,
            signature_in_progress: false,
        }
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Executes one loop iteration of the main loop.
    // IMPORTANT: no operations on this struct should ever block the main loop, except this one.
    pub async fn progress(&mut self) -> Result<(), Error> {
        if let Some(params) = self.proto.is_waiting_on_key_exchange() {
            if !self.signature_in_progress {
                self.signature_in_progress = true;

                let send = self.operations_send.clone();

                let do_key_exchange = self.auth_verify.do_key_exchange.clone();
                tokio::spawn(async move {
                    let result = do_key_exchange(params).await;
                    let _ = send
                        .send(Operation::KeyExchangeResponseReceived(result))
                        .await;
                });
            }
        }

        if let Some(auth) = self.proto.auth() {
            for req in auth.server_requests() {
                match req {
                    cluelessh_protocol::auth::ServerRequest::VerifyPassword(password_verify) => {
                        let send = self.operations_send.clone();
                        let verify = self
                            .auth_verify
                            .verify_password
                            .clone()
                            .ok_or_eyre("password auth not supported")?;
                        tokio::spawn(async move {
                            let result = verify(password_verify.clone()).await;
                            let _ = send
                                .send(Operation::VerifyPassword(password_verify.user, result))
                                .await;
                        });
                    }
                    cluelessh_protocol::auth::ServerRequest::CheckPubkey(check_pubkey) => {
                        let send = self.operations_send.clone();
                        let check = self
                            .auth_verify
                            .check_pubkey
                            .clone()
                            .ok_or_eyre("pubkey auth not supported")?;
                        tokio::spawn(async move {
                            let result = check(check_pubkey.clone()).await;
                            let _ = send
                                .send(Operation::CheckPubkey(result, check_pubkey.public_key))
                                .await;
                        });
                    }
                    cluelessh_protocol::auth::ServerRequest::VerifySignature(pubkey_verify) => {
                        let send = self.operations_send.clone();
                        let verify = self
                            .auth_verify
                            .verify_signature
                            .clone()
                            .ok_or_eyre("pubkey auth not supported")?;
                        tokio::spawn(async move {
                            let result = verify(pubkey_verify.clone()).await;
                            let _ = send
                                .send(Operation::VerifySignature(pubkey_verify.user, result))
                                .await;
                        });
                    }
                }
            }
        }

        if let Some(channels) = self.proto.channels() {
            while let Some(update) = channels.next_channel_update() {
                match &update.kind {
                    ChannelUpdateKind::Open(channel_kind) => {
                        let channel = self.channels.get_mut(&update.number);

                        match channel {
                            // We opened.
                            Some(ChannelState::Pending { updates_send, .. }) => {
                                let updates_send = updates_send.clone();
                                let old = self
                                    .channels
                                    .insert(update.number, ChannelState::Ready(updates_send));
                                match old.unwrap() {
                                    ChannelState::Pending { ready_send, .. } => {
                                        let _ = ready_send.send(Ok(()));
                                    }
                                    _ => unreachable!(),
                                }
                            }
                            Some(ChannelState::Ready(_)) => {
                                return Err(Error::ServerError(eyre!(
                                    "attemping to open channel twice: {}",
                                    update.number
                                )))
                            }
                            // They opened.
                            None => {
                                let (updates_send, updates_recv) = tokio::sync::mpsc::channel(10);

                                let number = update.number;

                                self.channels
                                    .insert(number, ChannelState::Ready(updates_send));

                                let channel = Channel {
                                    number,
                                    updates_recv,
                                    ops_send: self.channel_ops_send.clone(),
                                    kind: channel_kind.clone(),
                                };
                                self.new_channels.push_back(channel);
                            }
                        }
                    }
                    ChannelUpdateKind::OpenFailed { message, .. } => {
                        let channel = self
                            .channels
                            .get_mut(&update.number)
                            .wrap_err("unknown channel")?;
                        match channel {
                            ChannelState::Pending { .. } => {
                                let old = self.channels.remove(&update.number);
                                match old.unwrap() {
                                    ChannelState::Pending { ready_send, .. } => {
                                        let _ = ready_send.send(Err(message.clone()));
                                    }
                                    _ => unreachable!(),
                                }
                            }
                            ChannelState::Ready(_) => {
                                return Err(Error::ServerError(eyre!(
                                    "attemping to open channel twice: {}",
                                    update.number
                                )))
                            }
                        }
                    }
                    _ => {
                        let channel = self
                            .channels
                            .get_mut(&update.number)
                            .wrap_err("unknown channel")?;
                        match channel {
                            ChannelState::Pending { .. } => {
                                return Err(Error::ServerError(eyre!("channel not ready yet")))
                            }
                            ChannelState::Ready(updates_send) => {
                                let _ = updates_send.send(update.kind).await;
                            }
                        }
                    }
                }
            }
        }

        // Make sure that we send all queued messages before going into the select, waiting for things to happen.
        self.send_off_data().await?;

        tokio::select! {
            read = self.stream.read(&mut self.buf) => {
                let read = read.wrap_err("reading from connection")?;
                if read == 0 {
                    info!("Did not read any bytes from TCP stream, EOF");
                    return Err(Error::SshStatus(SshStatus::Disconnect));
                }
                if let Err(err) = self.proto.recv_bytes(&self.buf[..read]) {
                    return Err(Error::SshStatus(err));
                }
            }
            channel_op = self.channel_ops_recv.recv() => {
                let channels = self.proto.channels().expect("connection not ready");
                if let Some(channel_op) = channel_op {
                    channels.do_operation(channel_op);
                }
            }
            op = self.operations_recv.recv() => {
                match op {
                    Some(Operation::VerifySignature(user, result)) => if let Some(auth) = self.proto.auth() {
                        auth.verification_result(result?, user);
                    },
                    Some(Operation::CheckPubkey(result, public_key)) => if let Some(auth) = self.proto.auth() {
                        auth.pubkey_check_result(result?, public_key);
                    },
                    Some(Operation::VerifyPassword(user, result)) => if let Some(auth) = self.proto.auth() {
                        auth.verification_result(result?, user);
                    },
                    Some(Operation::KeyExchangeResponseReceived(signature)) => {
                        let signature = signature?;
                        self.proto.do_key_exchange(signature);
                    }
                    None => {}
                }
                self.send_off_data().await?;
            }
        }

        Ok(())
    }

    async fn send_off_data(&mut self) -> Result<()> {
        self.proto.progress();
        while let Some(msg) = self.proto.next_msg_to_send() {
            self.stream
                .write_all(&msg.to_bytes())
                .await
                .wrap_err("writing response")?;
        }
        Ok(())
    }

    pub fn open_channel(&mut self, kind: ChannelKind) -> PendingChannel {
        let Some(channels) = self.proto.channels() else {
            panic!("connection not ready yet")
        };
        let (updates_send, updates_recv) = tokio::sync::mpsc::channel(10);
        let (ready_send, ready_recv) = tokio::sync::oneshot::channel();

        let number = channels.create_channel(kind.clone());

        self.channels.insert(
            number,
            ChannelState::Pending {
                ready_send,
                updates_send,
            },
        );

        PendingChannel {
            ready_recv,
            channel: Channel {
                number,
                updates_recv,
                ops_send: self.channel_ops_send.clone(),
                kind,
            },
        }
    }

    pub fn next_new_channel(&mut self) -> Option<Channel> {
        self.new_channels.pop_front()
    }

    pub fn inner(&self) -> &cluelessh_protocol::ServerConnection {
        &self.proto
    }
}
