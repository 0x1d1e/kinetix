//! Canonical lifecycle classification for an upstream stream attempt.
//!
//! Failure cause and retry policy remain owned by `types::FailureKind`.

use crate::types::FailureKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Completed,
    ClientCancelled,
    UpstreamCleanEof,
    UpstreamError,
    ProtocolViolation,
    Timeout,
    GatewayAbort,
}

impl StreamOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ClientCancelled => "client_cancelled",
            Self::UpstreamCleanEof => "upstream_clean_eof",
            Self::UpstreamError => "upstream_error",
            Self::ProtocolViolation => "protocol_violation",
            Self::Timeout => "timeout",
            Self::GatewayAbort => "gateway_abort",
        }
    }

    pub fn is_upstream_failure(self) -> bool {
        matches!(
            self,
            Self::UpstreamCleanEof | Self::UpstreamError | Self::ProtocolViolation | Self::Timeout
        )
    }

    pub fn request_status(self) -> &'static str {
        match self {
            Self::Completed => "success",
            Self::ClientCancelled => "client_disconnect",
            _ => "stream_error",
        }
    }

    pub fn status_code(self) -> i64 {
        match self {
            Self::Completed => 200,
            Self::ClientCancelled => 499,
            Self::Timeout => 504,
            Self::GatewayAbort => 500,
            _ => 502,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitState {
    PreCommit,
    PostCommit,
}

impl CommitState {
    /// Existing Route Trace values, retained for compatibility.
    pub fn as_trace_str(self) -> &'static str {
        match self {
            Self::PreCommit => "not_committed",
            Self::PostCommit => "committed",
        }
    }

    pub fn as_usage_str(self) -> &'static str {
        match self {
            Self::PreCommit => "pre_commit",
            Self::PostCommit => "post_commit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTermination {
    pub outcome: StreamOutcome,
    pub commit_state: CommitState,
    pub failure_kind: Option<FailureKind>,
    pub upstream_status: Option<u16>,
}

impl StreamTermination {
    pub fn new(
        outcome: StreamOutcome,
        commit_state: CommitState,
        failure_kind: Option<FailureKind>,
        upstream_status: Option<u16>,
    ) -> Self {
        Self {
            outcome,
            commit_state,
            failure_kind,
            upstream_status: failure_kind.and(upstream_status),
        }
    }

    pub fn completed(commit_state: CommitState) -> Self {
        Self::new(StreamOutcome::Completed, commit_state, None, None)
    }

    /// Fallback needs a retryable failure, an uncommitted response, and a
    /// positive decision from the existing Route fallback policy.
    pub fn fallback_allowed(self, route_allows: impl FnOnce(FailureKind) -> bool) -> bool {
        self.commit_state == CommitState::PreCommit
            && self.outcome.is_upstream_failure()
            && self
                .failure_kind
                .is_some_and(|kind| kind.is_retryable() && route_allows(kind))
    }

    pub fn request_status(self) -> &'static str {
        self.outcome.request_status()
    }

    pub fn status_code(self) -> i64 {
        self.outcome.status_code()
    }
}

#[cfg(test)]
mod tests {
    use super::{CommitState, StreamOutcome, StreamTermination};
    use crate::types::FailureKind;

    #[test]
    fn fallback_requires_precommit_retryable_failure_and_route_policy() {
        let timeout = StreamTermination::new(
            StreamOutcome::Timeout,
            CommitState::PreCommit,
            Some(FailureKind::Timeout),
            None,
        );
        assert!(timeout.fallback_allowed(|_| true));
        assert!(!timeout.fallback_allowed(|_| false));

        let committed = StreamTermination::new(
            StreamOutcome::Timeout,
            CommitState::PostCommit,
            Some(FailureKind::Timeout),
            None,
        );
        assert!(!committed.fallback_allowed(|_| true));

        let bad_request = StreamTermination::new(
            StreamOutcome::UpstreamError,
            CommitState::PreCommit,
            Some(FailureKind::BadRequest),
            Some(400),
        );
        assert!(!bad_request.fallback_allowed(|_| true));

        let cancelled = StreamTermination::new(
            StreamOutcome::ClientCancelled,
            CommitState::PostCommit,
            Some(FailureKind::ClientCancelled),
            None,
        );
        assert!(!cancelled.fallback_allowed(|_| true));
    }

    #[test]
    fn lifecycle_status_is_independent_of_failure_kind() {
        assert_eq!(StreamOutcome::Completed.request_status(), "success");
        assert_eq!(StreamOutcome::ClientCancelled.status_code(), 499);
        assert_eq!(StreamOutcome::Timeout.status_code(), 504);
        assert_eq!(StreamOutcome::GatewayAbort.status_code(), 500);
    }
}
