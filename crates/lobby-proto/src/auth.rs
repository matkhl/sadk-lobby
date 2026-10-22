//! Login key exchange (tincat3 "AuthenticateSession").
//!
//! 1. Client → `StartAuthenticateSession { key }`: DER
//!    `SEQUENCE { BIT STRING, INTEGER, INTEGER x, INTEGER y }`, an ephemeral
//!    secp521r1 public key.
//! 2. Server → `AckAuthenticateSession { cipher }`: DER
//!    `SEQUENCE { OID sha512, OCTET STRING(server key, same layout), OCTET STRING(masked) }`
//!    where `masked = K xor SHA-512(ecdh_x)[..32]` and `K` is a random
//!    32-byte Twofish key chosen by the server.
//! 3. Client → `AuthenticateUser` / `SelfRegistration { cipher }`:
//!    `iv(16) || Twofish-CTR_K(credentials)`.
//! 4. Server → `SessionKey { perm_id, cipher }`: `iv(16) || Twofish-CTR_K(session key)`.
//!
//! The CTR counter starts at the IV and increments as a little-endian number.

use p521::ecdh::EphemeralSecret;
use p521::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p521::{EncodedPoint, FieldBytes, PublicKey};
use sha2::{Digest, Sha512};
use twofish::cipher::generic_array::GenericArray;
use twofish::cipher::{BlockEncrypt, KeyInit};
use twofish::Twofish;

pub const KEY_LEN: usize = 32;
pub const IV_LEN: usize = 16;
const COORD_LEN: usize = 66;
/// OID 2.16.840.1.101.3.4.2.3 (SHA-512).
const SHA512_OID: [u8; 9] = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    BadDer(&'static str),
    BadPoint,
    Random,
    BadCredentials,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadDer(what) => write!(f, "malformed DER ({what})"),
            Self::BadPoint => write!(f, "client key is not a secp521r1 point"),
            Self::Random => write!(f, "system random generator failed"),
            Self::BadCredentials => write!(f, "malformed credential block"),
        }
    }
}

impl std::error::Error for AuthError {}

// --- minimal DER -----------------------------------------------------------

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else if n < 0x100 {
        out.extend([0x81, n as u8]);
    } else {
        out.extend([0x82, (n >> 8) as u8, n as u8]);
    }
    out.extend_from_slice(content);
    out
}

/// Unsigned big-endian number as a DER INTEGER.
fn der_uint(be: &[u8]) -> Vec<u8> {
    let start = be.iter().position(|&b| b != 0).unwrap_or(be.len().saturating_sub(1));
    let mut content = be[start..].to_vec();
    if content.first().is_none_or(|&b| b & 0x80 != 0) {
        content.insert(0, 0);
    }
    der(0x02, &content)
}

/// Reads one TLV; returns (tag, content, rest).
fn read_tlv(buf: &[u8]) -> Result<(u8, &[u8], &[u8]), AuthError> {
    let (&tag, rest) = buf.split_first().ok_or(AuthError::BadDer("empty"))?;
    let (&first, mut rest) = rest.split_first().ok_or(AuthError::BadDer("no length"))?;
    let len = match first {
        n if n < 0x80 => n as usize,
        0x81 | 0x82 => {
            let count = (first & 0x7f) as usize;
            if rest.len() < count {
                return Err(AuthError::BadDer("short length"));
            }
            let len = rest[..count].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
            rest = &rest[count..];
            len
        }
        _ => return Err(AuthError::BadDer("unsupported length form")),
    };
    if rest.len() < len {
        return Err(AuthError::BadDer("content past end"));
    }
    Ok((tag, &rest[..len], &rest[len..]))
}

/// `SEQUENCE { BIT STRING 07 00, INTEGER 0x41, INTEGER x, INTEGER y }`.
fn encode_public_key(key: &PublicKey) -> Vec<u8> {
    let point = key.to_encoded_point(false);
    let mut body = der(0x03, &[0x07, 0x00]);
    body.extend(der(0x02, &[0x41]));
    body.extend(der_uint(point.x().expect("uncompressed point")));
    body.extend(der_uint(point.y().expect("uncompressed point")));
    der(0x30, &body)
}

