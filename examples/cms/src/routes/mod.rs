//! HTTP routes.
//!
//! The split mirrors WordPress's own: [`front`] is the public site (the
//! "template hierarchy"), [`admin`] is wp-admin, [`api`] is the REST API,
//! [`feed`] is the syndication layer, and [`auth`] is the login screen they all
//! sit behind.

pub mod admin;
pub mod api;
pub mod auth;
pub mod comments;
pub mod feed;
pub mod front;
pub mod site;
