//! Web Push to remote browsers that opted in (typically Mandor added to a
//! phone's home screen). The desktop client already decides when a session has
//! finished or needs input (see `notifyAttention` in `ui/app.js`) and calls
//! `push_notify`; this module owns the VAPID key pair and the subscriptions,
//! and delivers to every subscribed device.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use base64::{
    alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
    Engine as _,
};
use serde::{Deserialize, Serialize};
use web_push_native::{
    jwt_simple::algorithms::{ECDSAP256PublicKeyLike, ES256KeyPair},
    p256::PublicKey,
    Auth, WebPushBuilder,
};

// Browsers hand out unpadded base64url keys; tolerate padded ones too.
const B64URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

const VAPID_SUBJECT: &str = "https://github.com/otezz/mandor";
// Apple rejects VAPID tokens valid for much longer than an hour, and a
// "needs input" alert that arrives later than that is stale anyway.
const PUSH_TTL: Duration = Duration::from_secs(60 * 60);

static FILE_LOCK: Mutex<()> = Mutex::new(());

/// `PushSubscription.toJSON()` from the browser, as-is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushFile {
    #[serde(default)]
    vapid_private: String,
    #[serde(default)]
    subscriptions: Vec<Subscription>,
}

fn push_file() -> Option<PathBuf> {
    crate::store::store_dir().map(|d| d.join("push.json"))
}

fn load() -> PushFile {
    push_file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(file: &PushFile) -> Result<(), String> {
    let path = push_file().ok_or("no data directory available")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

// Holds the VAPID private key, so owner-only like the uploads.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

fn key_pair(file: &mut PushFile) -> Result<ES256KeyPair, String> {
    if let Ok(bytes) = B64URL.decode(&file.vapid_private) {
        if let Ok(kp) = ES256KeyPair::from_bytes(&bytes) {
            return Ok(kp);
        }
    }
    let kp = ES256KeyPair::generate();
    file.vapid_private = B64URL.encode(kp.to_bytes());
    save(file)?;
    Ok(kp)
}

fn public_key_of(kp: &ES256KeyPair) -> String {
    B64URL.encode(kp.public_key().public_key().to_bytes_uncompressed())
}

/// The VAPID public key a browser needs for `pushManager.subscribe`. Created on
/// first use and kept, since rotating it invalidates every subscription.
#[tauri::command]
pub fn push_public_key() -> Result<String, String> {
    let _guard = FILE_LOCK.lock().map_err(|e| e.to_string())?;
    let mut file = load();
    Ok(public_key_of(&key_pair(&mut file)?))
}

#[tauri::command]
pub fn push_subscribe(subscription: Subscription) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().map_err(|e| e.to_string())?;
    let mut file = load();
    file.subscriptions
        .retain(|s| s.endpoint != subscription.endpoint);
    file.subscriptions.push(subscription);
    save(&file)
}

#[tauri::command]
pub fn push_unsubscribe(endpoint: String) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().map_err(|e| e.to_string())?;
    let mut file = load();
    file.subscriptions.retain(|s| s.endpoint != endpoint);
    save(&file)
}

fn build_request(
    sub: &Subscription,
    kp: &ES256KeyPair,
    payload: &[u8],
) -> Result<http::Request<Vec<u8>>, String> {
    let endpoint = sub.endpoint.parse().map_err(|e| format!("{e}"))?;
    let ua_public = B64URL
        .decode(&sub.keys.p256dh)
        .ok()
        .and_then(|b| PublicKey::from_sec1_bytes(&b).ok())
        .ok_or("bad p256dh key")?;
    let auth = B64URL
        .decode(&sub.keys.auth)
        .ok()
        .filter(|b| b.len() == 16)
        .ok_or("bad auth secret")?;
    let mut req = WebPushBuilder::new(endpoint, ua_public, Auth::clone_from_slice(&auth))
        .with_valid_duration(PUSH_TTL)
        .with_vapid(kp, VAPID_SUBJECT)
        .build(payload.to_vec())
        .map_err(|e| format!("{e}"))?;
    req.headers_mut()
        .insert("Urgency", http::HeaderValue::from_static("high"));
    Ok(req)
}

/// 404/410 mean the browser dropped the subscription (app removed, permission
/// revoked); those are pruned rather than retried forever.
fn is_gone(status: u16) -> bool {
    status == 404 || status == 410
}

fn send_all(payload: Vec<u8>) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().map_err(|e| e.to_string())?;
    let mut file = load();
    if file.subscriptions.is_empty() {
        return Ok(());
    }
    let kp = key_pair(&mut file)?;
    let before = file.subscriptions.len();
    deliver(&mut file.subscriptions, &kp, &payload);
    if file.subscriptions.len() != before {
        save(&file)?;
    }
    Ok(())
}

