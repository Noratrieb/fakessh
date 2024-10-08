use std::{collections::VecDeque, mem::take};

use crate::crypto::{
    self, AlgorithmName, EncryptionAlgorithm, HostKeySigningAlgorithm, KexAlgorithm, SharedSecret,
    SupportedAlgorithms,
};
use crate::packet::{
    KeyExchangeEcDhInitPacket, KeyExchangeInitPacket, Packet, PacketTransport, ProtocolIdentParser,
    RecvBytesResult,
};
use crate::{peer_error, Msg, SshRng, SshStatus};
use crate::{Result, SessionId};
use cluelessh_format::numbers;
use cluelessh_format::{NameList, Reader, Writer};
use cluelessh_keys::private::PlaintextPrivateKey;
use cluelessh_keys::signature::Signature;
use tracing::{debug, info, trace};

pub struct ServerConnection {
    state: ServerState,
    packet_transport: PacketTransport,
    rng: Box<dyn SshRng + Send + Sync>,

    config: ServerConfig,

    plaintext_packets: VecDeque<Packet>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub server_identification: Vec<u8>,
    pub host_keys: Vec<cluelessh_keys::public::PublicKey>,
}

enum ServerState {
    ProtoExchange {
        ident_parser: ProtocolIdentParser,
    },
    KeyExchangeInit {
        client_identification: Vec<u8>,
    },
    DhKeyInit {
        client_identification: Vec<u8>,
        client_kexinit: Vec<u8>,
        server_kexinit: Vec<u8>,
        kex_algorithm: crypto::KexAlgorithm,
        server_host_key_algorithm: HostKeySigningAlgorithm,
        encryption_client_to_server: EncryptionAlgorithm,
        encryption_server_to_client: EncryptionAlgorithm,
    },
    WaitingForKeyExchange {
        client_identification: Vec<u8>,
        client_kexinit: Vec<u8>,
        server_kexinit: Vec<u8>,
        kex_algorithm: crypto::KexAlgorithm,
        server_host_key_algorithm: HostKeySigningAlgorithm,
        encryption_client_to_server: EncryptionAlgorithm,
        encryption_server_to_client: EncryptionAlgorithm,
        client_ephemeral_public_key: Vec<u8>,
    },
    NewKeys {
        /// h
        hash: [u8; 32],
        /// k
        shared_secret: SharedSecret,
        encryption_client_to_server: EncryptionAlgorithm,
        encryption_server_to_client: EncryptionAlgorithm,
    },
    ServiceRequest {
        session_id: SessionId,
        may_send_extensions: bool,
    },
    Open {
        session_id: SessionId,
    },
}

pub struct KeyExchangeParameters {
    pub client_ident: Vec<u8>,
    pub server_ident: Vec<u8>,
    pub client_kexinit: Vec<u8>,
    pub server_kexinit: Vec<u8>,
    pub eph_client_public_key: Vec<u8>,
    pub server_host_key_algorithm: HostKeySigningAlgorithm,
    pub kex_algorithm: KexAlgorithm,
}

pub struct KeyExchangeResponse {
    pub hash: SessionId,
    pub server_ephemeral_public_key: Vec<u8>,
    pub shared_secret: SharedSecret,
    pub signature: Signature,
}

impl ServerConnection {
    pub fn new(rng: impl SshRng + Send + Sync + 'static, config: ServerConfig) -> Self {
        Self {
            state: ServerState::ProtoExchange {
                ident_parser: ProtocolIdentParser::new(),
            },
            packet_transport: PacketTransport::new(),
            rng: Box::new(rng),
            config,
            plaintext_packets: VecDeque::new(),
        }
    }

    pub fn recv_bytes(&mut self, mut bytes: &[u8]) -> Result<()> {
        while let RecvBytesResult::Partial { consumed } = self.recv_bytes_inner(bytes)? {
            bytes = &bytes[consumed..];
            if bytes.is_empty() {
                break;
            }
        }
        Ok(())
    }