fn coordinate(int_content: &[u8]) -> Result<FieldBytes, AuthError> {
    let start = int_content.iter().position(|&b| b != 0).unwrap_or(int_content.len());
    let digits = &int_content[start..];
    if digits.len() > COORD_LEN {
        return Err(AuthError::BadPoint);
    }
    let mut out = [0u8; COORD_LEN];
    out[COORD_LEN - digits.len()..].copy_from_slice(digits);
    Ok(*FieldBytes::from_slice(&out))
}

fn decode_public_key(der_key: &[u8]) -> Result<PublicKey, AuthError> {
    let (tag, mut items, _) = read_tlv(der_key)?;
    if tag != 0x30 {
        return Err(AuthError::BadDer("key is not a SEQUENCE"));
    }
    let mut ints = Vec::new();
    while !items.is_empty() {
        let (tag, content, rest) = read_tlv(items)?;
        if tag == 0x02 {
            ints.push(content);
        }
        items = rest;
    }
    // INTEGER key size, then x and y.
    let [_, x, y] = ints[..] else { return Err(AuthError::BadDer("expected three INTEGERs")) };
    let point = EncodedPoint::from_affine_coordinates(&coordinate(x)?, &coordinate(y)?, false);
    Option::from(PublicKey::from_encoded_point(&point)).ok_or(AuthError::BadPoint)
}

fn random<const N: usize>() -> Result<[u8; N], AuthError> {
    let mut out = [0u8; N];
    getrandom::getrandom(&mut out).map_err(|_| AuthError::Random)?;
    Ok(out)
}

/// Mask derived from the ECDH x coordinate (66 bytes, big-endian).
fn mask(shared_x: &[u8]) -> [u8; KEY_LEN] {
    let digest = Sha512::digest(shared_x);
    digest[..KEY_LEN].try_into().unwrap()
}

/// Server side of step 2. Returns the `AckAuthenticateSession` cipher block and
/// the Twofish key `K` for this connection.
pub fn acknowledge(client_key: &[u8]) -> Result<(Vec<u8>, [u8; KEY_LEN]), AuthError> {
    let client = decode_public_key(client_key)?;
    let secret = EphemeralSecret::random(&mut p521::elliptic_curve::rand_core::OsRng);
    let shared = secret.diffie_hellman(&client);
    let key: [u8; KEY_LEN] = random()?;
    let m = mask(shared.raw_secret_bytes());
    let masked: Vec<u8> = key.iter().zip(m).map(|(k, m)| k ^ m).collect();

    let mut body = der(0x06, &SHA512_OID);
    body.extend(der(0x04, &encode_public_key(&secret.public_key())));
    body.extend(der(0x04, &masked));
    Ok((der(0x30, &body), key))
}

/// Client side of the key exchange (steps 1 and 2), for test clients and bots.
pub struct ClientExchange {
    secret: EphemeralSecret,
}

impl ClientExchange {
    /// A fresh key pair and the `StartAuthenticateSession` key block.
    pub fn start() -> (Self, Vec<u8>) {
        let secret = EphemeralSecret::random(&mut p521::elliptic_curve::rand_core::OsRng);
        let hello = encode_public_key(&secret.public_key());
        (Self { secret }, hello)
    }

    /// Recovers the Twofish key `K` from the `AckAuthenticateSession` cipher.
    pub fn finish(&self, ack: &[u8]) -> Result<[u8; KEY_LEN], AuthError> {
        let (tag, body, _) = read_tlv(ack)?;
        if tag != 0x30 {
            return Err(AuthError::BadDer("ack is not a SEQUENCE"));
        }
        let (_, _oid, rest) = read_tlv(body)?;
        let (_, server_key, rest) = read_tlv(rest)?;
        let (_, masked, _) = read_tlv(rest)?;
        let masked: [u8; KEY_LEN] = masked.try_into().map_err(|_| AuthError::BadDer("masked key length"))?;
        let shared = self.secret.diffie_hellman(&decode_public_key(server_key)?);
        let m = mask(shared.raw_secret_bytes());
        Ok(std::array::from_fn(|i| masked[i] ^ m[i]))
    }
}

