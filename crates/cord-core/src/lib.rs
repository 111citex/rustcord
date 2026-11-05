//! Discord protocol and state. No UI code lives here.

pub mod auth;
pub mod build_number;
pub mod cache;
pub mod cdn;
mod client;
pub mod event;
pub mod gateway;
pub mod http;
pub mod id;
pub mod poll;
pub mod identity;
pub mod invite;
pub mod member_list;
pub mod model;
pub mod paths;
pub mod permissions;
pub mod profile;
pub mod rich;
pub mod settings_proto;

pub use crate::client::{API_BASE, Core, CoreConfig, startup_identity};
pub use id::Id;
