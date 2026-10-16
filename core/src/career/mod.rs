//! Career Intelligence & Resume Subsystem

pub mod career_engine;

pub use career_engine::{
    AuditLedgerRecord, CareerEngine, EntailmentVerification, MasterProfile, TailoredResumeRequest,
};

pub fn status() -> &'static str {
    "Career Engine Ready"
}