    fn recv_bytes_inner(&mut self, bytes: &[u8]) -> Result<RecvBytesResult> {
        if let ServerState::ProtoExchange { ident_parser } = &mut self.state {
            ident_parser.recv_bytes(bytes);
            if let Some(client_identification) = ident_parser.get_peer_ident() {
                self.packet_transport
                    .queue_send_protocol_info(self.config.server_identification.clone());
                self.state = ServerState::KeyExchangeInit {
                    client_identification,
                };
            }
            // This means that we must be called at least twice, which is fine I think.
            return Ok(RecvBytesResult::Full);
        }

        let consumed = self.packet_transport.recv_bytes(bytes)?;

        while let Some(packet) = self.packet_transport.recv_next_packet() {
            let packet_type = packet.packet_type();
            let packet_type_string = numbers::packet_type_to_string(packet_type);

            trace!(%packet_type, %packet_type_string, packet_len = %packet.payload.len(), "Received packet");

            // Handle some packets ignoring the state.
            match packet_type {
                numbers::SSH_MSG_DISCONNECT => {
                    // <https://datatracker.ietf.org/doc/html/rfc4253#section-11.1>
                    let mut disconnect = Reader::new(&packet.payload[1..]);
                    let reason = disconnect.u32()?;
                    let description = disconnect.utf8_string()?;
                    let _language_tag = disconnect.utf8_string()?;

                    let reason_string = numbers::disconnect_reason_to_string(reason);

                    debug!(%reason, %reason_string, %description, "Client disconnecting");

                    return Err(SshStatus::Disconnect);
                }
                numbers::SSH_MSG_IGNORE => {
                    // <https://datatracker.ietf.org/doc/html/rfc4253#section-11.2>
                    let mut p = Reader::new(&packet.payload[1..]);
                    let _ = p.string()?;
                    continue;
                }
                numbers::SSH_MSG_DEBUG => {
                    // <https://datatracker.ietf.org/doc/html/rfc4253#section-11.3>
                    let mut p = Reader::new(&packet.payload[1..]);
                    let always_display = p.bool()?;
                    let msg = p.utf8_string()?;
                    let _language_tag = p.utf8_string()?;

                    if always_display {
                        info!(%msg, "Received debug message (SSH_MSG_DEBUG)");
                    } else {
                        debug!(%msg, "Received debug message (SSH_MSG_DEBUG)")
                    }
                    continue;
                }
                _ => {}
            }

            match &mut self.state {
                ServerState::ProtoExchange { .. } => unreachable!("handled above"),
                ServerState::KeyExchangeInit {
                    client_identification,
                } => {
                    let kex = KeyExchangeInitPacket::parse(&packet.payload)?;

                    let sup_algs = SupportedAlgorithms::secure(&self.config.host_keys);

                    let kex_algorithm = sup_algs.key_exchange.find(false, kex.kex_algorithms.0)?;
                    debug!(name = %kex_algorithm.name(), "Using KEX algorithm");

                    // <https://datatracker.ietf.org/doc/html/rfc8308#section-2.1>
                    // TODO: Send some extensions
                    // TODO: Because of the terrapin attack, we probably want to implement strict kex for that.
                    let _client_supports_extensions = kex.kex_algorithms.contains("ext-info-c");

                    let server_host_key_algorithm = sup_algs
                        .hostkey_sign
                        .find(false, kex.server_host_key_algorithms.0)?;
                    debug!(name = %server_host_key_algorithm.name(), "Using host key algorithm");

                    // TODO: Implement aes128-ctr
                    let _ = crypto::encrypt::ENC_AES128_CTR;

                    let encryption_client_to_server = sup_algs
                        .encryption_from_peer
                        .find(false, kex.encryption_algorithms_client_to_server.0)?;
                    debug!(name = %encryption_client_to_server.name(), "Using encryption algorithm C->S");

                    let encryption_server_to_client = sup_algs
                        .encryption_to_peer
                        .find(false, kex.encryption_algorithms_server_to_client.0)?;
                    debug!(name = %encryption_server_to_client.name(), "Using encryption algorithm S->C");

                    let mac_algorithm_client_to_server = sup_algs
                        .mac_from_peer
                        .find(false, kex.mac_algorithms_client_to_server.0)?;
                    let mac_algorithm_server_to_client = sup_algs
                        .mac_to_peer
                        .find(false, kex.mac_algorithms_server_to_client.0)?;

                    let compression_algorithm_client_to_server = sup_algs
                        .compression_from_peer
                        .find(false, kex.compression_algorithms_client_to_server.0)?;
                    let compression_algorithm_server_to_client = sup_algs
                        .compression_to_peer
                        .find(false, kex.compression_algorithms_server_to_client.0)?;

                    let _ = kex.languages_client_to_server;
                    let _ = kex.languages_server_to_client;

                    if kex.first_kex_packet_follows {
                        return Err(peer_error!(
                            "the client wants to send a guessed packet, that's annoying :("
                        ));
                    }

                    let mut cookie = [0; 16];
                    self.rng.fill_bytes(&mut cookie);
                    // <https://datatracker.ietf.org/doc/html/rfc8308#section-2.1>
                    let kex_algorithms = format!("{},ext-info-s", kex_algorithm.name());
                    let server_kexinit = KeyExchangeInitPacket {
                        cookie,
                        // TODO: we should send *all* our algorithms here...
                        kex_algorithms: NameList::multi(&kex_algorithms),
                        server_host_key_algorithms: NameList::one(server_host_key_algorithm.name()),
                        encryption_algorithms_client_to_server: NameList::one(
                            encryption_client_to_server.name(),
                        ),
                        encryption_algorithms_server_to_client: NameList::one(
                            encryption_server_to_client.name(),
                        ),
                        mac_algorithms_client_to_server: NameList::one(
                            mac_algorithm_client_to_server,
                        ),
                        mac_algorithms_server_to_client: NameList::one(
                            mac_algorithm_server_to_client,
                        ),
                        compression_algorithms_client_to_server: NameList::one(
                            compression_algorithm_client_to_server,
                        ),
                        compression_algorithms_server_to_client: NameList::one(
                            compression_algorithm_server_to_client,
                        ),
                        languages_client_to_server: NameList::none(),
                        languages_server_to_client: NameList::none(),
                        first_kex_packet_follows: false,
                    };

                    let client_identification = take(client_identification);
                    let server_kexinit_payload = server_kexinit.to_bytes();
                    self.packet_transport.queue_packet(Packet {
                        payload: server_kexinit_payload.clone(),
                    });
                    self.state = ServerState::DhKeyInit {
                        client_identification,
                        client_kexinit: packet.payload,
                        server_kexinit: server_kexinit_payload,
                        kex_algorithm,
                        server_host_key_algorithm,
                        encryption_client_to_server,
                        encryption_server_to_client,
                    };
                }
                ServerState::DhKeyInit {
                    client_identification,
                    client_kexinit,
                    server_kexinit,
                    kex_algorithm,
                    server_host_key_algorithm,
                    encryption_client_to_server,
                    encryption_server_to_client,
                } => {
                    let dh = KeyExchangeEcDhInitPacket::parse(&packet.payload)?;

                    let client_ephemeral_public_key = dh.qc;

                    self.state = ServerState::WaitingForKeyExchange {
                        client_identification: client_identification.clone(),
                        client_kexinit: client_kexinit.clone(),
                        server_kexinit: server_kexinit.clone(),
                        kex_algorithm: *kex_algorithm,
                        server_host_key_algorithm: server_host_key_algorithm.clone(),
                        encryption_client_to_server: *encryption_client_to_server,
                        encryption_server_to_client: *encryption_server_to_client,
                        client_ephemeral_public_key: client_ephemeral_public_key.to_vec(),
                    };
                }
                ServerState::WaitingForKeyExchange { .. } => {
                    return Err(peer_error!("unexpected packet"));
                }
                ServerState::NewKeys {
                    hash: h,
                    shared_secret: k,
                    encryption_client_to_server,
                    encryption_server_to_client,
                } => {
                    if packet.payload != [numbers::SSH_MSG_NEWKEYS] {
                        return Err(peer_error!("did not send SSH_MSG_NEWKEYS"));
                    }

                    self.packet_transport.queue_packet(Packet {
                        payload: vec![numbers::SSH_MSG_NEWKEYS],
                    });

                    self.packet_transport.set_key(
                        *h,
                        k,
                        *encryption_client_to_server,
                        *encryption_server_to_client,
                        true,
                    );
                    self.state = ServerState::ServiceRequest {
                        session_id: SessionId(*h),
                        may_send_extensions: true, // TODO: false if the client didn't advertise them
                    };
                }
                ServerState::ServiceRequest {
                    session_id,
                    may_send_extensions,
                } => match packet_type {
                    numbers::SSH_MSG_SERVICE_REQUEST => {
                        let mut p = packet.payload_parser();
                        p.u8()?;
                        let service = p.utf8_string()?;
                        debug!(%service, "Client requesting service");

                        if service != "ssh-userauth" {
                            return Err(peer_error!("only supports ssh-userauth"));
                        }

                        self.packet_transport.queue_packet(Packet {
                            payload: {
                                let mut writer = Writer::new();
                                writer.u8(numbers::SSH_MSG_SERVICE_ACCEPT);
                                writer.string(service.as_bytes());
                                writer.finish()
                            },
                        });
                        self.state = ServerState::Open {
                            session_id: *session_id,
                        };
                    }
                    numbers::SSH_MSG_EXT_INFO if *may_send_extensions => {
                        let mut p = packet.payload_parser();
                        p.u8()?;
                        let count = p.u32()?;

                        debug!(%count, "Received extensions");

                        for _ in 0..count {
                            // while the spec doesn't say it, if you send an extension name that's invalid UTF-8 you deserve the error
                            let name = p.utf8_string()?;
                            let _value = p.string()?;
                            debug!(?name, "Received extension");
                        }

                        self.state = ServerState::ServiceRequest {
                            session_id: *session_id,
                            may_send_extensions: false,
                        };
                    }
                    _ => {
                        return Err(peer_error!(
                            "unexpected packet: {packet_type}, expected SSH_MSG_SERVICE_REQUEST"
                        ))
                    }
                },
                ServerState::Open { .. } => {
                    self.plaintext_packets.push_back(packet);
                }
            }
        }
        Ok(consumed)
    }