/// Twofish-256 in CTR mode with a little-endian counter (encrypt = decrypt).
pub fn ctr(key: &[u8; KEY_LEN], iv: &[u8; IV_LEN], data: &mut [u8]) {
    let cipher = Twofish::new_from_slice(key).expect("32-byte key");
    let mut counter = *iv;
    for chunk in data.chunks_mut(IV_LEN) {
        let mut block = GenericArray::clone_from_slice(&counter);
        cipher.encrypt_block(&mut block);
        for (d, k) in chunk.iter_mut().zip(block) {
            *d ^= k;
        }
        for b in counter.iter_mut() {
            *b = b.wrapping_add(1);
            if *b != 0 {
                break;
            }
        }
    }
}

/// `iv || ciphertext` → plaintext.
pub fn open(key: &[u8; KEY_LEN], sealed: &[u8]) -> Option<Vec<u8>> {
    let iv: [u8; IV_LEN] = sealed.get(..IV_LEN)?.try_into().ok()?;
    let mut data = sealed[IV_LEN..].to_vec();
    ctr(key, &iv, &mut data);
    Some(data)
}

/// plaintext → `iv || ciphertext` with a fresh random IV.
pub fn seal(key: &[u8; KEY_LEN], plain: &[u8]) -> Result<Vec<u8>, AuthError> {
    let iv: [u8; IV_LEN] = random()?;
    let mut data = plain.to_vec();
    ctr(key, &iv, &mut data);
    let mut out = iv.to_vec();
    out.extend(data);
    Ok(out)
}

pub fn new_session_key() -> Result<[u8; KEY_LEN], AuthError> {
    random()
}

/// Decrypted login/registration block: `u8 len, name, u8 len, password`,
/// and for registration `u8 key count, u8 keypool, u8 len, cd key`.
#[derive(Debug, PartialEq, Eq)]
pub struct Credentials {
    pub name: Vec<u8>,
    pub password: Vec<u8>,
    pub cd_key: Option<(u8, Vec<u8>)>,
}

impl Credentials {
    pub fn parse(plain: &[u8], registration: bool) -> Result<Self, AuthError> {
        let mut rest = plain;
        let mut take = |n: usize| -> Result<&[u8], AuthError> {
            if rest.len() < n {
                return Err(AuthError::BadCredentials);
            }
            let (head, tail) = rest.split_at(n);
            rest = tail;
            Ok(head)
        };
        let n = take(1)?[0] as usize;
        let name = take(n)?.to_vec();
        let n = take(1)?[0] as usize;
        let password = take(n)?.to_vec();
        let cd_key = if registration {
            let header = take(3)?;
            let (count, pool, len) = (header[0], header[1], header[2] as usize);
            if count != 1 {
                return Err(AuthError::BadCredentials);
            }
            Some((pool, take(len)?.to_vec()))
        } else {
            None
        };
        Ok(Self { name, password, cd_key })
    }

    /// The block as a client builds it (the inverse of `parse`).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.name.len() as u8];
        out.extend(&self.name);
        out.push(self.password.len() as u8);
        out.extend(&self.password);
        if let Some((pool, key)) = &self.cd_key {
            out.extend([1, *pool, key.len() as u8]);
            out.extend(key);
        }
        out
    }
}

/// Decrypted `SendToken` cipher (tincat3 `Authenticator.GenerateToken`):
/// `u32 id, u8 len, name, u8 len, cd key hash, u32 value, u8 len, server
/// password, u8 len, nonce`, then 128 bytes of salt. The client seals it
/// like the credentials (`iv || CTR`), with the session key from `SessionKey`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub id: u32,
    pub name: Vec<u8>,
    pub key_hash: Vec<u8>,
    pub value: u32,
    pub password: Vec<u8>,
    pub nonce: Vec<u8>,
}

impl Token {
    pub fn parse(plain: &[u8]) -> Option<Self> {
        let mut rest = plain;
        let mut take = |n: usize| -> Option<&[u8]> {
            let (head, tail) = rest.split_at_checked(n)?;
            rest = tail;
            Some(head)
        };
        let id = u32::from_le_bytes(take(4)?.try_into().ok()?);
        let n = take(1)?[0] as usize;
        let name = take(n)?.to_vec();
        let n = take(1)?[0] as usize;
        let key_hash = take(n)?.to_vec();
        let value = u32::from_le_bytes(take(4)?.try_into().ok()?);
        let n = take(1)?[0] as usize;
        let password = take(n)?.to_vec();
        let n = take(1)?[0] as usize;
        let nonce = take(n)?.to_vec();
        Some(Self { id, name, key_hash, value, password, nonce })
    }

