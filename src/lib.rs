#![allow(
    clippy::bind_instead_of_map,
    clippy::cmp_owned,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::double_ended_iterator_last,
    clippy::doc_lazy_continuation,
    clippy::field_reassign_with_default,
    clippy::match_like_matches_macro,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::while_let_loop
)]

pub mod activity;
mod ansi;
pub mod approvals;
pub mod cli;
pub mod config;
pub mod container;
pub mod desktop;
mod fs_util;
pub mod init;
pub mod manager;
pub mod native_approval;
pub mod new_project;
pub mod notifications;
mod process_util;
pub mod proxy;
pub mod rebuild;
pub mod rules;
pub mod server;
pub mod service;
pub mod shared_config;
pub mod shell;
pub mod state;
pub mod telemetry;
pub mod tui;
pub mod workspace;

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
