use std::collections::{HashMap, VecDeque};
use tracing::{debug, info, warn};

use ssh_transport::client_error;
use ssh_transport::packet::Packet;
use ssh_transport::Result;

/// A channel number (on our side).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelNumber(pub u32);

pub struct ServerChannelsState {
    packets_to_send: VecDeque<Packet>,
    channel_updates: VecDeque<ChannelUpdate>,

    channels: HashMap<ChannelNumber, Channel>,
    next_channel_id: ChannelNumber,
}

struct Channel {
    /// Whether our side has closed this channel.
    we_closed: bool,
    /// The channel number for the other side.
    peer_channel: u32,
}

/// An update from a channel.
/// The receiver-equivalent of [`ChannelOperation`].
pub struct ChannelUpdate {
    pub number: ChannelNumber,
    pub kind: ChannelUpdateKind,
}
pub enum ChannelUpdateKind {
    Open(ChannelOpen),
    Request(ChannelRequest),
    Data { data: Vec<u8> },
    ExtendedData { code: u32, data: Vec<u8> },
    Eof,
    Closed,
}

pub enum ChannelOpen {
    Session,
}

pub struct ChannelRequest {
    pub want_reply: bool,
    pub kind: ChannelRequestKind,
}

pub enum ChannelRequestKind {
    PtyReq {
        term: String,
        width_chars: u32,
        height_rows: u32,
        width_px: u32,
        height_px: u32,
        term_modes: Vec<u8>,
    },
    Shell,
    Exec {
        command: Vec<u8>,
    },
}

impl ChannelNumber {
    #[must_use]
    pub fn construct_op(self, kind: ChannelOperationKind) -> ChannelOperation {
        ChannelOperation { number: self, kind }
    }
}

/// An operation to do on a channel.
/// The sender-equivalent of [`ChannelUpdate`].
pub struct ChannelOperation {
    pub number: ChannelNumber,
    pub kind: ChannelOperationKind,
}

pub enum ChannelOperationKind {
    Success,
    Failure,
    Data(Vec<u8>),
    Close,
}

impl ServerChannelsState {
    pub fn new() -> Self {
        ServerChannelsState {
            packets_to_send: VecDeque::new(),
            channels: HashMap::new(),
            channel_updates: VecDeque::new(),
            next_channel_id: ChannelNumber(0),
        }
    }

