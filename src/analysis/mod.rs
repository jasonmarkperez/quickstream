//! Intelligent Autopilot Mix Engine: acoustic analysis, harmonic mixing, and feature caching.

pub mod db;
pub mod harmonic;
pub mod bpm;
pub mod mixer;
pub mod online;

pub use db::{AnalysisDatabase, TrackFeatures};
pub use harmonic::{CamelotKey, MusicalKey};
pub use mixer::{MixProfile, MoodType};
