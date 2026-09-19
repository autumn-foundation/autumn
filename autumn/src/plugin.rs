//! Plugin trait for composable Autumn integrations.
//!
//! A [`Plugin`] encapsulates configuration and wiring for a reusable piece of
//! infrastructure (durable workflows, live feeds, telemetry exporters, etc.)
//! that attaches itself to an [`AppBuilder`]. Users register plugins with
//! [`AppBuilder::plugin`](crate::app::AppBuilder::plugin) or the tuple-taking
//! [`AppBuilder::plugins`](crate::app::AppBuilder::plugins); each plugin's
//! [`build`](Plugin::build) runs exactly once.
//!
//! # Naming conventions
//!
//! First-party plugin crates are named `autumn-<name>-plugin`. Third-party
//! crates are named `autumn-plugin-<name>` to keep names unambiguous on
//! crates.io. Each crate exposes a `<Name>Plugin` struct at its root with a
//! `::new()` constructor and `#[must_use]` fluent configuration methods.
//!
//! # Authoring a plugin
//!
//! ```rust,no_run
//! use autumn_web::app::AppBuilder;
//! use autumn_web::plugin::Plugin;
//!
//! pub struct HelloPlugin {
//!     greeting: String,
//! }
//!
//! impl HelloPlugin {
//!     #[must_use]
//!     pub fn new() -> Self {
//!         Self { greeting: "hello".to_owned() }
//!     }
//!
//!     #[must_use]
//!     pub fn greeting(mut self, greeting: impl Into<String>) -> Self {
//!         self.greeting = greeting.into();
//!         self
//!     }
//! }
//!
//! impl Plugin for HelloPlugin {
//!     fn build(self, app: AppBuilder) -> AppBuilder {
//!         let greeting = self.greeting;
//!         app.on_startup(move |_state| {
//!             let greeting = greeting.clone();
//!             async move {
//!                 tracing::info!(%greeting, "hello plugin started");
//!                 Ok(())
//!             }
//!         })
//!     }
//! }
//! ```
//!
//! # The stability contract
//!
//! A plugin can declare the `autumn-web` range it supports and the
//! experimental surface it leans on by implementing [`Plugin::contract`]. An
//! incompatible pairing fails loudly when the plugin is registered, naming both
//! versions; `autumn plugin-check` reports experimental dependence. See
//! [`plugin_contract`](crate::plugin_contract) for the declared tiers and the
//! `SemVer` policy behind each one.
//!
//! # Duplicate registration
//!
//! Registering two plugins that share the same [`Plugin::name`] is a no-op
//! after the first: the second call emits a `tracing::warn!` and returns the
//! builder unchanged. The default name is [`std::any::type_name`] of the
//! plugin struct, so two different instances of the same type collide by
//! default -- override [`Plugin::name`] if a plugin is genuinely designed to
//! be registered more than once.

use std::borrow::Cow;

use crate::app::AppBuilder;
use crate::plugin_contract::PluginContract;

/// A reusable Autumn integration that wires itself into an [`AppBuilder`].
///
/// See the [module-level documentation](self) for conventions and examples.
pub trait Plugin: Sized + Send + 'static {
    /// Stable identifier used for duplicate-registration detection.
    ///
    /// Defaults to [`std::any::type_name`] of the concrete plugin struct, so
    /// two instances of the same type collide by default. Override to allow
    /// multiple instances of the same type to coexist; the return type is
    /// [`Cow<'static, str>`](std::borrow::Cow) so plugins can compute a
    /// unique label from runtime configuration without leaking memory.
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(std::any::type_name::<Self>())
    }

    /// Apply this plugin's configuration to the builder.
    ///
    /// Called exactly once per `AppBuilder`. Implementations typically chain
    /// [`AppBuilder::on_startup`], [`AppBuilder::on_shutdown`],
    /// [`AppBuilder::nest`], [`AppBuilder::with_extension`] and (with the
    /// `db` feature) [`AppBuilder::migrations`].
    ///
    /// Plugins can also install **tier-1 subsystem replacements** here —
    /// [`AppBuilder::with_config_loader`], [`AppBuilder::with_pool_provider`]
    /// (with the `db` feature), [`AppBuilder::with_telemetry_provider`], and
    /// [`AppBuilder::with_session_store`] — which is the canonical way to
    /// distribute a custom subsystem (e.g. `AwsSecretsConfigPlugin`) for
    /// downstream consumers as a one-line install. See
    /// `docs/guide/extensibility.md` for the full extensibility model.
    #[must_use]
    fn build(self, app: AppBuilder) -> AppBuilder;

    /// Declare which `autumn-web` versions this plugin supports, and any
    /// experimental surface it knowingly depends on.
    ///
    /// Returning `None` (the default) declares nothing and changes no
    /// behaviour — every plugin written before the contract existed keeps
    /// working. Returning a contract whose
    /// [`autumn_web`](PluginContract::autumn_web) range excludes the framework
    /// this plugin is compiled into makes
    /// [`AppBuilder::plugin`](crate::app::AppBuilder::plugin) **panic at
    /// registration** with a diagnostic naming both versions and both
    /// remedies. That is deliberate: a silent mismatch is the failure mode
    /// this contract exists to remove.
    ///
    /// See [`plugin_contract`](crate::plugin_contract) for the stability tiers
    /// and the `SemVer` policy each one carries.
    ///
    /// ```rust
    /// # use autumn_web::app::AppBuilder;
    /// # use autumn_web::plugin::Plugin;
    /// use autumn_web::plugin_contract::PluginContract;
    ///
    /// # struct MyPlugin;
    /// impl Plugin for MyPlugin {
    ///     fn contract(&self) -> Option<PluginContract> {
    ///         Some(
    ///             PluginContract::new(env!("CARGO_PKG_NAME"))
    ///                 .plugin_version(env!("CARGO_PKG_VERSION"))
    ///                 .autumn_web("0.7"),
    ///         )
    ///     }
    ///
    ///     fn build(self, app: AppBuilder) -> AppBuilder { app }
    /// }
    /// ```
    fn contract(&self) -> Option<PluginContract> {
        None
    }
}