    pub fn recv_packet(&mut self, packet: Packet) -> Result<()> {
        // TODO: window

        let mut packet = packet.payload_parser();
        let packet_type = packet.u8()?;
        match packet_type {
            Packet::SSH_MSG_GLOBAL_REQUEST => {
                let request_name = packet.utf8_string()?;
                let want_reply = packet.bool()?;
                debug!(?request_name, ?want_reply, "Received global request");

                self.packets_to_send
                    .push_back(Packet::new_msg_request_failure());
            }
            Packet::SSH_MSG_CHANNEL_OPEN => {
                // <https://datatracker.ietf.org/doc/html/rfc4254#section-5.1>
                let channel_type = packet.utf8_string()?;
                let sender_channel = packet.u32()?;
                let initial_window_size = packet.u32()?;
                let max_packet_size = packet.u32()?;

                debug!(?channel_type, ?sender_channel, "Opening channel");

                let update_message = match channel_type {
                    "session" => ChannelOpen::Session,
                    _ => {
                        self.packets_to_send
                            .push_back(Packet::new_msg_channel_open_failure(
                                sender_channel,
                                3, // SSH_OPEN_UNKNOWN_CHANNEL_TYPE
                                b"unknown channel type",
                                b"",
                            ));
                        return Ok(());
                    }
                };

                let our_number = self.next_channel_id;
                self.next_channel_id =
                    ChannelNumber(self.next_channel_id.0.checked_add(1).ok_or_else(|| {
                        client_error!("created too many channels, overflowed the counter")
                    })?);

                self.packets_to_send
                    .push_back(Packet::new_msg_channel_open_confirmation(
                        our_number.0,
                        sender_channel,
                        initial_window_size,
                        max_packet_size,
                    ));

                self.channels.insert(
                    our_number,
                    Channel {
                        we_closed: false,
                        peer_channel: sender_channel,
                    },
                );

                self.channel_updates.push_back(ChannelUpdate {
                    number: our_number,
                    kind: ChannelUpdateKind::Open(update_message),
                });

                debug!(?channel_type, ?our_number, "Successfully opened channel");
            }
            Packet::SSH_MSG_CHANNEL_DATA => {
                let our_channel = packet.u32()?;
                let our_channel = self.validate_channel(our_channel)?;
                let data = packet.string()?;

                let _ = self.channel(our_channel)?;

                self.channel_updates.push_back(ChannelUpdate {
                    number: our_channel,
                    kind: ChannelUpdateKind::Data {
                        data: data.to_owned(),
                    },
                });
            }
            Packet::SSH_MSG_CHANNEL_CLOSE => {
                // <https://datatracker.ietf.org/doc/html/rfc4254#section-5.3>
                let our_channel = packet.u32()?;
                let our_channel = self.validate_channel(our_channel)?;
                let channel = self.channel(our_channel)?;
                if !channel.we_closed {
                    let close = Packet::new_msg_channel_close(channel.peer_channel);
                    self.packets_to_send.push_back(close);
                }

                self.channels.remove(&our_channel);

                self.channel_updates.push_back(ChannelUpdate {
                    number: our_channel,
                    kind: ChannelUpdateKind::Closed,
                });

                debug!("Channel has been closed");
            }
            Packet::SSH_MSG_CHANNEL_REQUEST => {
                let our_channel = packet.u32()?;
                let our_channel = self.validate_channel(our_channel)?;
                let request_type = packet.utf8_string()?;
                let want_reply = packet.bool()?;

                debug!(?our_channel, ?request_type, "Got channel request");

                let channel = self.channel(our_channel)?;
                let peer_channel = channel.peer_channel;

                let channel_request_kind = match request_type {
                    "pty-req" => {
                        let term = packet.utf8_string()?;
                        let width_chars = packet.u32()?;
                        let height_rows = packet.u32()?;
                        let width_px = packet.u32()?;
                        let height_px = packet.u32()?;
                        let term_modes = packet.string()?;

                        debug!(
                            ?our_channel,
                            ?term,
                            ?width_chars,
                            ?height_rows,
                            "Trying to open a terminal"
                        );

                        ChannelRequestKind::PtyReq {
                            term: term.to_owned(),
                            width_chars,
                            height_rows,
                            width_px,
                            height_px,
                            term_modes: term_modes.to_owned(),
                        }
                    }
                    "shell" => {
                        info!(?our_channel, "Opening shell");
                        ChannelRequestKind::Shell
                    }
                    "exec" => {
                        let command = packet.string()?;
                        info!(?our_channel, command = ?String::from_utf8_lossy(command), "Executing command");
                        ChannelRequestKind::Exec {
                            command: command.to_owned(),
                        }
                    }
                    "signal" => {
                        debug!(?our_channel, "Received signal");
                        // Ignore signals, something we can do.
                        return Ok(());
                    }
                    _ => {
                        warn!(?request_type, ?our_channel, "Unknown channel request");
                        self.send_channel_failure(peer_channel);
                        return Ok(());
                    }
                };

                self.channel_updates.push_back(ChannelUpdate {
                    number: our_channel,
                    kind: ChannelUpdateKind::Request(ChannelRequest {
                        want_reply,
                        kind: channel_request_kind,
                    }),
                })
            }
            _ => {
                todo!("{packet_type}");
            }
        }

        Ok(())
    }

    pub fn packets_to_send(&mut self) -> impl Iterator<Item = Packet> + '_ {
        self.packets_to_send.drain(..)
    }

    pub fn next_channel_update(&mut self) -> Option<ChannelUpdate> {
        self.channel_updates.pop_front()
    }

    pub fn do_operation(&mut self, op: ChannelOperation) {
        let peer = self
            .channel(op.number)
            .expect("passed channel ID that does not exist")
            .peer_channel;
        match op.kind {
            ChannelOperationKind::Success => self.send_channel_success(peer),
            ChannelOperationKind::Failure => self.send_channel_failure(peer),
            ChannelOperationKind::Data(data) => {
                self.packets_to_send
                    .push_back(Packet::new_msg_channel_data(peer, &data));
            }
            ChannelOperationKind::Close => {
                // <https://datatracker.ietf.org/doc/html/rfc4254#section-5.3>
                self.packets_to_send
                    .push_back(Packet::new_msg_channel_close(peer));

                let channel = self.channel(op.number).unwrap();
                channel.we_closed = true;
            }
        }
    }

    fn send_channel_success(&mut self, recipient_channel: u32) {
        self.packets_to_send
            .push_back(Packet::new_msg_channel_success(recipient_channel));
    }

    fn send_channel_failure(&mut self, recipient_channel: u32) {
        self.packets_to_send
            .push_back(Packet::new_msg_channel_failure(recipient_channel));
    }

    fn validate_channel(&self, number: u32) -> Result<ChannelNumber> {
        if !self.channels.contains_key(&ChannelNumber(number)) {
            return Err(client_error!("unknown channel: {number}"));
        }
        Ok(ChannelNumber(number))
    }

    fn channel(&mut self, number: ChannelNumber) -> Result<&mut Channel> {
        self.channels
            .get_mut(&number)
            .ok_or_else(|| client_error!("unknown channel: {number:?}"))
    }
}
