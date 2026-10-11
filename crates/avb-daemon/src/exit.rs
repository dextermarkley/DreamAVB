// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process exit codes (design §11.4). A single enum so every exit path names
//! its reason and the codes are pinned by a test.

/// The daemon's exit codes (§11.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Exit {
    Success = 0,
    Internal = 1,
    Usage = 2,
    ServiceNotFound = 3,
    AbiMismatch = 4,
    Link = 5,
    TalkerNotFound = 6,
    DextFault = 7,
    StreamRefused = 8,
    DiskOrPath = 9,
    Owned = 10,
    LocalLoss = 11,
    Interrupted = 130,
}

impl Exit {
    pub fn code(self) -> i32 {
        self as i32
    }

    /// A short human reason for logs.
    pub fn reason(self) -> &'static str {
        match self {
            Exit::Success => "success",
            Exit::Internal => "internal error",
            Exit::Usage => "usage",
            Exit::ServiceNotFound => "dext service AVBI210Driver not found",
            Exit::AbiMismatch => "ABI or shared-memory layout mismatch",
            Exit::Link => "no link at start, or link lost",
            Exit::TalkerNotFound => {
                "talker not found / ambiguous / ENTITY unreadable / no gPTP peer"
            }
            Exit::DextFault => "dext fault latched, or device removed",
            Exit::StreamRefused => "an audio stream was refused or failed",
            Exit::DiskOrPath => "output path or disk-space error",
            Exit::Owned => "data path owned by another client",
            Exit::LocalLoss => "local data loss during record",
            Exit::Interrupted => "interrupted by signal before record completed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_pinned() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Internal.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::ServiceNotFound.code(), 3);
        assert_eq!(Exit::AbiMismatch.code(), 4);
        assert_eq!(Exit::Link.code(), 5);
        assert_eq!(Exit::TalkerNotFound.code(), 6);
        assert_eq!(Exit::DextFault.code(), 7);
        assert_eq!(Exit::StreamRefused.code(), 8);
        assert_eq!(Exit::DiskOrPath.code(), 9);
        assert_eq!(Exit::Owned.code(), 10);
        assert_eq!(Exit::LocalLoss.code(), 11);
        assert_eq!(Exit::Interrupted.code(), 130);
    }
}