/// A bundle of plugins that can be applied to an [`AppBuilder`] in one call.
///
/// Implemented for every [`Plugin`] and for tuples of up to eight plugins.
/// Used by [`AppBuilder::plugins`](crate::app::AppBuilder::plugins).
pub trait Plugins: Sized {
    /// Apply every plugin in this bundle to the builder, in declaration order.
    #[must_use]
    fn apply(self, app: AppBuilder) -> AppBuilder;
}

impl<P: Plugin> Plugins for P {
    fn apply(self, app: AppBuilder) -> AppBuilder {
        app.plugin(self)
    }
}

macro_rules! impl_plugins_tuple {
    ($($idx:tt => $ty:ident),+ $(,)?) => {
        impl<$($ty: Plugin),+> Plugins for ($($ty,)+) {
            #[allow(non_snake_case)]
            fn apply(self, app: AppBuilder) -> AppBuilder {
                let ($($ty,)+) = self;
                let app = app;
                $(let app = app.plugin($ty);)+
                app
            }
        }
    };
}

impl_plugins_tuple!(0 => P0);
impl_plugins_tuple!(0 => P0, 1 => P1);
impl_plugins_tuple!(0 => P0, 1 => P1, 2 => P2);
impl_plugins_tuple!(0 => P0, 1 => P1, 2 => P2, 3 => P3);
impl_plugins_tuple!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4);
impl_plugins_tuple!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5);
impl_plugins_tuple!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6);
impl_plugins_tuple!(
    0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7
);

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self::default()
        }

        fn events(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .expect("lock shouldn't be poisoned")
                .clone()
        }

        fn push(&self, label: &'static str) {
            self.events
                .lock()
                .expect("lock shouldn't be poisoned")
                .push(label);
        }
    }

    struct RecordingPlugin {
        label: &'static str,
        recorder: Arc<Recorder>,
    }

    impl Plugin for RecordingPlugin {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed(self.label)
        }

        fn build(self, app: AppBuilder) -> AppBuilder {
            self.recorder.push(self.label);
            app
        }
    }

    struct ColaPlugin {
        recorder: Arc<Recorder>,
    }

    impl Plugin for ColaPlugin {
        fn build(self, app: AppBuilder) -> AppBuilder {
            self.recorder.push("cola");
            app
        }
    }

    struct PepsiPlugin {
        recorder: Arc<Recorder>,
    }

    impl Plugin for PepsiPlugin {
        fn build(self, app: AppBuilder) -> AppBuilder {
            self.recorder.push("pepsi");
            app
        }
    }

    #[test]
    fn single_plugin_builds_once() {
        let recorder = Arc::new(Recorder::new());
        let builder = crate::app::app().plugin(RecordingPlugin {
            label: "only",
            recorder: recorder.clone(),
        });

        assert_eq!(recorder.events(), vec!["only"]);
        assert!(builder.has_plugin("only"));
    }

    #[test]
    fn duplicate_named_plugin_is_skipped_with_warning() {
        let recorder = Arc::new(Recorder::new());
        let builder = crate::app::app()
            .plugin(RecordingPlugin {
                label: "dup",
                recorder: recorder.clone(),
            })
            .plugin(RecordingPlugin {
                label: "dup",
                recorder: recorder.clone(),
            });

        assert_eq!(recorder.events(), vec!["dup"]);
        assert!(builder.has_plugin("dup"));
    }

    #[test]
    fn single_plugin_applied_via_plugins_trait() {
        let recorder = Arc::new(Recorder::new());
        let builder = crate::app::app().plugins(RecordingPlugin {
            label: "single_via_trait",
            recorder: recorder.clone(),
        });

        assert_eq!(recorder.events(), vec!["single_via_trait"]);
        assert!(builder.has_plugin("single_via_trait"));
    }

    #[test]
    fn tuple_of_plugins_applies_in_declaration_order() {
        let recorder = Arc::new(Recorder::new());
        let _builder = crate::app::app().plugins((
            ColaPlugin {
                recorder: recorder.clone(),
            },
            PepsiPlugin {
                recorder: recorder.clone(),
            },
        ));

        assert_eq!(recorder.events(), vec!["cola", "pepsi"]);
    }
}
