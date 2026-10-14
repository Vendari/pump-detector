//! Short-lived read tokens for browsers (the Trigger mini app and web app).
//!
//! The Trigger api signs them for a signed-in user; we only check them, with
//! the shared `PRICES_TOKEN_SECRET`, so a browser never holds our API key and
//! this service never calls the api. Format:
//! `base64url(payload) "." base64url(HMAC-SHA256(secret, base64url(payload)))`
//! with payload `{"sub": "<user id>", "exp": <unix seconds>, "scope": "prices"}`.
//! A leaked token reads public market data until it expires; it opens
//! nothing else.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub const SCOPE: &str = "prices";
/// Longest lifetime we accept, whatever `exp` says.
pub const MAX_LIFETIME_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: i64,
    pub scope: String,
}

fn mac(secret: &str, data: &[u8]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    m.update(data);
    m
}

pub fn sign(secret: &str, claims: &Claims) -> String {
    let body = B64.encode(serde_json::to_vec(claims).expect("claims"));
    let sig = B64.encode(mac(secret, body.as_bytes()).finalize().into_bytes());
    format!("{body}.{sig}")
}

pub fn verify(secret: &str, token: &str, now: i64) -> Result<Claims, &'static str> {
    let (body, sig) = token.split_once('.').ok_or("malformed token")?;
    let sig = B64.decode(sig).map_err(|_| "malformed token")?;
    mac(secret, body.as_bytes())
        .verify_slice(&sig)
        .map_err(|_| "bad signature")?;
    let claims: Claims = serde_json::from_slice(&B64.decode(body).map_err(|_| "malformed token")?)
        .map_err(|_| "malformed token")?;
    if claims.scope != SCOPE {
        return Err("wrong scope");
    }
    if claims.exp <= now {
        return Err("token expired");
    }
    if claims.exp - now > MAX_LIFETIME_SECS {
        return Err("token lifetime too long");
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: i64) -> Claims {
        Claims {
            sub: "user-1".into(),
            exp,
            scope: SCOPE.into(),
        }
    }

    #[test]
    fn round_trip_and_rejections() {
        let t = sign("s3cret", &claims(1_000 + 900));
        assert_eq!(verify("s3cret", &t, 1_000).unwrap().sub, "user-1");
        assert_eq!(verify("other", &t, 1_000), Err("bad signature"));
        assert_eq!(verify("s3cret", &t, 2_000), Err("token expired"));
        let long = sign("s3cret", &claims(1_000 + MAX_LIFETIME_SECS + 1));
        assert_eq!(
            verify("s3cret", &long, 1_000),
            Err("token lifetime too long")
        );
        let (body, _) = t.split_once('.').unwrap();
        assert_eq!(verify("s3cret", body, 1_000), Err("malformed token"));
        // A payload swapped under someone else's signature fails.
        let other = sign("s3cret", &claims(5_000));
        let forged = format!(
            "{}.{}",
            other.split_once('.').unwrap().0,
            t.split_once('.').unwrap().1
        );
        assert_eq!(verify("s3cret", &forged, 1_000), Err("bad signature"));
    }

    #[test]
    fn matches_the_documented_format() {
        // Same bytes a Node signer produces:
        // createHmac('sha256', secret).update(body).digest('base64url')
        let c = claims(1_791_000_000);
        let t = sign("k", &c);
        let body = B64.encode(br#"{"sub":"user-1","exp":1791000000,"scope":"prices"}"#);
        assert!(t.starts_with(&format!("{body}.")));
    }
}
