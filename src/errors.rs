// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Service error with conversions from common errors.

use actix::MailboxError;
use log::error;
use std::io::ErrorKind;

/// Errors returned to the Remote as WebSocket error responses.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("Internal server error: {0}")]
    InternalServerError(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Receiver is not connected")]
    NotConnected,
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),
    #[error("Timeout")]
    Timeout,
    #[error("Not supported: {0}")]
    NotSupported(String),
    /// The state of the receiver does not allow the command, for example a Dirac filter blocks the
    /// tone control. Nothing was sent. The text says why.
    #[error("Not possible now: {0}")]
    Blocked(String),
}

impl From<std::io::Error> for ServiceError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound(error.to_string()),
            ErrorKind::AlreadyExists | ErrorKind::InvalidInput | ErrorKind::InvalidData => {
                Self::BadRequest(error.to_string())
            }
            ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::AddrInUse
            | ErrorKind::AddrNotAvailable => Self::ServiceUnavailable(error.to_string()),
            ErrorKind::TimedOut => Self::Timeout,
            _ => {
                error!("Internal error: {error:?}");
                Self::InternalServerError(error.to_string())
            }
        }
    }
}

impl From<MailboxError> for ServiceError {
    fn from(e: MailboxError) -> Self {
        match e {
            // The receiver actor was stopped, for example because the receiver was removed.
            MailboxError::Closed => Self::NotFound("The receiver is not configured".into()),
            MailboxError::Timeout => Self::Timeout,
        }
    }
}

impl From<serde_json::Error> for ServiceError {
    fn from(e: serde_json::Error) -> Self {
        Self::SerializationError(e.to_string())
    }
}

impl From<strum::ParseError> for ServiceError {
    fn from(e: strum::ParseError) -> Self {
        Self::BadRequest(e.to_string())
    }
}

impl From<denon_marantz::Error> for ServiceError {
    fn from(e: denon_marantz::Error) -> Self {
        if e.is_not_connected() {
            Self::NotConnected
        } else if e.is_not_sent() {
            // The connection could not be opened, also after the tries of the library.
            Self::ServiceUnavailable(e.to_string())
        } else if e.is_timeout() {
            Self::Timeout
        } else if e.is_invalid_input() {
            Self::BadRequest(e.to_string())
        } else if matches!(e, denon_marantz::Error::QueryNeedsTelnet) {
            // A query needs telnet. Telnet can be down for a short time: this is not a missing
            // feature.
            Self::ServiceUnavailable("This needs a telnet connection".into())
        } else if e.is_blocked() {
            Self::Blocked(e.to_string())
        } else if e.is_unsupported() {
            Self::NotSupported(e.to_string())
        } else {
            // Network errors and the rest.
            Self::ServiceUnavailable(e.to_string())
        }
    }
}

impl From<denon_marantz_heos::Error> for ServiceError {
    fn from(e: denon_marantz_heos::Error) -> Self {
        use denon_marantz_heos::Error;
        match e {
            Error::NotConnected => Self::NotConnected,
            Error::Timeout { .. } => Self::Timeout,
            Error::InvalidCommand(_) | Error::InvalidConfig(_) => Self::BadRequest(e.to_string()),
            // For example an item that cannot pause. The device gives the reason.
            Error::Failed { .. } => Self::Blocked(e.to_string()),
            _ => Self::ServiceUnavailable(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denon_marantz::Error;
    use std::time::Duration;

    #[test]
    fn a_query_over_http_is_not_a_missing_feature() {
        let e = ServiceError::from(Error::QueryNeedsTelnet);
        assert!(matches!(e, ServiceError::ServiceUnavailable(_)), "{e:?}");
    }

    #[test]
    fn a_refusal_of_the_state_is_blocked_with_the_reason() {
        let e = ServiceError::from(Error::DynamicEqOff);
        assert!(
            matches!(&e, ServiceError::Blocked(text) if text.contains("Dynamic EQ")),
            "{e:?}"
        );
    }

    #[test]
    fn a_stopped_receiver_is_not_found() {
        assert!(matches!(
            ServiceError::from(MailboxError::Closed),
            ServiceError::NotFound(_)
        ));
        assert_eq!(
            ServiceError::Timeout,
            ServiceError::from(MailboxError::Timeout)
        );
    }

    fn network(kind: ErrorKind, sent: bool) -> Error {
        Error::Network {
            request: "GET http://receiver/".into(),
            source: Box::new(std::io::Error::from(kind)),
            sent,
        }
    }

    #[test]
    fn a_failed_connect_is_unavailable() {
        for kind in [ErrorKind::NetworkUnreachable, ErrorKind::ConnectionRefused] {
            let e = ServiceError::from(network(kind, false));
            assert!(matches!(e, ServiceError::ServiceUnavailable(_)), "{e:?}");
        }
        let connect = Error::Timeout {
            request: "GET http://receiver/".into(),
            limit: crate::device::discovery::CONNECT_TIMEOUT,
            sent: false,
        };
        assert!(matches!(
            ServiceError::from(connect),
            ServiceError::ServiceUnavailable(_)
        ));
    }

    #[test]
    fn a_request_that_can_have_arrived_keeps_its_class() {
        let e = ServiceError::from(network(ErrorKind::ConnectionReset, true));
        assert!(matches!(e, ServiceError::ServiceUnavailable(_)), "{e:?}");
        let answer = Error::Timeout {
            request: "GET http://receiver/".into(),
            limit: Duration::from_secs(15),
            sent: true,
        };
        assert_eq!(ServiceError::Timeout, ServiceError::from(answer));
    }

    #[test]
    fn other_errors_keep_their_class() {
        assert_eq!(
            ServiceError::NotConnected,
            ServiceError::from(Error::NotConnected)
        );
    }
}
