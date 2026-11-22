//! Wire types for the distlock protocol. See `crates/distlock-proto/proto/distlock/v1/distlock.proto`.

pub mod v1 {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/distlock.v1.rs"));
}

/// Login by password (a bcrypt hash on the server). `Hello`.
pub const AUTH_MODE_PASSWORD: u32 = 1;
/// Login by Ed25519 signature over a server nonce. `AuthStart`, `AuthChallenge`, `AuthProof`.
pub const AUTH_MODE_KEY: u32 = 2;
/// The newest auth mode this version of the protocol crate knows.
pub const MAX_AUTH_MODE: u32 = AUTH_MODE_KEY;

/// The bytes a client signs to log in with auth mode 2. The prefix keeps a signature made for this
/// purpose from being usable for anything else.
pub fn auth_message(nonce: &[u8], client_id: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(17 + nonce.len() + client_id.len());
    m.extend_from_slice(b"distlock-auth-v2\0");
    m.extend_from_slice(nonce);
    m.extend_from_slice(client_id.as_bytes());
    m
}

/// A 32-byte key (an Ed25519 seed or public key) from base64 text. Accepts the standard and the
/// URL-safe alphabet, with or without padding, so a key can be pasted into a URL as it is.
pub fn decode_key32(text: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    let url_safe: String = text
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(url_safe)
        .ok()?;
    bytes.try_into().ok()
}

/// The URL-safe, unpadded base64 text of a key: 43 characters for 32 bytes.
pub fn encode_key(key: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_in_every_base64_spelling() {
        let key: [u8; 32] = std::array::from_fn(|i| (i * 7 + 250) as u8); // has bytes that give + and /
        let url = encode_key(&key);
        assert_eq!(url.len(), 43);
        assert_eq!(decode_key32(&url), Some(key));
        let standard = url.replace('-', "+").replace('_', "/");
        assert_eq!(decode_key32(&standard), Some(key));
        assert_eq!(decode_key32(&format!("{standard}=")), Some(key));
        assert_eq!(decode_key32(&format!("  {url}\n")), Some(key));
        assert_eq!(decode_key32(&url[..42]), None);
        assert_eq!(decode_key32(""), None);
        assert_eq!(decode_key32("not base64!"), None);
    }

    #[test]
    fn the_signed_message_is_bound_to_the_nonce_and_the_user() {
        assert_ne!(auth_message(b"n1", "a"), auth_message(b"n2", "a"));
        assert_ne!(auth_message(b"n", "a"), auth_message(b"n", "b"));
        assert!(auth_message(b"n", "a").starts_with(b"distlock-auth-v2\0"));
    }

    use super::v1::*;
    use prost::Message;

    #[test]
    fn client_frame_roundtrip() {
        let frame = ClientFrame {
            seq: 7,
            body: Some(client_frame::Body::Acquire(Acquire {
                request_id: 42,
                name: "jobs/nightly".into(),
                mode: Mode::Semaphore as i32,
                ttl_ms: 30_000,
                wait_ms: 5_000,
                semaphore_capacity: 4,
                permits: 1,
            })),
        };
        let bytes = frame.encode_to_vec();
        assert_eq!(ClientFrame::decode(bytes.as_slice()).unwrap(), frame);
    }

    #[test]
    fn server_push_roundtrip() {
        let frame = ServerFrame {
            seq: 0,
            body: Some(server_frame::Body::Granted(Granted {
                grant: Some(Grant {
                    request_id: 42,
                    name: "jobs/nightly".into(),
                    fencing_token: 1_700_000_000_000_001,
                    ttl_remaining_ms: 30_000,
                    previous: Some(PreviousState {
                        state: HolderState::Abandoned as i32,
                        set_by_token: 1_700_000_000_000_000,
                        age_ms: 120,
                    }),
                }),
            })),
        };
        let bytes = frame.encode_to_vec();
        assert_eq!(ServerFrame::decode(bytes.as_slice()).unwrap(), frame);
    }

    #[test]
    fn event_messages_roundtrip_and_keep_absent_apart_from_empty_and_zero() {
        // "No value" and "the empty string", and "server default window" and "no window", are four
        // different requests; they must survive the wire as such.
        for (value, publish_ms) in [
            (None, None),
            (Some(String::new()), Some(0)),
            (Some("payload".to_owned()), Some(2_500)),
        ] {
            let frame = ClientFrame {
                seq: 3,
                body: Some(client_frame::Body::EventSet(EventSet {
                    name: "ns:event:deploy-finished".into(),
                    value: value.clone(),
                    publish_ms,
                })),
            };
            let decoded = ClientFrame::decode(frame.encode_to_vec().as_slice()).unwrap();
            assert_eq!(decoded, frame);
            let Some(client_frame::Body::EventSet(set)) = decoded.body else {
                panic!("not an EventSet")
            };
            assert_eq!((set.value, set.publish_ms), (value, publish_ms));
        }

        let push = ServerFrame {
            seq: 0,
            body: Some(server_frame::Body::EventPublished(EventPublished {
                request_id: 9,
                name: "ns:event:deploy-finished".into(),
                publication: Some(Publication {
                    id: 1_700_000_000_000_002,
                    value: None,
                    age_ms: 0,
                }),
            })),
        };
        assert_eq!(
            ServerFrame::decode(push.encode_to_vec().as_slice()).unwrap(),
            push
        );
    }

    #[test]
    fn garbage_does_not_panic() {
        // Decoder robustness smoke test (real fuzzing comes later).
        for len in 0..64u8 {
            let junk: Vec<u8> = (0..len)
                .map(|i| i.wrapping_mul(37).wrapping_add(11))
                .collect();
            let _ = ClientFrame::decode(junk.as_slice());
            let _ = ServerFrame::decode(junk.as_slice());
        }
    }
}