    pub fn is_open(&self) -> Option<SessionId> {
        match self.state {
            ServerState::Open { session_id } => Some(session_id),
            _ => None,
        }
    }

    pub fn is_waiting_on_key_exchange(&self) -> Option<KeyExchangeParameters> {
        match &self.state {
            ServerState::WaitingForKeyExchange {
                client_identification,
                client_kexinit,
                server_kexinit,
                kex_algorithm,
                server_host_key_algorithm,
                client_ephemeral_public_key,
                ..
            } => Some(KeyExchangeParameters {
                client_ident: client_identification.clone(),
                server_ident: self.config.server_identification.to_vec(),
                client_kexinit: client_kexinit.clone(),
                server_kexinit: server_kexinit.clone(),
                eph_client_public_key: client_ephemeral_public_key.clone(),
                server_host_key_algorithm: server_host_key_algorithm.clone(),
                kex_algorithm: *kex_algorithm,
            }),
            _ => None,
        }
    }

    pub fn do_key_exchange(&mut self, response: KeyExchangeResponse) {
        match &self.state {
            ServerState::WaitingForKeyExchange {
                encryption_client_to_server,
                encryption_server_to_client,
                server_host_key_algorithm,
                ..
            } => {
                let packet = Packet::new_msg_kex_ecdh_reply(
                    &server_host_key_algorithm.public_key().to_wire_encoding(),
                    &response.server_ephemeral_public_key,
                    &response.signature.to_wire_encoding(),
                );

                self.packet_transport.queue_packet(packet);
                self.state = ServerState::NewKeys {
                    hash: response.hash.0,
                    shared_secret: response.shared_secret.clone(),
                    encryption_client_to_server: *encryption_client_to_server,
                    encryption_server_to_client: *encryption_server_to_client,
                };
            }
            _ => unreachable!("doing signature while not waiting for it"),
        }
    }

