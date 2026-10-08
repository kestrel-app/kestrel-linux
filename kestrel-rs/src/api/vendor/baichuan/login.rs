//! The device login handshake.
//!
//! Two legs, both confirmed against a real device up to the nonce:
//!
//! 1. A **header-only** "login upgrade" message (msg_id 1, class `0x6514`), whose
//!    `response_code` asks for an encryption level. The device replies with a
//!    BCEncrypt'd XML `Encryption` block carrying a **nonce** and `type` `md5`.
//! 2. A **modern** login (class `0x6414`) with the credentials salted by that
//!    nonce: `userName` and `password` are each the **uppercase** MD5 hex of
//!    `value + nonce`, truncated to 31 characters (the device keeps 31 of the 32
//!    and null-terminates). Success is `response_code == 200`.
//!
//! **No proof-of-work is involved.** The nonce reply's `authTypeList` is
//! `password`/`sigV1`/`authLogin`/`getAccesskey`; the app's PoW solver belongs to
//! *cloud-account* auth, not device login, so it is not used here.
//!
//! **Verification status — verified live.** The whole login was completed against a
//! real NVR: it returns `response_code == 200` and a `DeviceInfo` body. The one
//! subtlety that made it work: every login message (`msg_id == 1`), in both
//! directions, is encrypted with **BCEncrypt even though AES is requested** — AES
//! only takes over for messages *after* login. So leg 1 asks for AES (the device
//! will not answer a BCEncrypt-only request), but leg 2 and the reply are BCEncrypt.
//! On success the session switches to AES-128-CFB (keyed from the nonce and
//! password), which every message after login uses. See [`super`] and
//! `docs/untested.md`.

use std::time::Duration;

use log::info;

use super::crypto::{make_aes_key, md5};
use super::session::Session;
use super::wire::{Encryption, Message, CLASS_LEGACY};
use super::{cmd, xml};
use crate::api::error::{Error, Result};

/// The encryption level leg 1 asks for. It must be AES: the device does not answer
/// a BCEncrypt-only request (`0xdc01`) at all. The login bodies are still BCEncrypt
/// regardless — AES only applies to post-login messages.
/// (`0xdc00` = none, `0xdc01` = BCEncrypt, `0xdc12` = AES.)
const ENC_REQUEST_AES: u16 = 0xdc12;

/// The device's login hash: uppercase MD5 hex of `value + nonce`, truncated to 31
/// characters. The truncation mirrors the device keeping 31 hex chars plus a null.
pub fn login_hash(value: &str, nonce: &str) -> String {
    let digest = md5(format!("{value}{nonce}").as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest {
        hex.push_str(&format!("{byte:02X}"));
    }
    hex.truncate(31);
    hex
}

/// Run the full login against an open session. On success the session's cipher is
/// raised to AES-128-CFB, which is what every message after login uses, and the
/// device's `DeviceInfo` reply body (which carries the channel count) is returned.
pub fn login(
    session: &mut Session,
    user: &str,
    password: &str,
    timeout: Duration,
) -> Result<String> {
    // The whole login exchange is BCEncrypt (offset 0), so decrypt replies that way.
    // The sent messages carry their own cipher: leg 1 is header-only, leg 2 BCEncrypt.
    session.set_encryption(Encryption::BcEncrypt(0));

    // Leg 1: header-only login upgrade, requesting AES. The msg_num is shared across
    // both legs, as the device pairs the reply to it.
    let msg_num = 1;
    let upgrade = Message::header_only(cmd::LOGIN, msg_num, ENC_REQUEST_AES, CLASS_LEGACY);
    let reply = session.send(upgrade, timeout)?;

    let nonce = xml::login_nonce(reply.xml()?)
        .ok_or_else(|| Error::auth("login: device did not return a nonce"))?;
    info!("device returned a login nonce; sending credentials");

    // Leg 2: modern login, credentials salted with the nonce.
    let user_hash = login_hash(user, &nonce);
    let pass_hash = login_hash(password, &nonce);
    let mut auth = Message::modern(
        cmd::LOGIN,
        msg_num,
        Encryption::BcEncrypt(0),
        xml::login_auth(&user_hash, &pass_hash),
    );
    auth.response_code = 0;
    let result = session.send(auth, timeout)?;

    if result.response_code != 200 {
        return Err(Error::auth(format!(
            "login: credentials rejected (response {})",
            result.response_code
        )));
    }

    // The DeviceInfo body comes back under the login's own (BCEncrypt) cipher; grab
    // it before switching the session to AES for everything that follows.
    let device_info = result.xml()?.to_string();

    // Everything after login is AES-128-CFB, keyed from the nonce and password.
    session.set_encryption(Encryption::Aes(make_aes_key(&nonce, password)));
    Ok(device_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest is uppercase, 31 chars, salted by the nonce. Anchored to MD5 of
    /// "admin" so the uppercase-and-truncate behaviour is pinned: the full
    /// uppercase MD5 of "admin" is 21232F297A57A5A743894A0E4A801FC2, and the login
    /// form drops the last character.
    #[test]
    fn login_hash_is_uppercase_md5_truncated_to_31() {
        // With an empty nonce the input is just "admin".
        assert_eq!(login_hash("admin", ""), "21232F297A57A5A743894A0E4A801FC");
        assert_eq!(login_hash("admin", "").len(), 31);
    }

    #[test]
    fn login_hash_is_salted_by_the_nonce() {
        let a = login_hash("secret", "nonce-A");
        let b = login_hash("secret", "nonce-B");
        assert_ne!(a, b);
        assert_eq!(a, login_hash("secret", "nonce-A"));
    }
}
