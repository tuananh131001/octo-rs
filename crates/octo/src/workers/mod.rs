//! Background workers (the C# `BackgroundService`s) and the supervisor that hosts them.

pub mod supervisor;

pub use supervisor::{
    Backoff, SHUTDOWN_TIMEOUT, ShutdownReport, WorkerState, WorkerStatus, WorkerSupervisor,
};
