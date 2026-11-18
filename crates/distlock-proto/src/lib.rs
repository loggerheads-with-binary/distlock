//! Wire types for the distlock protocol. See `proto/distlock/v1/distlock.proto`.

pub mod v1 {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/distlock.v1.rs"));
}

#[cfg(test)]
mod tests {
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
