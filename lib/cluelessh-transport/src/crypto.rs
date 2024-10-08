pub mod encrypt;

use cluelessh_keys::{public::PublicKey, signature::Signature};
use p256::ecdsa::signature::Verifier;
use secrecy::ExposeSecret;
use sha2::Digest;

use crate::{
    packet::{EncryptedPacket, MsgKind, Packet, RawPacket},
    peer_error, Msg, Result, SessionId, SshRng,
};

pub type SharedSecret = secrecy::Secret<SharedSecretInner>;

#[derive(Clone)]
pub struct SharedSecretInner(pub Vec<u8>);
impl secrecy::Zeroize for SharedSecretInner {
    fn zeroize(&mut self) {
        secrecy::Zeroize::zeroize(&mut self.0);
    }
}
impl secrecy::CloneableSecret for SharedSecretInner {}

pub trait AlgorithmName {
    fn name(&self) -> &'static str;
}

// Dummy algorithm.
impl AlgorithmName for &'static str {
    fn name(&self) -> &'static str {
        self
    }
}

#[derive(Clone, Copy)]
pub struct KexAlgorithm {
    name: &'static str,
    /// Generate an ephemeral key for the exchange.
    pub generate_secret: fn(random: &mut (dyn SshRng + Send + Sync)) -> KeyExchangeSecret,
}
impl AlgorithmName for KexAlgorithm {
    fn name(&self) -> &'static str {
        self.name
    }
}

pub struct KeyExchangeSecret {
    /// Q_x
    pub pubkey: Vec<u8>,
    /// Does the exchange, returning the shared secret K.
    pub exchange: Box<dyn FnOnce(&[u8]) -> Result<SharedSecret> + Send + Sync>,
}

pub fn kex_algorithm_by_name(name: &str) -> Option<KexAlgorithm> {
    match name {
        "curve25519-sha256" => Some(KEX_CURVE_25519_SHA256),
        "ecdh-sha2-nistp256" => Some(KEX_ECDH_SHA2_NISTP256),
        _ => None,
    }
}

/// <https://datatracker.ietf.org/doc/html/rfc8731>
pub const KEX_CURVE_25519_SHA256: KexAlgorithm = KexAlgorithm {
    name: "curve25519-sha256",
    generate_secret: |rng| {
        let secret = x25519_dalek::EphemeralSecret::random_from_rng(crate::SshRngRandAdapter(rng));
        let my_public_key = x25519_dalek::PublicKey::from(&secret);

        KeyExchangeSecret {
            pubkey: my_public_key.as_bytes().to_vec(),
            exchange: Box::new(move |peer_public_key| {
                let Ok(peer_public_key) = <[u8; 32]>::try_from(peer_public_key) else {
                    return Err(crate::peer_error!(
                        "invalid x25519 public key length, should be 32, was: {}",
                        peer_public_key.len()
                    ));
                };
                let peer_public_key = x25519_dalek::PublicKey::from(peer_public_key);
                let shared_secret = secret.diffie_hellman(&peer_public_key); // K

                Ok(secrecy::Secret::new(SharedSecretInner(
                    shared_secret.as_bytes().to_vec(),
                )))
            }),
        }
    },
};
/// <https://datatracker.ietf.org/doc/html/rfc5656>
pub const KEX_ECDH_SHA2_NISTP256: KexAlgorithm = KexAlgorithm {
    name: "ecdh-sha2-nistp256",
    generate_secret: |rng| {
        let secret = p256::ecdh::EphemeralSecret::random(&mut crate::SshRngRandAdapter(rng));
        let my_public_key = p256::EncodedPoint::from(secret.public_key());

        KeyExchangeSecret {
            pubkey: my_public_key.as_bytes().to_vec(),
            exchange: Box::new(move |peer_public_key| {
                let peer_public_key =
                    p256::PublicKey::from_sec1_bytes(peer_public_key).map_err(|_| {
                        crate::peer_error!(
                            "invalid p256 public key length: {}",
                            peer_public_key.len()
                        )
                    })?;

                let shared_secret = secret.diffie_hellman(&peer_public_key); // K

                Ok(secrecy::Secret::new(SharedSecretInner(
                    shared_secret.raw_secret_bytes().to_vec(),
                )))
            }),
        }
    },
};

