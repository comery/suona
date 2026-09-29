//! suona — a desktop pet that reports what your local coding agents are doing.

pub mod agents;
pub mod app;
pub mod collectors;
pub mod crash;
pub mod model;
pub mod pet;
pub mod screens;

pub use app::run;
