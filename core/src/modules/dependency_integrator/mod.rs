//! Autonomous Dependency Integrator Module

pub mod auto_integrator;

pub use auto_integrator::{
    AutoIntegrator, CandidateDependency, CapabilityRequest, IntegrationResult, PackageRegistry,
};

pub fn status() -> &'static str {
    "Dependency Integrator Ready"
}