#[derive(Clone, Copy)]
pub struct EncryptionAlgorithm {
    name: &'static str,
    iv_size: usize,
    key_size: usize,
    decrypt_len: fn(state: &mut [u8], bytes: &mut [u8], packet_number: u64),
    decrypt_packet: fn(state: &mut [u8], bytes: RawPacket, packet_number: u64) -> Result<Packet>,
    encrypt_packet: fn(state: &mut [u8], packet: Packet, packet_number: u64) -> EncryptedPacket,
}
impl AlgorithmName for EncryptionAlgorithm {
    fn name(&self) -> &'static str {
        self.name
    }
}
pub struct EncodedSshSignature(pub Vec<u8>);

#[derive(Clone)]
pub struct HostKeySigningAlgorithm {
    public_key: PublicKey,
}

impl AlgorithmName for HostKeySigningAlgorithm {
    fn name(&self) -> &'static str {
        self.public_key.algorithm_name()
    }
}

impl HostKeySigningAlgorithm {
    pub fn new(public_key: PublicKey) -> Self {
        Self { public_key }
    }
    pub fn public_key(&self) -> PublicKey {
        self.public_key.clone()
    }
}

pub struct HostKeyVerifyAlgorithm {
    name: &'static str,
    pub verify:
        fn(public_key: &[u8], message: &[u8], signature: &EncodedSshSignature) -> Result<()>,
}

impl AlgorithmName for HostKeyVerifyAlgorithm {
    fn name(&self) -> &'static str {
        self.name
    }
}

const HOSTKEY_VERIFY_ED25519: HostKeyVerifyAlgorithm = HostKeyVerifyAlgorithm {
    name: "ssh-ed25519",
    verify: |public_key, message, signature| {
        let public_key = PublicKey::from_wire_encoding(public_key)
            .map_err(|err| peer_error!("incorrect public host key: {err}"))?;
        let PublicKey::Ed25519 { public_key } = public_key else {
            return Err(peer_error!("incorrect algorithm public host key"));
        };

        let signature = Signature::from_wire_encoding(&signature.0)
            .map_err(|err| peer_error!("incorrect signature: {err}"))?;
        let Signature::Ed25519 { signature } = signature else {
            return Err(peer_error!("incorrect algorithm for signature"));
        };

        public_key
            .verify_strict(message, &signature)
            .map_err(|err| peer_error!("incorrect signature: {err}"))
    },
};
const HOSTKEY_VERIFY_ECDSA_SHA2_NISTP256: HostKeyVerifyAlgorithm = HostKeyVerifyAlgorithm {
    name: "ecdsa-sha2-nistp256",
    verify: |public_key, message, signature| {
        let public_key = PublicKey::from_wire_encoding(public_key)
            .map_err(|err| peer_error!("incorrect public host key: {err}"))?;

        dbg!(&public_key);
        let PublicKey::EcdsaSha2NistP256 { public_key } = public_key else {
            return Err(peer_error!("incorrect algorithm for public host key"));
        };

        let signature = Signature::from_wire_encoding(&signature.0)
            .map_err(|err| peer_error!("incorrect signature: {err}"))?;
        let Signature::EcdsaSha2NistP256 { signature } = signature else {
            return Err(peer_error!("incorrect algorithm for signature"));
        };

        public_key
            .verify(message, &signature)
            .map_err(|err| peer_error!("incorrect signature: {err}"))
    },
};
pub struct AlgorithmNegotiation<T> {
    pub supported: Vec<T>,
}

impl<T: AlgorithmName> AlgorithmNegotiation<T> {
    pub fn to_name_list(&self) -> String {
        self.supported
            .iter()
            .map(|alg| alg.name())
            .collect::<Vec<&str>>()
            .join(",")
    }

