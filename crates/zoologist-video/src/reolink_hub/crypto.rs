//! The Hub's newer login ("Version 1"), as its own web app does it:
//!
//! 1. `Login` with `{"Version": 1}` is answered with a `WWW-Authenticate: Digest realm=…, qop=…,
//!    nonce=…, nc=…` header.
//! 2. The second `Login` sends an HTTP-digest style `Response` = MD5(HA1:nonce:nc:cnonce:qop:HA2).
//! 3. From then on, every request body and every answer is AES-128-CFB encrypted (CryptoJS
//!    semantics: full 128-bit feedback, zero padding) and Base64 encoded. The key and IV are
//!    16 upper-case hex characters of MD5 hashes of the nonce, password and a random cnonce.
//!    The query carries `encrypt=` + an encrypted `countId=…&checkNum=…&cmd=…` against replays.

use aes::Aes128;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cfb_mode::cipher::{AsyncStreamCipher, KeyIvInit};
use md5::{Digest, Md5};

/// `uri` the digest is computed over (the web app's relative request URL).
pub const LOGIN_URI: &str = "cgi-bin/api.cgi?cmd=Login";

pub fn md5_hex(s: &str) -> String {
    let hash = Md5::digest(s.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// The fields of a `WWW-Authenticate: Digest …` challenge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub qop: String,
    pub nonce: String,
    pub nc: String,
}

impl Challenge {
    /// Parses `Digest realm="x", qop="auth", nonce="…", nc=00000001` (quotes optional).
    pub fn parse(header: &str) -> Option<Challenge> {
        let rest = header.trim().strip_prefix("Digest")?;
        let field = |name: &str| {
            rest.split(',').find_map(|part| {
                let (k, v) = part.trim().split_once('=')?;
                (k.trim() == name).then(|| v.trim().trim_matches('"').to_string())
            })
        };
        Some(Challenge {
            realm: field("realm")?,
            qop: field("qop").unwrap_or_default(),
            nonce: field("nonce")?,
            nc: field("nc").unwrap_or_default(),
        })
    }
}

/// The digest `Response` for the second login step.
pub fn digest_response(c: &Challenge, user: &str, password: &str, cnonce: &str) -> String {
    let ha1 = md5_hex(&format!("{user}:{}:{password}", c.realm));
    let ha2 = md5_hex(&format!("POST:{LOGIN_URI}"));
    md5_hex(&format!(
        "{ha1}:{}:{}:{cnonce}:{}:{ha2}",
        c.nonce, c.nc, c.qop
    ))
}

/// 48 random hex characters, like the web app's cnonce.
pub fn new_cnonce() -> String {
    let mut bytes = [0u8; 24];
    if getrandom::fill(&mut bytes).is_err() {
        // No OS randomness: fall back to the clock. The cnonce only needs to be unique.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        return md5_hex(&t.to_string()) + &md5_hex(&format!("{t}x"))[..16];
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The session cipher agreed during login.
#[derive(Clone)]
pub struct Cipher {
    key: [u8; 16],
    iv: [u8; 16],
}

impl Cipher {
    pub fn new(c: &Challenge, user: &str, password: &str, cnonce: &str) -> Cipher {
        let part = |s: String| -> [u8; 16] {
            let hex = md5_hex(&s).to_uppercase();
            let mut out = [0u8; 16];
            out.copy_from_slice(&hex.as_bytes()[..16]);
            out
        };
        Cipher {
            key: part(format!("{}-{password}-{cnonce}", c.nonce)),
            iv: part(format!("webapp-{cnonce}-{password}-{}-{user}", c.nonce)),
        }
    }

    /// Zero-pads `plain` to whole blocks, encrypts it and returns Base64.
    pub fn encrypt(&self, plain: &str) -> String {
        let mut buf = plain.as_bytes().to_vec();
        buf.resize(buf.len().div_ceil(16) * 16, 0);
        cfb_mode::Encryptor::<Aes128>::new(&self.key.into(), &self.iv.into()).encrypt(&mut buf);
        B64.encode(buf)
    }

    /// Decodes and decrypts `text`, dropping the zero padding. `None` if it is not Base64 or not text.
    pub fn decrypt(&self, text: &str) -> Option<String> {
        let mut buf = B64.decode(text.trim()).ok()?;
        cfb_mode::Decryptor::<Aes128>::new(&self.key.into(), &self.iv.into()).decrypt(&mut buf);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).ok()
    }

    #[cfg(test)]
    pub fn key_iv(&self) -> (String, String) {
        (
            String::from_utf8_lossy(&self.key).into_owned(),
            String::from_utf8_lossy(&self.iv).into_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference values computed with Node's OpenSSL AES-128-CFB (the same full-block CFB and
    /// zero padding as the Hub's CryptoJS web app).
    fn challenge() -> Challenge {
        Challenge {
            realm: "Reolink".into(),
            qop: "auth".into(),
            nonce: "abc123nonce".into(),
            nc: "00000001".into(),
        }
    }
    const USER: &str = "zoologist";
    const PASS: &str = "p@ss word!";
    const CNONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn digest_matches_the_web_app() {
        assert_eq!(
            digest_response(&challenge(), USER, PASS, CNONCE),
            "5819cddf3f30c83d39d398d1fd6169c2"
        );
    }

    #[test]
    fn cipher_matches_the_web_app() {
        let c = Cipher::new(&challenge(), USER, PASS, CNONCE);
        assert_eq!(
            c.key_iv(),
            ("B49F2A6AEF77BC38".into(), "489F93CF39EC2285".into())
        );
        let plain = r#"[{"cmd":"GetDevInfo","action":0,"param":{}}]"#;
        let enc = c.encrypt(plain);
        assert_eq!(
            enc,
            "c9bPor/pK7bXS6kHcaV2Wti+/2TMGiR3RQz48HsJ+cFu+2DWfKEtOrKDsyiR19zp"
        );
        assert_eq!(c.decrypt(&enc).as_deref(), Some(plain));
        assert_eq!(
            c.encrypt("countId=3&checkNum=1001&cmd=Search"),
            "S8KYr6bEbbHGKq8bUKNrXbR9zkDImPjLNJTNIcZqqvAULJHSjM4hkLWPYBZ/ue+Y"
        );
        assert_eq!(c.decrypt("not base64!"), None);
    }

    #[test]
    fn challenges_are_parsed() {
        let c = Challenge::parse(
            r#"Digest realm="Reolink", qop="auth", nonce="abc123nonce", nc=00000001"#,
        )
        .unwrap();
        assert_eq!(c, challenge());
        assert!(Challenge::parse("Basic realm=x").is_none());
        assert_eq!(new_cnonce().len(), 48);
    }
}