    /// The token as tincat builds it (the inverse of `parse`), sealed with
    /// the session key, with a random salt.
    pub fn seal(&self, session_key: &[u8; KEY_LEN]) -> Result<Vec<u8>, AuthError> {
        let mut plain = self.id.to_le_bytes().to_vec();
        for field in [&self.name, &self.key_hash] {
            plain.push(field.len() as u8);
            plain.extend(field);
        }
        plain.extend(self.value.to_le_bytes());
        for field in [&self.password, &self.nonce] {
            plain.push(field.len() as u8);
            plain.extend(field);
        }
        plain.extend(random::<128>()?);
        seal(session_key, &plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays the client: send a key, read the ack, recover K, send credentials.
    #[test]
    fn client_and_server_agree() {
        let (client, hello) = ClientExchange::start();
        let (ack, server_key) = acknowledge(&hello).unwrap();
        assert_eq!(client.finish(&ack).unwrap(), server_key);

        let creds = Credentials { name: b"siedler".to_vec(), password: b"test123".to_vec(), cd_key: None };
        let sealed = seal(&server_key, &creds.encode()).unwrap();
        assert_ne!(&sealed[IV_LEN..], &creds.encode()[..]);
        assert_eq!(Credentials::parse(&open(&server_key, &sealed).unwrap(), false).unwrap(), creds);
    }

    #[test]
    fn counter_is_little_endian() {
        // Two blocks with IV ff..ff 00: the second block must use 00..00 01,
        // i.e. byte 0 wraps and carries into byte 1.
        let key = [3u8; KEY_LEN];
        let mut iv = [0u8; IV_LEN];
        iv[0] = 0xff;
        let mut both = [0u8; 32];
        ctr(&key, &iv, &mut both);
        let mut next = [0u8; IV_LEN];
        next[1] = 1;
        let mut second = [0u8; 16];
        ctr(&key, &next, &mut second);
        assert_eq!(&both[16..], &second);
    }

    #[test]
    fn parses_registration_block() {
        let mut plain = vec![2, b'a', b'b', 1, b'x', 1, 1, 4];
        plain.extend(b"KEY1");
        let creds = Credentials::parse(&plain, true).unwrap();
        assert_eq!(creds.cd_key, Some((1, b"KEY1".to_vec())));
        assert_eq!(Credentials::parse(&plain[..4], true), Err(AuthError::BadCredentials));
    }

    #[test]
    fn opens_and_parses_a_token() {
        let key = [7u8; KEY_LEN];
        let mut plain = 1u32.to_le_bytes().to_vec();
        plain.push(7);
        plain.extend(b"siedler");
        plain.push(64);
        plain.extend([0xab; 64]);
        plain.extend(1u32.to_le_bytes());
        plain.push(0);
        plain.push(16);
        plain.extend([5; 16]);
        plain.extend([0; 128]);
        let token = Token::parse(&open(&key, &seal(&key, &plain).unwrap()).unwrap()).unwrap();
        let expected = Token { id: 1, name: b"siedler".to_vec(), key_hash: vec![0xab; 64], value: 1, password: vec![], nonce: vec![5; 16] };
        assert_eq!(token, expected);
        let resealed = expected.seal(&key).unwrap();
        assert_eq!(open(&key, &resealed).unwrap().len(), plain.len());
        assert_eq!(Token::parse(&open(&key, &resealed).unwrap()).unwrap(), expected);
        assert_eq!(Token::parse(&plain[..20]), None);
    }

    #[test]
    fn der_integers_are_minimal_and_positive() {
        assert_eq!(der_uint(&[0, 0, 0x7f]), vec![2, 1, 0x7f]);
        assert_eq!(der_uint(&[0, 0x80]), vec![2, 2, 0, 0x80]);
        assert_eq!(der_uint(&[0, 0]), vec![2, 1, 0]);
    }
}
