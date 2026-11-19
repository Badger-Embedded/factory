//! The factory's dashboard and job store.

mod pages;
mod projects;
mod store;
mod web;

pub use projects::{Project, Projects, ProjectsError};
pub use store::{JobRow, Store, StoreError};
pub use web::{USER_HEADER, app};