    pub fn find(mut self, this_is_client: bool, peer_supports: &str) -> Result<T> {
        // <https://datatracker.ietf.org/doc/html/rfc4253#section-7.1>
        // We let the client guide the algorithm search.

        let my_algs = self
            .supported
            .iter()
            .map(|alg| alg.name())
            .collect::<Vec<_>>();
        let peer_algs = peer_supports.split(',').collect();

        let (client_algs, server_algs) = if this_is_client {
            (my_algs, peer_algs)
        } else {
            (peer_algs, my_algs)
        };

        for alg_name in client_algs {
            if server_algs.iter().any(|peer| *peer == alg_name) {
                // Algorithm is supported
                if let Some(alg) = self.supported.iter().position(|alg| alg.name() == alg_name) {
                    return Ok(self.supported.remove(alg));
                }
            }
        }

        let we_support = self
            .supported
            .iter()
            .map(|alg| alg.name())
            .collect::<Vec<_>>()
            .join(",");

        Err(peer_error!(
            "peer does not support any matching algorithm: we support: {we_support:?}, peer supports: {peer_supports:?}"
        ))
    }
}

pub struct SupportedAlgorithms {
    pub key_exchange: AlgorithmNegotiation<KexAlgorithm>,
    pub hostkey_sign: AlgorithmNegotiation<HostKeySigningAlgorithm>,
    pub hostkey_verify: AlgorithmNegotiation<HostKeyVerifyAlgorithm>,
    pub encryption_to_peer: AlgorithmNegotiation<EncryptionAlgorithm>,
    pub encryption_from_peer: AlgorithmNegotiation<EncryptionAlgorithm>,
    pub mac_to_peer: AlgorithmNegotiation<&'static str>,
    pub mac_from_peer: AlgorithmNegotiation<&'static str>,
    pub compression_to_peer: AlgorithmNegotiation<&'static str>,
    pub compression_from_peer: AlgorithmNegotiation<&'static str>,
}

impl SupportedAlgorithms {
    /// A secure default using elliptic curves and AEAD.
    pub fn secure(host_keys: &[PublicKey]) -> Self {
        let supported_host_keys = host_keys
            .iter()
            .map(|key| HostKeySigningAlgorithm::new(key.clone()))
            .collect();

        Self {
            key_exchange: AlgorithmNegotiation {
                supported: vec![KEX_CURVE_25519_SHA256, KEX_ECDH_SHA2_NISTP256],
            },
            hostkey_sign: AlgorithmNegotiation {
                supported: supported_host_keys,
            },
            hostkey_verify: AlgorithmNegotiation {
                supported: vec![HOSTKEY_VERIFY_ECDSA_SHA2_NISTP256, HOSTKEY_VERIFY_ED25519],
            },
            encryption_to_peer: AlgorithmNegotiation {
                supported: vec![encrypt::CHACHA20POLY1305, encrypt::AES256_GCM],
            },
            encryption_from_peer: AlgorithmNegotiation {
                supported: vec![encrypt::CHACHA20POLY1305, encrypt::AES256_GCM],
            },
            mac_to_peer: AlgorithmNegotiation {
                supported: vec!["hmac-sha2-256", "hmac-sha2-256-etm@openssh.com"],
            },
            mac_from_peer: AlgorithmNegotiation {
                supported: vec!["hmac-sha2-256", "hmac-sha2-256-etm@openssh.com"],
            },
            compression_to_peer: AlgorithmNegotiation {
                supported: vec!["none"],
            },
            compression_from_peer: AlgorithmNegotiation {
                supported: vec!["none"],
            },
        }
    }
}

pub(crate) struct Session {
    session_id: SessionId,
    from_peer: Tunnel,
    to_peer: Tunnel,
}

struct Tunnel {
    /// `key || IV`
    state: Vec<u8>,
    algorithm: EncryptionAlgorithm,
}

pub(crate) trait Keys: Send + Sync + 'static {
    fn decrypt_len(&mut self, bytes: &mut [u8; 4], packet_number: u64);
    fn decrypt_packet(&mut self, raw_packet: RawPacket, packet_number: u64) -> Result<Packet>;

    fn encrypt_packet_to_msg(&mut self, packet: Packet, packet_number: u64) -> Msg;

    fn additional_mac_len(&self) -> usize;
    // TODO: actually rekey...
    fn rekey(
        &mut self,
        h: [u8; 32],
        k: &SharedSecret,
        encryption_client_to_server: EncryptionAlgorithm,
        encryption_server_to_client: EncryptionAlgorithm,
        is_server: bool,
    ) -> Result<(), ()>;
}

