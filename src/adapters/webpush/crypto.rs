/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Cryptographic operations for WebPush.
//!
//! Implemented as described in the draft IETF RFC:
//! https://tools.ietf.org/html/draft-ietf-webpush-encryption-02
//!

extern crate libc;
extern crate crypto;

use self::crypto::aead::AeadEncryptor;
use self::crypto::aes_gcm::AesGcm;
use self::crypto::aes::KeySize;
use self::crypto::hkdf::hkdf_extract;
use self::crypto::hmac::Hmac;
use self::crypto::sha2::Sha256;
use self::crypto::mac::Mac;

use std::ffi::{ CString, CStr };
use std::ptr;
use rand::Rng;
use rand::os::OsRng;

use rustc_serialize::base64::{ FromBase64, ToBase64, URL_SAFE };
use rustc_serialize::hex::{ ToHex, FromHex };

const NID_X9_62_PRIMVE256V1: libc::c_int = 415;

const EVP_PKEY_EC: libc::c_int = 408;
const EVP_PKEY_OP_PARAMGEN: libc::c_int = 2;
const EVP_PKEY_CTRL_EC_PARAMGEN_CURVE_NID: libc::c_int = 4097;

#[repr(C)]
enum EcPointConversion {
    Uncompressed = 4
}

type EcKey = libc::c_void;
type EcPoint = libc::c_void;
type EcGroup = libc::c_void;
type EvpPkey = libc::c_void;
type EvpPkeyCtx = libc::c_void;
type BnCtx = libc::c_void;

// TODO: switch to rust-openssl crate once the missing EC_*** and EVP_PKEY_*** APIs are added
//       instead of using the FFI directly
extern "C" {
    fn EC_KEY_new_by_curve_name(nid: libc::c_int) -> *mut EcKey;
    fn EC_KEY_free(key: *mut EcKey);
    fn EC_KEY_get0_group(key: *const EcKey) -> *const EcGroup;
    fn EC_KEY_set_public_key(key: *mut EcKey, pub_key: *const EcPoint) -> libc::c_int;
    fn EC_KEY_get0_public_key(key: *const EcKey) -> *mut EcPoint;

    fn EC_POINT_hex2point(group: *const EcGroup, hex: *const libc::c_char, p: *mut EcPoint, ctx: *mut BnCtx) -> *mut EcPoint;
    fn EC_POINT_point2hex(group: *const EcGroup, point: *const EcPoint, form: EcPointConversion, ctx: *mut BnCtx) -> *mut libc::c_char;
    fn EC_POINT_free(point: *mut EcPoint);

    fn EVP_PKEY_new() -> *mut EvpPkey;
    fn EVP_PKEY_free(pkey: *mut EvpPkey);
    fn EVP_PKEY_set1_EC_KEY(evpKey: *mut EvpPkey, ecKey: *mut EcKey) -> libc::c_int;
    fn EVP_PKEY_get1_EC_KEY(pkey: *mut EvpPkey) -> *mut EcKey;

    fn EVP_PKEY_CTX_new_id(id: libc::c_int, engine: *mut libc::c_void) -> *mut EvpPkeyCtx;
    fn EVP_PKEY_CTX_new(pkey: *mut EvpPkey, engine: *mut libc::c_void) -> *mut EvpPkeyCtx;
    fn EVP_PKEY_CTX_free(ctx: *mut EvpPkeyCtx);
    fn EVP_PKEY_CTX_ctrl(ctx: *mut EvpPkeyCtx, keytype: libc::c_int, optype: libc::c_int, cmd: libc::c_int, p1: libc::c_int, p2: *mut libc::c_void) -> libc::c_int;

    fn EVP_PKEY_paramgen_init(ctx: *mut EvpPkeyCtx) -> libc::c_int;
    fn EVP_PKEY_paramgen(ctx: *mut EvpPkeyCtx, ppkey: *mut *mut EvpPkey) -> libc::c_int;

    fn EVP_PKEY_keygen_init(ctx: *mut EvpPkeyCtx) -> libc::c_int;
    fn EVP_PKEY_keygen(ctx: *mut EvpPkeyCtx, ppkey: *mut *mut EvpPkey) -> libc::c_int;

    fn EVP_PKEY_derive_init(ctx: *mut EvpPkeyCtx) -> libc::c_int;
    fn EVP_PKEY_derive_set_peer(ctx: *mut EvpPkeyCtx, peer: *mut EvpPkey) -> libc::c_int;
    fn EVP_PKEY_derive(ctx: *mut EvpPkeyCtx, key: *mut libc::c_char, size: *mut libc::size_t) -> libc::c_int;

    fn CRYPTO_free(ptr: *mut libc::c_void);
}

