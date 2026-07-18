//! Shared UI primitives for framework-owned HTML surfaces.
//!
//! Holds design tokens that actuator, error pages, and first-party plugins
//! (e.g. `autumn-admin-plugin`) all reference so the visual language stays
//! consistent, plus reusable Maud renderers like the
//! [`pagination`] pager control that apps and plugins share.

/// Reusable Maud pagination-nav renderers (offset and cursor).
#[cfg(feature = "maud")]
pub mod pagination;
pub mod tokens;
/// Framework-owned widget component CSS — see [`widgets_css`] for docs.
///
/// Gated on `maud` like every module it backs (`form`, `widgets`, `wizard`,
/// `pagination`, `storage::form_helper`, `job_tracking`'s render helpers) —
/// there's no configuration where a widget emits an `autumn-*`/`wizard-*`
/// class without maud, so the CSS serving it is scoped the same way instead
/// of being reachable-but-unrouted.
#[cfg(feature = "maud")]
mod widgets_css;

#[cfg(feature = "maud")]
pub use widgets_css::{WIDGETS_COMPONENT_CSS, WIDGETS_CSS, WIDGETS_CSS_PATH};