pub(crate) struct Plaintext;
impl Keys for Plaintext {
    fn decrypt_len(&mut self, _: &mut [u8; 4], _: u64) {}
    fn decrypt_packet(&mut self, raw: RawPacket, _: u64) -> Result<Packet> {
        Packet::from_full(raw.rest())
    }
    fn encrypt_packet_to_msg(&mut self, packet: Packet, _: u64) -> Msg {
        Msg(MsgKind::PlaintextPacket(packet))
    }
    fn additional_mac_len(&self) -> usize {
        0
    }
    fn rekey(
        &mut self,
        _: [u8; 32],
        _: &SharedSecret,
        _: EncryptionAlgorithm,
        _: EncryptionAlgorithm,
        _: bool,
    ) -> Result<(), ()> {
        Err(())
    }
}

impl Session {
    pub(crate) fn new(
        h: SessionId,
        k: &SharedSecret,
        encryption_client_to_server: EncryptionAlgorithm,
        encryption_server_to_client: EncryptionAlgorithm,
        is_server: bool,
    ) -> Self {
        Self::from_keys(
            h,
            h.0,
            k,
            encryption_client_to_server,
            encryption_server_to_client,
            is_server,
        )
    }

    /// <https://datatracker.ietf.org/doc/html/rfc4253#section-7.2>
    fn from_keys(
        session_id: SessionId,
        h: [u8; 32],
        k: &SharedSecret,
        alg_c2s: EncryptionAlgorithm,
        alg_s2c: EncryptionAlgorithm,
        is_server: bool,
    ) -> Self {
        let c2s = Tunnel {
            algorithm: alg_c2s,
            state: {
                let mut state = derive_key(k, h, "C", session_id, alg_c2s.key_size);
                let iv = derive_key(k, h, "A", session_id, alg_c2s.iv_size);
                state.extend_from_slice(&iv);
                state
            },
        };
        let s2c = Tunnel {
            algorithm: alg_s2c,
            state: {
                let mut state = derive_key(k, h, "D", session_id, alg_s2c.key_size);
                state.extend_from_slice(&derive_key(k, h, "B", session_id, alg_s2c.iv_size));
                state
            },
        };

        let (from_peer, to_peer) = if is_server { (c2s, s2c) } else { (s2c, c2s) };

        Self {
            session_id,
            from_peer,
            to_peer,
            // integrity_key_client_to_server: derive("E").into(),
            // integrity_key_server_to_client: derive("F").into(),
        }
    }
}

impl Keys for Session {
    fn decrypt_len(&mut self, bytes: &mut [u8; 4], packet_number: u64) {
        (self.from_peer.algorithm.decrypt_len)(&mut self.from_peer.state, bytes, packet_number);
    }

    fn decrypt_packet(&mut self, bytes: RawPacket, packet_number: u64) -> Result<Packet> {
        (self.from_peer.algorithm.decrypt_packet)(&mut self.from_peer.state, bytes, packet_number)
    }

    fn encrypt_packet_to_msg(&mut self, packet: Packet, packet_number: u64) -> Msg {
        let packet =
            (self.to_peer.algorithm.encrypt_packet)(&mut self.to_peer.state, packet, packet_number);
        Msg(MsgKind::EncryptedPacket(packet))
    }

    fn additional_mac_len(&self) -> usize {
        poly1305::BLOCK_SIZE
    }

    fn rekey(
        &mut self,
        h: [u8; 32],
        k: &SharedSecret,
        encryption_client_to_server: EncryptionAlgorithm,
        encryption_server_to_client: EncryptionAlgorithm,
        is_server: bool,
    ) -> Result<(), ()> {
        *self = Self::from_keys(
            self.session_id,
            h,
            k,
            encryption_client_to_server,
            encryption_server_to_client,
            is_server,
        );
        Ok(())
    }
}