    pub fn next_msg_to_send(&mut self) -> Option<Msg> {
        self.packet_transport.next_msg_to_send()
    }

    pub fn next_plaintext_packet(&mut self) -> Option<Packet> {
        self.plaintext_packets.pop_front()
    }

    pub fn send_plaintext_packet(&mut self, packet: Packet) {
        self.packet_transport.queue_packet(packet);
    }
}

pub fn do_key_exchange(
    msg: KeyExchangeParameters,
    private: &PlaintextPrivateKey,
    rng: &mut dyn SshRng,
) -> Result<KeyExchangeResponse> {
    let server_secret = (msg.kex_algorithm.generate_secret)(rng);
    let server_ephemeral_public_key = server_secret.pubkey;
    let shared_secret = (server_secret.exchange)(&msg.eph_client_public_key)?;
    let pub_hostkey = msg.server_host_key_algorithm.public_key();

    let hash = crypto::key_exchange_hash(
        &msg.client_ident,
        &msg.server_ident,
        &msg.client_kexinit,
        &msg.server_kexinit,
        &pub_hostkey.to_wire_encoding(),
        &msg.eph_client_public_key,
        &server_ephemeral_public_key,
        &shared_secret,
    );

    Ok(KeyExchangeResponse {
        hash: SessionId(hash),
        server_ephemeral_public_key,
        shared_secret,
        signature: private.private_key.sign(&hash),
    })
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use crate::{
        packet::MsgKind,
        server::{ServerConfig, ServerConnection},
        SshRng,
    };

    struct NoRng;
    impl SshRng for NoRng {
        fn fill_bytes(&mut self, _: &mut [u8]) {
            unreachable!()
        }
    }

    struct HardcodedRng(Vec<u8>);
    impl SshRng for HardcodedRng {
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            dest.copy_from_slice(&self.0[..dest.len()]);
            self.0.splice(0..dest.len(), []);
        }
    }

    #[test]
    fn protocol_exchange() {
        let mut con = ServerConnection::new(NoRng, ServerConfig::default());
        con.recv_bytes(b"SSH-2.0-OpenSSH_9.7\r\n").unwrap();
        let msg = con.next_msg_to_send().unwrap();
        assert!(matches!(msg.0, MsgKind::ServerProtocolInfo(_)));
    }

    #[test]
    fn protocol_exchange_slow_client() {
        let mut con = ServerConnection::new(NoRng, ServerConfig::default());
        con.recv_bytes(b"SSH-2.0-").unwrap();
        con.recv_bytes(b"OpenSSH_9.7\r\n").unwrap();
        let msg = con.next_msg_to_send().unwrap();
        assert!(matches!(msg.0, MsgKind::ServerProtocolInfo(_)));
    }

    #[test]
    #[ignore = "this is super annoying, use expect-test please"]
    fn handshake() {
        #[rustfmt::skip]
        let rng = vec![
            0x14, 0xa2, 0x04, 0xa5, 0x4b, 0x2f, 0x5f, 0xa7, 0xff, 0x53, 0x13, 0x67, 0x57, 0x67, 0xbc,
            0x55, 0x3f, 0xc0, 0x6c, 0x0d, 0x07, 0x8f, 0xe2, 0x75, 0x95, 0x18, 0x4b, 0xd2, 0xcb, 0xd0,
            0x64, 0x06, 0x14, 0xa2, 0x04, 0xa5, 0x4b, 0x2f, 0x5f, 0xa7, 0xff, 0x53, 0x13, 0x67, 0x57,
            0x67, 0xbc, 0x55, 0x3f, 0xc0, 0x6c, 0x0d, 0x07, 0x8f, 0xe2, 0x75, 0x95, 0x18, 0x4b, 0xd2,
            0xcb, 0xd0, 0x64, 0x06, 0x67, 0xbc, 0x55, 0x3f, 0xc0, 0x6c, 0x0d, 0x07, 0x8f, 0xe2, 0x75,
            0x95, 0x18, 0x4b, 0xd2, 0xcb, 0xd0, 0x64, 0x06,
        ];
        struct Part {
            client: &'static [u8],
            server: &'static [u8],
        }

        // Extracted from a real OpenSSH client using this server (with hardcoded creds) using Wireshark.
        let conversation = [
            Part {
                client: &hex!("5353482d322e302d4f70656e5353485f392e370d0a"),
                server: &hex!("5353482d322e302d4f70656e5353485f392e370d0a"),
            },
            // KEX Init
            Part {
                client: &hex!(
                    "000005fc0714fd3d911937c7294823f93c5ba691f77e00000131736e747275703736317832353531392d736861353132406f70656e7373682e636f6d2c637572766532353531392d7368613235362c637572766532353531392d736861323536406c69627373682e6f72672c656364682d736861322d6e697374703235362c656364682d736861322d6e697374703338342c656364682d736861322d6e697374703532312c6469666669652d68656c6c6d616e2d67726f75702d65786368616e67652d7368613235362c6469666669652d68656c6c6d616e2d67726f757031362d7368613531322c6469666669652d68656c6c6d616e2d67726f757031382d7368613531322c6469666669652d68656c6c6d616e2d67726f757031342d7368613235362c6578742d696e666f2d632c6b65782d7374726963742d632d763030406f70656e7373682e636f6d000001cf7373682d656432353531392d636572742d763031406f70656e7373682e636f6d2c65636473612d736861322d6e697374703235362d636572742d763031406f70656e7373682e636f6d2c65636473612d736861322d6e697374703338342d636572742d763031406f70656e7373682e636f6d2c65636473612d736861322d6e697374703532312d636572742d763031406f70656e7373682e636f6d2c736b2d7373682d656432353531392d636572742d763031406f70656e7373682e636f6d2c736b2d65636473612d736861322d6e697374703235362d636572742d763031406f70656e7373682e636f6d2c7273612d736861322d3531322d636572742d763031406f70656e7373682e636f6d2c7273612d736861322d3235362d636572742d763031406f70656e7373682e636f6d2c7373682d656432353531392c65636473612d736861322d6e697374703235362c65636473612d736861322d6e697374703338342c65636473612d736861322d6e697374703532312c736b2d7373682d65643235353139406f70656e7373682e636f6d2c736b2d65636473612d736861322d6e69737470323536406f70656e7373682e636f6d2c7273612d736861322d3531322c7273612d736861322d3235360000006c63686163686132302d706f6c7931333035406f70656e7373682e636f6d2c6165733132382d6374722c6165733139322d6374722c6165733235362d6374722c6165733132382d67636d406f70656e7373682e636f6d2c6165733235362d67636d406f70656e7373682e636f6d0000006c63686163686132302d706f6c7931333035406f70656e7373682e636f6d2c6165733132382d6374722c6165733139322d6374722c6165733235362d6374722c6165733132382d67636d406f70656e7373682e636f6d2c6165733235362d67636d406f70656e7373682e636f6d000000d5756d61632d36342d65746d406f70656e7373682e636f6d2c756d61632d3132382d65746d406f70656e7373682e636f6d2c686d61632d736861322d3235362d65746d406f70656e7373682e636f6d2c686d61632d736861322d3531322d65746d406f70656e7373682e636f6d2c686d61632d736861312d65746d406f70656e7373682e636f6d2c756d61632d3634406f70656e7373682e636f6d2c756d61632d313238406f70656e7373682e636f6d2c686d61632d736861322d3235362c686d61632d736861322d3531322c686d61632d73686131000000d5756d61632d36342d65746d406f70656e7373682e636f6d2c756d61632d3132382d65746d406f70656e7373682e636f6d2c686d61632d736861322d3235362d65746d406f70656e7373682e636f6d2c686d61632d736861322d3531322d65746d406f70656e7373682e636f6d2c686d61632d736861312d65746d406f70656e7373682e636f6d2c756d61632d3634406f70656e7373682e636f6d2c756d61632d313238406f70656e7373682e636f6d2c686d61632d736861322d3235362c686d61632d736861322d3531322c686d61632d736861310000001a6e6f6e652c7a6c6962406f70656e7373682e636f6d2c7a6c69620000001a6e6f6e652c7a6c6962406f70656e7373682e636f6d2c7a6c69620000000000000000000000000000000000000000"
                ),
                server: &hex!(
                    "000000bc051414a204a54b2f5fa7ff5313675767bc5500000011637572766532353531392d7368613235360000000b7373682d656432353531390000001d63686163686132302d706f6c7931333035406f70656e7373682e636f6d0000001d63686163686132302d706f6c7931333035406f70656e7373682e636f6d0000000d686d61632d736861322d3235360000000d686d61632d736861322d323536000000046e6f6e65000000046e6f6e65000000000000000000000000000000000000"
                ),
            },
            // ECDH KEX Init
            Part {
                client: &hex!(
                    "0000002c061e000000204c646d1281abf23264d63db96e05c0223cfead668d9d38c62579b8856e67ae19000000000000"
                ),
                server: &hex!(
                    "000000bc081f000000330000000b7373682d6564323535313900000020e939cdfa6fc0d737333b534e913dd332c8d5179fe00c3045575217224b19b8f6000000204260e2c5e5383f1a021c9631fa61f60f305b29183fd219d4c8207c664e063410000000530000000b7373682d65643235353139000000406504a045499f26aa4ee17606ea6bd9e3f288838591f25d8604a63f77a52f5b9e909c00d10f386553e585d86ab329bbde0fca5c64b1b1982d7adcac17cf7f06010000000000000000"
                ),
            },
            // New Keys
            Part {
                client: &hex!("0000000c0a1500000000000000000000"),
                server: &hex!("0000000c0a1500000000000000000000"),
            },
            // Service Request (encrypted)
            Part {
                client: &hex!("09ca4db7baeb24836a1f7d22368055bf4c26981ed86738ac7a5c31d0730ad656f1967853781dff91ee1c4de8"),
                server: &hex!("7b444c0d5faf740d350701a054ea469fab1c98e4b669e4872a454163edb42ec5e4fa95c404ab601f016bd259"),
            },
        ];

        let mut con = ServerConnection::new(HardcodedRng(rng), ServerConfig::default());
        for part in conversation {
            con.recv_bytes(&part.client).unwrap();
            eprintln!("client: {:x?}", part.client);
            let bytes = con.next_msg_to_send().unwrap().to_bytes();
            if part.server != bytes {
                panic!(
                    "expected != found\nexpected: {:x?}\nfound:    {:x?}",
                    part.server, bytes
                );
            }
        }
    }
}
