//! Natural Language File Organizer Subsystem

pub mod organizer_engine;

pub use organizer_engine::{FileCategory, FileOrganizerEngine, OrganizeReport};

pub fn status() -> &'static str {
    "File Organizer Ready"
}