/// Creates an OpenSSL representation of the given ECDH X9.62 public key,
/// represented as a string of hex digits.
fn ecdh_import_public_key(public_key : String) -> *mut EvpPkey {
    let eckey;
    let mut ecpoint = ptr::null_mut();
    let mut peer_key = ptr::null_mut();
    let native_key = CString::new(public_key).unwrap();

    unsafe {
        loop {
            eckey = EC_KEY_new_by_curve_name(NID_X9_62_PRIMVE256V1);
            if eckey.is_null() {
                warn!("cannot create EC X9.62 key");
                break;
            }

            let ecgroup = EC_KEY_get0_group(eckey);
            if ecgroup.is_null() {
                warn!("cannot get EC group from key");
                break;
            }

            ecpoint = EC_POINT_hex2point(ecgroup, native_key.as_ptr(), ptr::null_mut(), ptr::null_mut());
            if ecpoint.is_null() {
                warn!("cannot convert raw EC public key to EC point");
                break;
            }

            if EC_KEY_set_public_key(eckey, ecpoint) != 1 {
                warn!("cannot set EC public key");
                break;
            }

            peer_key = EVP_PKEY_new();
            if peer_key.is_null() {
                warn!("cannot create EVP pkey");
                break;
            }

            if EVP_PKEY_set1_EC_KEY(peer_key, eckey) != 1 {
                warn!("cannot initialize EVP pkey from EC key");
                EVP_PKEY_free(peer_key);
                peer_key = ptr::null_mut();
                break;
            }

            break;
        }

        if !eckey.is_null() { EC_KEY_free(eckey); }
        if !ecpoint.is_null() { EC_POINT_free(ecpoint); }
    }

    peer_key
}

/// Generates an OpenSSL representation of the parameters describing
/// an ECDH X9.62 key pair.
fn ecdh_generate_params() -> *mut EvpPkey {
    let ctx;
    let mut params = ptr::null_mut();

    unsafe {
        loop {
            ctx = EVP_PKEY_CTX_new_id(EVP_PKEY_EC, ptr::null_mut());
            if ctx.is_null() {
                warn!("cannot create param context");
                break;
            }

            if EVP_PKEY_paramgen_init(ctx) != 1 {
                warn!("cannot init param context");
                break;
            }

            if EVP_PKEY_CTX_ctrl(ctx, EVP_PKEY_EC, EVP_PKEY_OP_PARAMGEN, EVP_PKEY_CTRL_EC_PARAMGEN_CURVE_NID, NID_X9_62_PRIMVE256V1, ptr::null_mut()) != 1 {
                warn!("cannot set param context as X9.62 P256V1");
                break;
            }

            if EVP_PKEY_paramgen(ctx, &mut params) != 1 || params.is_null() {
                warn!("cannot generate params from context");
                break;
            }

            break;
        }

        if !ctx.is_null() { EVP_PKEY_CTX_free(ctx); }
    }

    params
}

/// Generates an OpenSSL representation of an ECDH X9.62 key pair.
fn ecdh_generate_key_pair() -> *mut EvpPkey {
    let params;
    let mut ctx = ptr::null_mut();
    let mut key = ptr::null_mut();

    unsafe {
        loop {
            params = ecdh_generate_params();
            if params.is_null() {
                break;
            }

            ctx = EVP_PKEY_CTX_new(params, ptr::null_mut());
            if ctx.is_null() {
                warn!("cannot create key context");
                break;
            }

            if EVP_PKEY_keygen_init(ctx) != 1 {
                warn!("cannot init key context");
                break;
            }

            if EVP_PKEY_keygen(ctx, &mut key) != 1 || key.is_null() {
                warn!("cannot generate public/private key pair from key context");
                break;
            }

            break;
        }

        if !ctx.is_null() { EVP_PKEY_CTX_free(ctx); }
        if !params.is_null() { EVP_PKEY_free(params); }
    }

    key
}

