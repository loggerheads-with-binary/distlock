//! Conversions between wire (protobuf) types and `distlock-core` types.

use std::time::Duration;

use distlock_core as core;
use distlock_proto::v1 as pb;
use distlock_proto::v1::server_frame;

pub(crate) fn ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

pub(crate) fn dur(ms: u32) -> Duration {
    Duration::from_millis(u64::from(ms))
}

pub(crate) fn want_from_pb(
    mode: i32,
    capacity: u32,
    permits: u32,
) -> Result<core::Want, &'static str> {
    match pb::Mode::try_from(mode) {
        Ok(pb::Mode::Mutex) => Ok(core::Want::Mutex),
        Ok(pb::Mode::Read) => Ok(core::Want::Read),
        Ok(pb::Mode::Write) => Ok(core::Want::Write),
        Ok(pb::Mode::Semaphore) => Ok(core::Want::Semaphore {
            capacity,
            permits: permits.max(1),
        }),
        Ok(pb::Mode::Unspecified) | Err(_) => Err("mode is missing or unknown"),
    }
}

pub(crate) fn mode_to_pb(want: core::Want) -> pb::Mode {
    match want {
        core::Want::Mutex => pb::Mode::Mutex,
        core::Want::Read => pb::Mode::Read,
        core::Want::Write => pb::Mode::Write,
        core::Want::Semaphore { .. } => pb::Mode::Semaphore,
    }
}

pub(crate) fn holder_state_to_pb(state: core::HolderState) -> pb::HolderState {
    match state {
        core::HolderState::Succeeded => pb::HolderState::Succeeded,
        core::HolderState::Failed => pb::HolderState::Failed,
        core::HolderState::Stored => pb::HolderState::Stored,
        core::HolderState::Abandoned => pb::HolderState::Abandoned,
    }
}

/// `Ok(None)` = UNSET = leave the existing tag untouched.
pub(crate) fn release_state_from_pb(
    value: i32,
) -> Result<Option<core::ReleaseState>, &'static str> {
    match pb::HolderState::try_from(value) {
        Ok(pb::HolderState::Unset) => Ok(None),
        Ok(pb::HolderState::Succeeded) => Ok(Some(core::ReleaseState::Succeeded)),
        Ok(pb::HolderState::Failed) => Ok(Some(core::ReleaseState::Failed)),
        Ok(pb::HolderState::Stored) => Ok(Some(core::ReleaseState::Stored)),
        Ok(pb::HolderState::Abandoned) => {
            Err("ABANDONED is assigned by the server and cannot be set by clients")
        }
        Err(_) => Err("unknown state"),
    }
}

pub(crate) fn grant_to_pb(
    request: core::RequestId,
    name: &str,
    grant: &core::GrantInfo,
) -> pb::Grant {
    pb::Grant {
        request_id: request.0,
        name: name.to_owned(),
        fencing_token: grant.token.0,
        ttl_remaining_ms: ms(grant.ttl_remaining),
        previous: previous_to_pb(grant.previous),
    }
}

fn previous_to_pb(previous: Option<core::PreviousState>) -> Option<pb::PreviousState> {
    previous.map(|p| pb::PreviousState {
        state: holder_state_to_pb(p.state) as i32,
        set_by_token: p.set_by.0,
        age_ms: ms(p.age),
    })
}

pub(crate) fn view_to_pb(view: core::SessionView) -> pb::SessionView {
    pb::SessionView {
        held: view
            .held
            .into_iter()
            .map(|h| pb::HeldLock {
                request_id: h.request.0,
                name: h.name.to_string(),
                mode: mode_to_pb(h.want) as i32,
                fencing_token: h.token.0,
                ttl_remaining_ms: ms(h.ttl_remaining),
                previous: previous_to_pb(h.previous),
            })
            .collect(),
        pending: view
            .pending
            .into_iter()
            .map(|p| pb::PendingRequest {
                request_id: p.request.0,
                name: p.name.to_string(),
                mode: mode_to_pb(p.want) as i32,
            })
            .collect(),
    }
}

pub(crate) fn error_code_of(err: &core::Error) -> pb::ErrorCode {
    match err {
        core::Error::InvalidArgument(_) => pb::ErrorCode::InvalidArgument,
        core::Error::Busy => pb::ErrorCode::Busy,
        core::Error::KindMismatch { .. } => pb::ErrorCode::KindMismatch,
        core::Error::CapacityMismatch { .. } => pb::ErrorCode::CapacityMismatch,
        core::Error::NotHeld => pb::ErrorCode::NotHeld,
        core::Error::RequestConflict => pb::ErrorCode::RequestConflict,
        core::Error::ResourceExhausted(_) => pb::ErrorCode::ResourceExhausted,
        core::Error::GracePeriod => pb::ErrorCode::Unavailable,
        core::Error::Internal(_) => pb::ErrorCode::Internal,
    }
}

pub(crate) fn core_error_to_pb(err: &core::Error) -> pb::Error {
    pb::Error {
        code: error_code_of(err) as i32,
        message: err.to_string(),
        existing_capacity: match err {
            core::Error::CapacityMismatch { existing } => *existing,
            _ => 0,
        },
    }
}

pub(crate) fn error_body(code: pb::ErrorCode, message: impl Into<String>) -> server_frame::Body {
    server_frame::Body::Error(pb::Error {
        code: code as i32,
        message: message.into(),
        existing_capacity: 0,
    })
}

pub(crate) fn frame(seq: u64, body: server_frame::Body) -> pb::ServerFrame {
    pb::ServerFrame {
        seq,
        body: Some(body),
    }
}
