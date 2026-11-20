//! The factory's dashboard and job store.

mod cost;
mod pages;
mod podman;
mod projects;
mod scheduler;
mod settings;
mod store;
mod web;

pub use cost::format_cost;
pub use podman::PodmanExecutor;
pub use projects::{Project, Projects, ProjectsError};
pub use scheduler::{
    AgentRequest, ChecksRequest, Executor, FINISHED_JOB_DAYS, LogSink, PublishRequest, Scheduler,
    SummaryRequest, WatchReport, WatchRequest, branch_name,
};
pub use settings::Settings;
pub use store::{JobRow, LogEntry, Store, StoreError};
pub use web::{MAX_IMAGE_BYTES, MAX_IMAGES, USER_HEADER, app};