/// Derives a shared key from an ECDH X9.62 public/private key pair and
/// another (peer) ECDH X9.62 public key.
fn ecdh_derive_shared_key(local_key: *mut EvpPkey, peer_key: *mut EvpPkey) -> Option<Vec<u8>> {
    if local_key.is_null() || peer_key.is_null() {
        return None;
    }

    let mut status = None;
    let ctx;

    unsafe {
        loop {
            ctx = EVP_PKEY_CTX_new(local_key, ptr::null_mut());
            if ctx.is_null() {
                warn!("cannot create shared key context");
                break;
            }

            if EVP_PKEY_derive_init(ctx) != 1 {
                warn!("cannot init shared context");
                break;
            }

            if EVP_PKEY_derive_set_peer(ctx, peer_key) != 1 {
                warn!("cannot set peer key for shared context");
                break;
            }

            let mut shared_len : libc::size_t = 0;
            if EVP_PKEY_derive(ctx, ptr::null_mut(), &mut shared_len) != 1 {
                warn!("cannot get shared key length from shared context");
                break;
            }

            let shared_key = vec![0u8; shared_len];
            if EVP_PKEY_derive(ctx, shared_key.as_ptr() as *mut libc::c_char, &mut shared_len) != 1 {
                warn!("cannot get shared key from shared context");
                break;
            }

            status = Some(shared_key);
            break;
        }

        if !ctx.is_null() { EVP_PKEY_CTX_free(ctx); }
    }

    status
}

/// Creates a string of hex digits representing an ECDH X9.62 public key
/// given an OpenSSL public/private key pair.
fn ecdh_export_public_key(key: *mut EvpPkey) -> Option<String> {
    if key.is_null() {
        return None;
    }

    let mut status = None;
    let mut buf = ptr::null_mut();
    let eckey;

    unsafe {
        loop {
            eckey = EVP_PKEY_get1_EC_KEY(key);
            if eckey.is_null() {
                warn!("cannot get local ec key from local key");
                break;
            }

            let ecpoint = EC_KEY_get0_public_key(eckey);
            if ecpoint.is_null() {
                warn!("cannot get public key from local ec key");
                break;
            }

            let ecgroup = EC_KEY_get0_group(eckey);
            if ecgroup.is_null() {
                warn!("cannot get group from local ec key");
            }

            buf = EC_POINT_point2hex(ecgroup, ecpoint, EcPointConversion::Uncompressed, ptr::null_mut());
            if buf == ptr::null_mut() {
                warn!("cannot get uncompressed public key from local ec point");
                break;
            }

            status = Some(CStr::from_ptr(buf).to_string_lossy().into_owned());
            break;
        }

        if !buf.is_null() { CRYPTO_free(buf as *mut libc::c_void); }
        if !eckey.is_null() { EC_KEY_free(eckey); }
    }

    status
}

struct EcdhKeyData {
    /// String of hex digits representing the local public key.
    public_key: String,
    /// Byte array representing the shared key with the peer.
    shared_key: Vec<u8>
}

/// Derives a public key and a shared key from the given peer's public key.
///
/// * `raw_peer_key` is the peer's public ECDH key represented as hex digits.
fn ecdh_derive_keys(raw_peer_key : String) -> Option<EcdhKeyData> {
    let peer_key = ecdh_import_public_key(raw_peer_key);
    let local_key = ecdh_generate_key_pair();
    let shared_key = ecdh_derive_shared_key(local_key, peer_key);
    let public_key = ecdh_export_public_key(local_key);

    unsafe {
        if !peer_key.is_null() { EVP_PKEY_free(peer_key); }
        if !local_key.is_null() { EVP_PKEY_free(local_key); }
    }

    if shared_key.is_none() || public_key.is_none() {
        return None;
    }

    Some(EcdhKeyData {
        public_key: public_key.unwrap(),
        shared_key: shared_key.unwrap()
    })
}

