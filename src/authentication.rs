use crate::{config::Settings, delegation::SECP};
use bitcoin_hashes::{sha256, Hash};
use chrono::{Days, Utc};
use hmac::{Hmac, Mac};
use http::HeaderValue;
use jwt::{self, SignWithKey, VerifyWithKey};
use nostr::Keys;
use secp256k1::{schnorr, XOnlyPublicKey};
use sha2::Sha256;
use std::{collections::BTreeMap, str::FromStr};

pub fn authenticate(key: &Keys, signature: &str) -> bool {
    let pub_key_str = key.public_key().to_string();
    let public_key = XOnlyPublicKey::from_str(&pub_key_str).unwrap();
    if let Ok(sig) = schnorr::Signature::from_str(signature) {
        let data = format!("nostr:permission:{}:allowed", &public_key);
        let digest: sha256::Hash = sha256::Hash::hash(data.as_bytes());
        let msg = secp256k1::Message::from_slice(digest.as_ref()).unwrap();
        let verify = SECP.verify_schnorr(&sig, &msg, &public_key);
        return verify.is_ok();
    }
    false
}

pub fn generate_auth_token(key: &Keys, settings: &Settings) -> String {
    let signing_key: Hmac<Sha256> = Hmac::new_from_slice(settings.pay_to_relay.auth_secret.as_bytes()).unwrap();
    let mut claims = BTreeMap::new();
    claims.insert("sub", key.public_key().to_string());
    let exp = Utc::now().checked_add_days(Days::new(1)).unwrap().to_rfc3339();
    claims.insert("exp", exp);

    claims.sign_with_key(&signing_key).unwrap()
}

pub fn get_token_value(header: &HeaderValue) -> Option<&str> {
    if let Ok(cookies) = header.to_str() {
        for cookie in cookies.split(';') {
            let parts: Vec<&str> = cookie.split('=').collect();
            if parts.len() == 2 {
                if parts[0] == "token" {
                    return Some(parts[1]);
                }
            }
        }
    }
    None
}

pub fn validate_auth_token(token: &str, key: &Keys, settings: &Settings) -> bool {
    let signing_key: Hmac<Sha256> = Hmac::new_from_slice(settings.pay_to_relay.auth_secret.as_bytes()).unwrap();
    let verify_result: Result<BTreeMap<String, String>, jwt::Error> = token.verify_with_key(&signing_key);
    if let Ok(claims) = verify_result {
        if let Some(sub) = claims.get("sub") {
            let pubkey = key.public_key().to_string();
            return *sub == pubkey;
        }
    }
    false
}