/// Derive a key from the shared secret K and exchange hash H.
/// <https://datatracker.ietf.org/doc/html/rfc4253#section-7.2>
fn derive_key(
    k: &SharedSecret,
    h: [u8; 32],
    letter: &str,
    session_id: SessionId,
    key_size: usize,
) -> Vec<u8> {
    let sha2len = sha2::Sha256::output_size();
    let padded_key_size = key_size.next_multiple_of(sha2len);
    let mut output = vec![0; padded_key_size];

    for i in 0..(padded_key_size / sha2len) {
        let mut hash = <sha2::Sha256 as sha2::Digest>::new();
        encode_mpint_for_hash(k.expose_secret().0.as_slice(), |data| hash.update(data));
        hash.update(h);

        if i == 0 {
            hash.update(letter.as_bytes());
            hash.update(session_id.0);
        } else {
            hash.update(&output[..(i * sha2len)]);
        }

        output[(i * sha2len)..][..sha2len].copy_from_slice(&hash.finalize())
    }

    output.truncate(key_size);
    output
}

pub(crate) fn encode_mpint_for_hash(key: &[u8], mut add_to_hash: impl FnMut(&[u8])) {
    let (key, pad_zero) = cluelessh_format::fixup_mpint(key);
    add_to_hash(&u32::to_be_bytes((key.len() + (pad_zero as usize)) as u32));
    if pad_zero {
        add_to_hash(&[0]);
    }
    add_to_hash(key);
}

pub fn key_exchange_hash(
    client_ident: &[u8],
    server_ident: &[u8],
    client_kexinit: &[u8],
    server_kexinit: &[u8],
    server_hostkey: &[u8],
    eph_client_public_key: &[u8],
    eph_server_public_key: &[u8],
    shared_secret: &SharedSecret,
) -> [u8; 32] {
    let mut hash = sha2::Sha256::new();
    let add_hash = |hash: &mut sha2::Sha256, bytes: &[u8]| {
        hash.update(bytes);
    };
    let hash_string = |hash: &mut sha2::Sha256, bytes: &[u8]| {
        add_hash(hash, &u32::to_be_bytes(bytes.len() as u32));
        add_hash(hash, bytes);
    };
    let hash_mpint = |hash: &mut sha2::Sha256, bytes: &[u8]| {
        encode_mpint_for_hash(bytes, |data| add_hash(hash, data));
    };

    // Strip the \r\n
    hash_string(&mut hash, &client_ident[..(client_ident.len() - 2)]); // V_C
    hash_string(&mut hash, &server_ident[..(server_ident.len() - 2)]); // V_S

    hash_string(&mut hash, client_kexinit); // I_C
    hash_string(&mut hash, server_kexinit); // I_S
    hash_string(&mut hash, server_hostkey); // K_S

    // For normal DH as in RFC4253, e and f are mpints.
    // But for ECDH as defined in RFC5656, Q_C and Q_S are strings.
    // <https://datatracker.ietf.org/doc/html/rfc5656#section-4>
    hash_string(&mut hash, eph_client_public_key); // Q_C
    hash_string(&mut hash, eph_server_public_key); // Q_S
    hash_mpint(&mut hash, shared_secret.expose_secret().0.as_slice()); // K

    let hash = hash.finalize();
    hash.into()
}

#[cfg(test)]
mod tests {
    use super::AlgorithmNegotiation;

    #[test]
    fn alg_negotation() {
        let server_algs = [
            "ssh-ed25519",
            "ecdsa-sha2-nistp256",
            "rsa-sha2-512,rsa-sha2-256",
        ];
        let client_algs = ["ssh-ed25519", "ecdsa-sha2-nistp256"];

        let we_are_client_negotiation = AlgorithmNegotiation {
            supported: client_algs.to_vec(),
        };

        let chosen = we_are_client_negotiation
            .find(
                true,
                &server_algs.iter().copied().collect::<Vec<&str>>().join(","),
            )
            .unwrap();
        assert_eq!(chosen, "ssh-ed25519");

        let we_are_server_negotiation = AlgorithmNegotiation {
            supported: server_algs.to_vec(),
        };
        let chosen = we_are_server_negotiation
            .find(
                true,
                &client_algs.iter().copied().collect::<Vec<&str>>().join(","),
            )
            .unwrap();
        assert_eq!(chosen, "ssh-ed25519");
    }
}