/// Sends to each subscription, keeping only those that are still alive.
fn deliver(subscriptions: &mut Vec<Subscription>, kp: &ES256KeyPair, payload: &[u8]) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    subscriptions.retain(|sub| {
        let req = match build_request(sub, kp, payload) {
            Ok(req) => req,
            Err(e) => {
                eprintln!("[mandor] push: dropping malformed subscription: {e}");
                return false;
            }
        };
        match agent.run(req) {
            Ok(res) if is_gone(res.status().as_u16()) => false,
            Ok(res) => {
                if !res.status().is_success() {
                    eprintln!("[mandor] push: {} from {}", res.status(), sub.endpoint);
                }
                true
            }
            Err(e) => {
                eprintln!("[mandor] push: {e}");
                true
            }
        }
    });
}

#[tauri::command]
pub async fn push_notify(
    title: String,
    body: String,
    session_id: Option<String>,
) -> Result<(), String> {
    let payload = serde_json::json!({ "title": title, "body": body, "sessionId": session_id });
    let bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || send_all(bytes))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_encrypted_signed_and_urgent() {
        // A real browser-side key pair, so the encryption step actually runs.
        let ua = ES256KeyPair::generate();
        let sub = Subscription {
            endpoint: "https://push.example.com/send/abc".into(),
            keys: SubscriptionKeys {
                p256dh: B64URL.encode(ua.public_key().public_key().to_bytes_uncompressed()),
                auth: B64URL.encode([7u8; 16]),
            },
        };
        let kp = ES256KeyPair::generate();
        let req = build_request(&sub, &kp, b"{\"title\":\"t\"}").unwrap();
        let h = req.headers();
        assert_eq!(req.uri(), "https://push.example.com/send/abc");
        assert_eq!(h["Content-Encoding"], "aes128gcm");
        assert_eq!(h["Urgency"], "high");
        assert_eq!(h["TTL"], "3600");
        let auth = h["Authorization"].to_str().unwrap();
        assert!(auth.starts_with("vapid t="));
        assert!(auth.ends_with(&format!("k={}", public_key_of(&kp))));
        assert!(!req.body().windows(5).any(|w| w == b"title"));
    }

    /// A fake push service on loopback: answers 410 on paths containing
    /// "gone", 201 otherwise, and hands back each request body it received.
    fn fake_push_service(expected: usize) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for stream in listener.incoming().take(expected) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                bodies.push(body);
                let status = if request_line.contains("gone") {
                    "410 Gone"
                } else {
                    "201 Created"
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
                .unwrap();
            }
            bodies
        });
        (base, handle)
    }

    #[test]
    fn delivers_decryptable_payloads_and_prunes_gone_subscriptions() {
        use web_push_native::p256::{elliptic_curve::sec1::ToEncodedPoint, SecretKey};
        let ua_secret = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let auth = [9u8; 16];
        let keys = SubscriptionKeys {
            p256dh: B64URL.encode(ua_secret.public_key().to_encoded_point(false).as_bytes()),
            auth: B64URL.encode(auth),
        };
        let (base, server) = fake_push_service(2);
        let alive = Subscription {
            endpoint: format!("{base}/alive"),
            keys: keys.clone(),
        };
        let gone = Subscription {
            endpoint: format!("{base}/gone"),
            keys,
        };
        let mut subs = vec![alive.clone(), gone];
        deliver(&mut subs, &ES256KeyPair::generate(), b"{\"title\":\"hi\"}");
        assert_eq!(subs, vec![alive]);
        for body in server.join().unwrap() {
            let plain =
                web_push_native::decrypt(body, &ua_secret, &Auth::clone_from_slice(&auth)).unwrap();
            assert_eq!(plain, b"{\"title\":\"hi\"}");
        }
    }

    // Network: run with --ignored to confirm TLS and VAPID reach Apple's push
    // service, which rejects the made-up device token with a 4xx.
    #[test]
    #[ignore]
    fn reaches_apple_push_service() {
        use web_push_native::p256::{elliptic_curve::sec1::ToEncodedPoint, SecretKey};
        let ua_secret = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let sub = Subscription {
            endpoint: "https://web.push.apple.com/QGd0aW5nLWEtdGVzdA".into(),
            keys: SubscriptionKeys {
                p256dh: B64URL.encode(ua_secret.public_key().to_encoded_point(false).as_bytes()),
                auth: B64URL.encode([9u8; 16]),
            },
        };
        let req = build_request(&sub, &ES256KeyPair::generate(), b"x").unwrap();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let res = agent.run(req).expect("TLS connection to Apple failed");
        let status = res.status().as_u16();
        eprintln!("apple status {status}");
        assert!((400..500).contains(&status));
    }

    #[test]
    fn rejects_malformed_keys() {
        let kp = ES256KeyPair::generate();
        let sub = Subscription {
            endpoint: "https://push.example.com/x".into(),
            keys: SubscriptionKeys {
                p256dh: "not-a-key".into(),
                auth: B64URL.encode([1u8; 16]),
            },
        };
        assert!(build_request(&sub, &kp, b"x").is_err());
    }

    #[test]
    fn padded_and_unpadded_keys_both_decode() {
        assert_eq!(B64URL.decode("AQ==").unwrap(), vec![1]);
        assert_eq!(B64URL.decode("AQ").unwrap(), vec![1]);
    }

    #[test]
    fn only_404_and_410_prune() {
        assert!(is_gone(404) && is_gone(410));
        assert!(!is_gone(201) && !is_gone(429) && !is_gone(500));
    }
}