/// Encrypts the given payload using AES-GCM 128-bit with the shared key and salt.
/// The shared key and salt are not used directly but rather are used to derive
/// the encryption key and nonce as defined in the draft RFC.
fn aesgcm128_encrypt(input: String, shared_key: Vec<u8>, salt: &[u8]) -> Vec<u8> {
    // Create the HKDF salt from our shared key and transaction salt
    let mut salt_hmac = Hmac::new(Sha256::new(), salt);
    salt_hmac.input(&shared_key[..]);
    let hkdf_salt = salt_hmac.result();

    // Create the AES-GCM encryption key
    // https://tools.ietf.org/html/draft-thomson-http-encryption-01#section-3.2
    let encrypt_info = b"Content-Encoding: aesgcm128\x01";
    let mut encrypt_key = [0u8; 32];
    hkdf_extract(Sha256::new(), hkdf_salt.code(), encrypt_info, &mut encrypt_key);

    // Create the AES-GCM nonce
    // https://tools.ietf.org/html/draft-thomson-http-encryption-01#section-3.3
    let nonce_info = b"Content-Encoding: nonce\x01";
    let mut nonce = [0u8; 32];
    hkdf_extract(Sha256::new(), hkdf_salt.code(), nonce_info, &mut nonce);

    // Add padding to input data in accordance with
    // https://tools.ietf.org/html/draft-ietf-httpbis-encryption-encoding-00#section-2
    //
    // "Padding consists of a length byte, followed that number of zero-valued octets.
    //  A receiver MUST fail to decrypt if any padding octet other than the first is
    //  non-zero"
    //
    // "Valid records always contain at least one byte of padding"
    let mut raw_input = input.into_bytes();
    raw_input.insert(0, 0);

    // TODO: if the data is greater than 4096, we need to encrypt
    // in chunks and change the nonce appropriately
    assert!(raw_input.len() <= 4095);

    // With the generation AES-GCM key/nonce pair, encrypt the payload
    let mut cipher = AesGcm::new(KeySize::KeySize128, &encrypt_key[0..16], &nonce[0..12], &[0; 0]);
    let mut tag = [0u8; 16];
    let mut out = vec![0u8; raw_input.len() + tag.len()];
    out.truncate(raw_input.len());
    cipher.encrypt(&raw_input[..], &mut out, &mut tag);

    // Append the authentication tag to the record payload
    // https://tools.ietf.org/html/draft-thomson-http-encryption-01#section-2
    //
    // "Valid records always contain at least one byte of padding and a 16
    // octet authentication tag."
    out.extend_from_slice(&tag);
    out
}

#[derive(Debug)]
pub struct EncryptData {
    pub public_key: String,
    pub salt: String,
    pub output: Vec<u8>
}

/// Encrypt a payload using the given public key according to the WebPush
/// RFC specifications.
pub fn encrypt(peer_key: &str, input: String) -> Option<EncryptData> {
    // Derive public and secret keys from peer public key
    let peer_key_bytes = match peer_key.from_base64() {
        Ok(x) => x,
        Err(e) => {
            warn!("could not base64 decode peer key: {:?}", e);
            return None;
        }
    };

    let ecdh = match ecdh_derive_keys(peer_key_bytes.to_hex()) {
        Some(ekd) => ekd,
        None => {
            warn!("could not derive keys");
            return None;
        }
    };

    let public_key_bytes = match ecdh.public_key.from_hex() {
        Ok(x) => x,
        Err(e) => {
            warn!("Could not derive keys: {:?}", e);
            return None;
        }
    };

    // Create the salt for this transaction
    // https://tools.ietf.org/html/draft-thomson-http-encryption-01#section-3.1
    //
    // "The "salt" parameter MUST be present, and MUST be exactly 16 octets long
    //  when decoded.  The "salt" parameter MUST NOT be reused for two different
    //  payload bodies that have the same input keying material; generating a
    //  random salt for every application of the content encoding ensures that
    //  content encryption key reuse is highly unlikely."
    let mut gen = OsRng::new().unwrap();
    let mut salt = [0u8; 16];
    gen.fill_bytes(&mut salt);

    Some(EncryptData {
        public_key: public_key_bytes.to_base64(URL_SAFE),
        salt: salt.to_base64(URL_SAFE),
        output: aesgcm128_encrypt(input, ecdh.shared_key, &salt)
    })
}

#[cfg(test)]
describe! aesgcm128_encrypt {
    it "should encrypt one record" {
        use super::aesgcm128_encrypt;

        let input = String::from("test");
        let shared_key = vec![14, 55, 71, 109, 215, 177, 33, 176, 142, 43, 241, 48, 179, 164, 96, 220, 146, 176, 76, 1, 63, 108, 78, 67, 141, 55, 125, 200, 40, 153, 252, 85];
        let salt = [23, 249, 70, 109, 205, 73, 187, 20, 140, 197, 163, 250, 114, 55, 122, 88];
        let output = aesgcm128_encrypt(input, shared_key, &salt);
        let expected = vec![177, 172, 8, 114, 38, 164, 249, 255, 11, 140, 152, 0, 194, 82, 79, 121, 26, 116, 68, 34, 182];
        assert_eq!(output, expected);
    }
}
