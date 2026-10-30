//! First-class multi-step form wizards with session-backed state.
//!
//! Orchestrates multi-step flows (onboarding, checkout, KYC) on top of the
//! existing [`crate::session::Session`], [`crate::form::Changeset`], and
//! [`crate::flash::Flash`] primitives — no new storage machinery.
//!
//! ## Session key format
//!
//! Each step's data is stored under:
//! ```text
//! __autumn_wizard:{wizard_name}:{step_name}
//! ```
//! Values are serde-JSON strings, so they round-trip through any
//! `SessionStore` backend (memory or Redis) without change.
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use autumn_web::wizard::{WizardContext, wizard_progress};
//!
//! // In a GET handler for step 1:
//! #[get("/checkout/shipping")]
//! async fn shipping_step(session: Session, csrf: CsrfToken) -> impl IntoResponse {
//!     let wizard = WizardContext::new(session, "checkout",
//!         vec!["shipping".into(), "payment".into(), "review".into()]);
//!     let data = wizard.step_data::<ShippingForm>("shipping").await
//!         .unwrap_or_default();
//!     let form = ChangesetForm::blank(data, csrf.token());
//!     html! {
//!         (wizard_progress(&wizard, "shipping").await)
//!         (form.form_tag("/checkout/shipping", "post", html! {
//!             (form.text_input("address", "Address"))
//!             (form.submit_button("Continue"))
//!         }))
//!     }
//! }
//!
//! // In a POST handler:
//! #[post("/checkout/shipping")]
//! async fn shipping_submit(session: Session, form: ChangesetForm<ShippingForm>)
//!     -> impl IntoResponse
//! {
//!     let wizard = WizardContext::new(session, "checkout",
//!         vec!["shipping".into(), "payment".into(), "review".into()]);
//!     match form.into_valid() {
//!         Ok(data) => {
//!             wizard.save_step("shipping", &data).await.unwrap();
//!             Redirect::to("/checkout/payment")
//!         }
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render_form(&form)).into_response(),
//!     }
//! }
//! ```

use serde::{Serialize, de::DeserializeOwned};

use crate::session::Session;

#[cfg(feature = "maud")]
use maud::{Markup, html};

/// Error type for wizard operations.
#[derive(Debug, thiserror::Error)]
pub enum WizardError {
    /// Failed to serialize step data to JSON.
    #[error("failed to serialize wizard step data: {0}")]
    Serialize(#[from] serde_json::Error),
}

// Session key prefix for wizard state.
const WIZARD_KEY_PREFIX: &str = "__autumn_wizard";

/// Context for a multi-step wizard flow.
///
/// Holds the session, wizard name, and ordered step list. Provides methods
/// for persisting per-step data, checking completion state, and performing
/// step-guard redirects.
///
/// Construct in each handler and pass around; it wraps the [`Session`] by
/// reference so it shares the same session handle.
#[derive(Clone, Debug)]
pub struct WizardContext {
    session: Session,
    name: String,
    steps: Vec<String>,
}

impl WizardContext {
    /// Create a new wizard context.
    ///
    /// `name` identifies the wizard (e.g. `"checkout"`).
    /// `steps` is the ordered list of step names (e.g. `["shipping", "payment", "review"]`).
    pub fn new(
        session: Session,
        name: impl Into<String>,
        steps: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            session,
            name: name.into(),
            steps: steps.into_iter().collect(),
        }
    }

    /// The total number of steps in this wizard.
    #[must_use]
    pub const fn total_steps(&self) -> usize {
        self.steps.len()
    }

    /// The 0-based index of the named step, or `None` if not found.
    #[must_use]
    pub fn step_index(&self, step: &str) -> Option<usize> {
        self.steps.iter().position(|s| s == step)
    }

    /// The 1-based step number for display (e.g. `"step 2 of 4"`), or `None`.
    #[must_use]
    pub fn step_number(&self, step: &str) -> Option<usize> {
        self.step_index(step).map(|i| i + 1)
    }

    /// An ordered slice of all step names.
    #[must_use]
    pub fn steps(&self) -> &[String] {
        &self.steps
    }

    /// The session key used to store `step`'s data.
    #[must_use]
    pub fn session_key(&self, step: &str) -> String {
        format!("{WIZARD_KEY_PREFIX}:{}:{}", self.name, step)
    }

    /// Returns `true` if `step` has valid, parseable data persisted in the session.
    ///
    /// Returns `false` if the key is absent or the stored value is no longer
    /// valid JSON (e.g. after a schema change), ensuring `first_incomplete_step`
    /// correctly routes users back through invalidated steps.
    pub async fn is_step_complete(&self, step: &str) -> bool {
        let key = self.session_key(step);
        let Some(json) = self.session.get(&key).await else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(&json).is_ok()
    }

    /// Returns the name of the first step that has *not* been completed, or
    /// `None` if all steps are complete.
    pub async fn first_incomplete_step(&self) -> Option<String> {
        for step in &self.steps {
            if !self.is_step_complete(step).await {
                return Some(step.clone());
            }
        }
        None
    }

    /// Guard for a step handler.
    ///
    /// If any earlier step is incomplete, returns `Err(redirect_url)` where
    /// the URL is `{base_path}/{first_incomplete_step}`.  When the current
    /// step is the first incomplete step (or all prior steps are complete),
    /// returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns `Err(redirect_url)` when a prior step is incomplete.
    ///
    /// # Usage
    ///
    /// ```rust,ignore
    /// if let Err(url) = wizard.guard_step("payment", "/checkout").await {
    ///     return Redirect::to(&url).into_response();
    /// }
    /// ```
    pub async fn guard_step(&self, current_step: &str, base_path: &str) -> Result<(), String> {
        let Some(current_idx) = self.step_index(current_step) else {
            return Ok(()); // unknown step — let the handler decide
        };
        let base_path = base_path.trim_end_matches('/');
        for step in &self.steps[..current_idx] {
            if !self.is_step_complete(step).await {
                return Err(format!("{base_path}/{step}"));
            }
        }
        Ok(())
    }

    /// Persist `data` for `step` into the session as a JSON string.
    ///
    /// Overwrites any previously saved value for that step; later steps'
    /// data is **not** discarded, supporting back-navigation re-submission.
    ///
    /// # Errors
    ///
    /// Returns [`WizardError::Serialize`] when `data` cannot be serialized.
    pub async fn save_step<T: Serialize + Sync>(
        &self,
        step: &str,
        data: &T,
    ) -> Result<(), WizardError> {
        let json = serde_json::to_string(data)?;
        let key = self.session_key(step);
        self.session.insert(key, json).await;
        Ok(())
    }

    /// Load the persisted data for `step` from the session.
    ///
    /// Returns `None` when the step has not yet been completed or when
    /// deserialization fails (treat as not-started).
    pub async fn step_data<T: DeserializeOwned>(&self, step: &str) -> Option<T> {
        let key = self.session_key(step);
        let json = self.session.get(&key).await?;
        serde_json::from_str(&json).ok()
    }

    /// Produce a stable key for storing this wizard's draft in an external
    /// data store (database, Redis, etc.).
    ///
    /// `user_key` scopes the draft to a specific user — typically a user ID
    /// converted to a string (`&user_id.to_string()`).
    ///
    /// The returned string has the form `wizard:{name}:{user_key}` and is safe
    /// to use as a database column value, cache key, or filename.
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// let key = wizard.draft_key(&current_user.id.to_string());
    /// db.upsert_wizard_draft(&key, &wizard.export_draft().await.to_string()).await?;
    /// ```
    #[must_use]
    pub fn draft_key(&self, user_key: &str) -> String {
        format!("wizard:{}:{}", self.name, user_key)
    }

    /// Serialize all completed steps into a single JSON object for external storage.
    ///
    /// Returns a `serde_json::Value::Object` keyed by step name.  Steps that
    /// have not been saved yet are omitted.  The value is the canonical draft
    /// format consumed by [`restore_draft`](Self::restore_draft).
    ///
    /// Store the result with [`draft_key`](Self::draft_key); restore it on the
    /// user's next visit with [`restore_draft`](Self::restore_draft).
    ///
    /// ## Example
    ///
    /// ```rust,ignore
    /// // In a POST handler or on a "save progress" button:
    /// let key   = wizard.draft_key(&user.id.to_string());
    /// let draft = wizard.export_draft().await;
    /// db.upsert("wizard_drafts", &key, &draft.to_string()).await?;
    /// ```
    #[must_use]
    pub async fn export_draft(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for step in &self.steps {
            let key = self.session_key(step);
            if let Some(json_str) = self.session.get(&key).await
                && let Ok(val) = serde_json::from_str(&json_str)
            {
                map.insert(step.clone(), val);
            }
        }
        serde_json::Value::Object(map)
    }

    /// Restore wizard step data from a previously exported draft.
    ///
    /// For each step in the wizard, if the session does **not** already have
    /// data for that step but the draft does, the draft data is written back
    /// into the session.  Steps that are already complete in the current
    /// session are left unchanged — **session takes precedence over draft**.
    ///
    /// Returns the number of steps restored into the session.
    ///
    /// Call this on the first GET handler when the session is empty, after
    /// loading the stored draft from your data store:
    ///
    /// ```rust,ignore
    /// #[get("/checkout/shipping")]
    /// async fn show_shipping(session: Session, current_user: CurrentUser) -> impl IntoResponse {
    ///     let wizard = wizard_context(session.clone());
    ///
    ///     // Restore a saved draft if the session is empty.
    ///     if wizard.first_incomplete_step().await.as_deref() == Some("shipping") {
    ///         let key = wizard.draft_key(&current_user.id.to_string());
    ///         if let Some(json) = db.load_wizard_draft(&key).await? {
    ///             let draft: serde_json::Value = serde_json::from_str(&json)?;
    ///             wizard.restore_draft(&draft).await;
    ///         }
    ///     }
    ///     // ... render the form as usual
    /// }
    /// ```
    pub async fn restore_draft(&self, draft: &serde_json::Value) -> usize {
        let Some(map) = draft.as_object() else {
            return 0;
        };
        let mut restored = 0;
        for step in &self.steps {
            if !self.is_step_complete(step).await
                && let Some(val) = map.get(step.as_str())
                && let Ok(json) = serde_json::to_string(val)
            {
                let key = self.session_key(step);
                self.session.insert(key, json).await;
                restored += 1;
            }
        }
        restored
    }

    /// Remove all wizard-state keys for this wizard from the session.
    ///
    /// Call on `commit` (successful completion) or `cancel`.  Abandoned
    /// wizards expire with the session automatically — no separate TTL needed.
    pub async fn clear(&self) {
        for step in &self.steps {
            let key = self.session_key(step);
            self.session.remove(&key).await;
        }
    }

    /// Convenience: load data for all steps at once.
    ///
    /// Returns a `Vec<Option<serde_json::Value>>` (one entry per step in
    /// declaration order).  Use [`step_data`](Self::step_data) when you need
    /// a typed value for a specific step.
    pub async fn all_step_data_json(&self) -> Vec<Option<serde_json::Value>> {
        let mut out = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            let key = self.session_key(step);
            let val = self
                .session
                .get(&key)
                .await
                .and_then(|j| serde_json::from_str(&j).ok());
            out.push(val);
        }
        out
    }
}

// ── Progress/stepper rendering ─────────────────────────────────────

/// The display state of a single step in the stepper UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// All required data was saved and the step is done.
    Completed,
    /// The step the user is currently on.
    Current,
    /// The step has not been reached yet.
    Upcoming,
}

/// A single step descriptor for the progress/stepper helper.
#[derive(Debug, Clone)]
pub struct WizardProgressStep {
    /// Step name (matches the name in [`WizardContext`]).
    pub name: String,
    /// Human-readable label (title-cased step name by default).
    pub label: String,
    /// Display state.
    pub state: StepState,
}

/// Compute the progress steps for rendering a stepper widget.
///
/// Each step's state is determined by checking the session, so this is
/// `async`.  Call [`wizard_progress`] (with the `maud` feature) to render
/// directly, or use the returned `Vec` to build a custom template.
pub async fn wizard_progress_steps(
    ctx: &WizardContext,
    current_step: &str,
) -> Vec<WizardProgressStep> {
    let current_idx = ctx.step_index(current_step);
    let mut result = Vec::with_capacity(ctx.steps.len());
    for (i, step) in ctx.steps.iter().enumerate() {
        let state = match current_idx {
            Some(cur) if i < cur => StepState::Completed,
            Some(cur) if i == cur => StepState::Current,
            _ => {
                if ctx.is_step_complete(step).await {
                    StepState::Completed
                } else {
                    StepState::Upcoming
                }
            }
        };
        result.push(WizardProgressStep {
            name: step.clone(),
            label: title_case(step),
            state,
        });
    }
    result
}

/// Render an accessible step-progress indicator as Maud [`Markup`].
///
/// Reads the step states from the session, then calls
/// [`wizard_progress_from_steps`].
///
/// ```html
/// <nav aria-label="Progress">
///   <ol role="list" class="autumn-wizard-progress">
///     <li class="autumn-wizard-step autumn-wizard-step--completed">…Shipping…</li>
///     <li class="autumn-wizard-step autumn-wizard-step--current" aria-current="step">…Payment…</li>
///     <li class="autumn-wizard-step autumn-wizard-step--upcoming">…Review…</li>
///   </ol>
/// </nav>
/// ```
#[cfg(feature = "maud")]
pub async fn wizard_progress(ctx: &WizardContext, current_step: &str) -> Markup {
    wizard_progress_from_steps(&wizard_progress_steps(ctx, current_step).await)
}

/// Render the step-progress indicator from steps you already have.
///
/// This function is not async. [`wizard_progress`] calls it. Use it when
/// the step states do not come from a session, for example in a story or a
/// test.
///
/// Emits a `<nav>` that holds an `<ol role="list">`, with one `<li>` for
/// each step. The step in [`StepState::Current`] gets
/// `aria-current="step"`. The summary shows "Step N of M". N is the
/// position of the current step. If no step is current, N is 1.
#[cfg(feature = "maud")]
#[must_use]
pub fn wizard_progress_from_steps(steps: &[WizardProgressStep]) -> Markup {
    let total = steps.len();
    let current_number = steps
        .iter()
        .position(|step| step.state == StepState::Current)
        .map_or(1, |i| i + 1);

    html! {
        nav aria-label="Progress" {
            ol role="list" class="autumn-wizard-progress" {
                @for (i, step) in steps.iter().enumerate() {
                    @let step_num = i + 1;
                    @let class = match step.state {
                        StepState::Completed => "autumn-wizard-step autumn-wizard-step--completed",
                        StepState::Current   => "autumn-wizard-step autumn-wizard-step--current",
                        StepState::Upcoming  => "autumn-wizard-step autumn-wizard-step--upcoming",
                    };
                    @if step.state == StepState::Current {
                        li class=(class) aria-current="step" {
                            span class="autumn-wizard-step__number" { (step_num) }
                            span class="autumn-wizard-step__label" { (step.label) }
                        }
                    } @else {
                        li class=(class) {
                            span class="autumn-wizard-step__number" { (step_num) }
                            span class="autumn-wizard-step__label" { (step.label) }
                        }
                    }
                }
            }
            p class="autumn-wizard-progress__summary" aria-live="polite" {
                "Step " (current_number) " of " (total)
            }
        }
    }
}

/// Convert a `snake_case` or `kebab-case` step name into a Title Case label.
fn title_case(s: &str) -> String {
    s.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map_or_else(String::new, |f| {
                f.to_uppercase().collect::<String>() + c.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    fn make_session() -> Session {
        Session::new_for_test("test-session-id".to_string(), HashMap::new())
    }

    fn make_wizard(session: Session) -> WizardContext {
        WizardContext::new(
            session,
            "checkout",
            vec!["shipping".into(), "payment".into(), "review".into()],
        )
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct ShippingData {
        address: String,
        city: String,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct PaymentData {
        card_last4: String,
    }

    // ── session key format ─────────────────────────────────────────

    #[test]
    fn session_key_uses_namespaced_format() {
        let wizard = make_wizard(make_session());
        assert_eq!(
            wizard.session_key("shipping"),
            "__autumn_wizard:checkout:shipping"
        );
        assert_eq!(
            wizard.session_key("payment"),
            "__autumn_wizard:checkout:payment"
        );
    }

    // ── step metadata ──────────────────────────────────────────────

    #[test]
    fn total_steps_returns_correct_count() {
        let wizard = make_wizard(make_session());
        assert_eq!(wizard.total_steps(), 3);
    }

    #[test]
    fn step_index_returns_zero_based_position() {
        let wizard = make_wizard(make_session());
        assert_eq!(wizard.step_index("shipping"), Some(0));
        assert_eq!(wizard.step_index("payment"), Some(1));
        assert_eq!(wizard.step_index("review"), Some(2));
        assert_eq!(wizard.step_index("missing"), None);
    }

    #[test]
    fn step_number_returns_one_based_display_number() {
        let wizard = make_wizard(make_session());
        assert_eq!(wizard.step_number("shipping"), Some(1));
        assert_eq!(wizard.step_number("payment"), Some(2));
        assert_eq!(wizard.step_number("review"), Some(3));
        assert_eq!(wizard.step_number("unknown"), None);
    }

    // ── save and load ──────────────────────────────────────────────

    #[tokio::test]
    async fn save_and_load_step_data_roundtrip() {
        let wizard = make_wizard(make_session());
        let data = ShippingData {
            address: "123 Main St".into(),
            city: "Springfield".into(),
        };
        wizard.save_step("shipping", &data).await.unwrap();
        let loaded: Option<ShippingData> = wizard.step_data("shipping").await;
        assert_eq!(loaded, Some(data));
    }

    #[tokio::test]
    async fn step_data_returns_none_for_unset_step() {
        let wizard = make_wizard(make_session());
        let loaded: Option<ShippingData> = wizard.step_data("shipping").await;
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn save_step_overwrites_without_discarding_other_steps() {
        let wizard = make_wizard(make_session());
        let shipping = ShippingData {
            address: "First".into(),
            city: "A".into(),
        };
        let payment = PaymentData {
            card_last4: "4242".into(),
        };

        wizard.save_step("shipping", &shipping).await.unwrap();
        wizard.save_step("payment", &payment).await.unwrap();

        // Update shipping
        let shipping2 = ShippingData {
            address: "Second".into(),
            city: "B".into(),
        };
        wizard.save_step("shipping", &shipping2).await.unwrap();

        // Payment is still there
        let loaded_payment: Option<PaymentData> = wizard.step_data("payment").await;
        assert_eq!(
            loaded_payment.as_ref().map(|p| p.card_last4.as_str()),
            Some("4242")
        );

        // Shipping is updated
        let loaded_shipping: Option<ShippingData> = wizard.step_data("shipping").await;
        assert_eq!(
            loaded_shipping.as_ref().map(|s| s.address.as_str()),
            Some("Second")
        );
    }

    // ── completion check ───────────────────────────────────────────

    #[tokio::test]
    async fn is_step_complete_reflects_session_state() {
        let wizard = make_wizard(make_session());
        assert!(!wizard.is_step_complete("shipping").await);

        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();

        assert!(wizard.is_step_complete("shipping").await);
    }

    #[tokio::test]
    async fn first_incomplete_step_returns_first_missing() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            wizard.first_incomplete_step().await.as_deref(),
            Some("payment")
        );
    }

    #[tokio::test]
    async fn first_incomplete_step_returns_none_when_all_complete() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();
        wizard
            .save_step(
                "payment",
                &PaymentData {
                    card_last4: "4242".into(),
                },
            )
            .await
            .unwrap();
        wizard
            .save_step("review", &serde_json::json!({"confirmed": true}))
            .await
            .unwrap();

        assert!(wizard.first_incomplete_step().await.is_none());
    }

    // ── step guards ────────────────────────────────────────────────

    #[tokio::test]
    async fn guard_step_allows_first_step_unconditionally() {
        let wizard = make_wizard(make_session());
        // shipping is step 0 — no prior steps, always allowed
        assert!(wizard.guard_step("shipping", "/checkout").await.is_ok());
    }

    #[tokio::test]
    async fn guard_step_blocks_step_2_when_step_1_incomplete() {
        let wizard = make_wizard(make_session());
        // payment is step 1 — shipping (step 0) must be complete first
        let result = wizard.guard_step("payment", "/checkout").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "/checkout/shipping");
    }

    #[tokio::test]
    async fn guard_step_allows_step_2_when_step_1_complete() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();
        assert!(wizard.guard_step("payment", "/checkout").await.is_ok());
    }

    #[tokio::test]
    async fn guard_step_blocks_step_3_when_step_2_incomplete() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();
        // review is step 2 — payment (step 1) must be complete
        let result = wizard.guard_step("review", "/checkout").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "/checkout/payment");
    }

    // ── clear ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn clear_removes_all_wizard_keys() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();
        wizard
            .save_step(
                "payment",
                &PaymentData {
                    card_last4: "4242".into(),
                },
            )
            .await
            .unwrap();

        wizard.clear().await;

        assert!(!wizard.is_step_complete("shipping").await);
        assert!(!wizard.is_step_complete("payment").await);
    }

    #[tokio::test]
    async fn clear_does_not_remove_other_session_keys() {
        let session = make_session();
        session.insert("unrelated_key", "keep_me").await;
        let wizard = make_wizard(session.clone());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();

        wizard.clear().await;

        assert_eq!(
            session.get("unrelated_key").await.as_deref(),
            Some("keep_me")
        );
    }

    // ── all_step_data_json ─────────────────────────────────────────

    #[tokio::test]
    async fn all_step_data_json_returns_none_for_missing_steps() {
        let wizard = make_wizard(make_session());
        let all = wizard.all_step_data_json().await;
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn all_step_data_json_returns_data_for_completed_steps() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();

        let all = wizard.all_step_data_json().await;
        assert!(all[0].is_some()); // shipping
        assert!(all[1].is_none()); // payment
        assert!(all[2].is_none()); // review
    }

    // ── draft persistence helpers ──────────────────────────────────

    #[test]
    fn draft_key_is_namespaced() {
        let wizard = make_wizard(make_session());
        assert_eq!(wizard.draft_key("42"), "wizard:checkout:42");
        assert_eq!(wizard.draft_key("user-abc"), "wizard:checkout:user-abc");
    }

    #[tokio::test]
    async fn export_draft_only_includes_complete_steps() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "1 Main St".into(),
                    city: "Springfield".into(),
                },
            )
            .await
            .unwrap();

        let draft = wizard.export_draft().await;
        let obj = draft.as_object().unwrap();
        assert!(obj.contains_key("shipping"), "shipping should be in draft");
        assert!(
            !obj.contains_key("payment"),
            "payment not saved — must be absent"
        );
        assert!(
            !obj.contains_key("review"),
            "review not saved — must be absent"
        );
    }

    #[tokio::test]
    async fn export_draft_is_keyed_by_step_name() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "1 Main St".into(),
                    city: "Springfield".into(),
                },
            )
            .await
            .unwrap();
        wizard
            .save_step(
                "payment",
                &PaymentData {
                    card_last4: "4242".into(),
                },
            )
            .await
            .unwrap();

        let draft = wizard.export_draft().await;
        let obj = draft.as_object().unwrap();
        assert_eq!(obj["shipping"]["address"], "1 Main St");
        assert_eq!(obj["payment"]["card_last4"], "4242");
    }

    #[tokio::test]
    async fn restore_draft_fills_empty_session_steps() {
        let wizard = make_wizard(make_session());
        let draft = serde_json::json!({
            "shipping": { "address": "Restored St", "city": "ReturnCity" },
            "payment":  { "card_last4": "9999" }
        });

        let count = wizard.restore_draft(&draft).await;
        assert_eq!(count, 2);
        assert!(wizard.is_step_complete("shipping").await);
        assert!(wizard.is_step_complete("payment").await);

        let shipping: Option<ShippingData> = wizard.step_data("shipping").await;
        assert_eq!(shipping.unwrap().address, "Restored St");
    }

    #[tokio::test]
    async fn restore_draft_skips_already_complete_steps() {
        let wizard = make_wizard(make_session());
        // Pre-fill shipping in the session.
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "Current".into(),
                    city: "Here".into(),
                },
            )
            .await
            .unwrap();

        // Draft has different shipping data.
        let draft = serde_json::json!({
            "shipping": { "address": "Stale", "city": "Elsewhere" },
            "payment":  { "card_last4": "1111" }
        });

        let count = wizard.restore_draft(&draft).await;
        // Only payment should be restored; shipping is already in session.
        assert_eq!(count, 1);
        let shipping: Option<ShippingData> = wizard.step_data("shipping").await;
        assert_eq!(
            shipping.unwrap().address,
            "Current",
            "session must win over draft"
        );
    }

    #[tokio::test]
    async fn restore_draft_ignores_unknown_step_names() {
        let wizard = make_wizard(make_session());
        // Draft contains a step name not in this wizard.
        let draft = serde_json::json!({
            "shipping":    { "address": "A", "city": "B" },
            "nonexistent": { "foo": "bar" }
        });

        let count = wizard.restore_draft(&draft).await;
        assert_eq!(count, 1, "only known steps should be restored");
        assert!(wizard.is_step_complete("shipping").await);
        assert!(!wizard.is_step_complete("nonexistent").await);
    }

    #[tokio::test]
    async fn export_restore_roundtrip_preserves_all_step_data() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "42 Loop".into(),
                    city: "Rustville".into(),
                },
            )
            .await
            .unwrap();
        wizard
            .save_step(
                "payment",
                &PaymentData {
                    card_last4: "1234".into(),
                },
            )
            .await
            .unwrap();

        // Export, then restore into a brand new session.
        let draft = wizard.export_draft().await;
        let wizard2 = make_wizard(make_session());
        wizard2.restore_draft(&draft).await;

        let shipping: Option<ShippingData> = wizard2.step_data("shipping").await;
        assert_eq!(shipping.unwrap().city, "Rustville");
        let payment: Option<PaymentData> = wizard2.step_data("payment").await;
        assert_eq!(payment.unwrap().card_last4, "1234");
    }

    #[tokio::test]
    async fn restore_draft_returns_zero_for_non_object_json() {
        let wizard = make_wizard(make_session());
        let count = wizard
            .restore_draft(&serde_json::json!("not an object"))
            .await;
        assert_eq!(count, 0);
        let count = wizard.restore_draft(&serde_json::json!(null)).await;
        assert_eq!(count, 0);
    }

    // ── title_case helper ──────────────────────────────────────────

    #[test]
    fn title_case_converts_snake_case() {
        assert_eq!(title_case("shipping"), "Shipping");
        assert_eq!(title_case("billing_address"), "Billing Address");
        assert_eq!(title_case("payment_method"), "Payment Method");
    }

    #[test]
    fn title_case_converts_kebab_case() {
        assert_eq!(title_case("payment-method"), "Payment Method");
    }

    // ── progress steps ─────────────────────────────────────────────

    #[tokio::test]
    async fn wizard_progress_steps_marks_current_and_upcoming() {
        let wizard = make_wizard(make_session());
        let steps = wizard_progress_steps(&wizard, "shipping").await;
        assert_eq!(steps[0].state, StepState::Current);
        assert_eq!(steps[1].state, StepState::Upcoming);
        assert_eq!(steps[2].state, StepState::Upcoming);
    }

    #[tokio::test]
    async fn wizard_progress_steps_marks_prior_steps_completed() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();

        let steps = wizard_progress_steps(&wizard, "payment").await;
        assert_eq!(steps[0].state, StepState::Completed);
        assert_eq!(steps[1].state, StepState::Current);
        assert_eq!(steps[2].state, StepState::Upcoming);
    }

    #[tokio::test]
    async fn wizard_progress_steps_labels_use_title_case() {
        let wizard = make_wizard(make_session());
        let steps = wizard_progress_steps(&wizard, "shipping").await;
        assert_eq!(steps[0].label, "Shipping");
        assert_eq!(steps[1].label, "Payment");
        assert_eq!(steps[2].label, "Review");
    }

    // ── maud rendering ─────────────────────────────────────────────

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn wizard_progress_renders_aria_current_on_current_step() {
        let wizard = make_wizard(make_session());
        let markup = wizard_progress(&wizard, "shipping").await;
        let html = markup.into_string();
        assert!(html.contains("aria-current=\"step\""));
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn wizard_progress_renders_step_count_summary() {
        let wizard = make_wizard(make_session());
        let markup = wizard_progress(&wizard, "payment").await;
        let html = markup.into_string();
        assert!(html.contains("Step 2 of 3"));
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn wizard_progress_renders_all_step_labels() {
        let wizard = make_wizard(make_session());
        let markup = wizard_progress(&wizard, "shipping").await;
        let html = markup.into_string();
        assert!(html.contains("Shipping"));
        assert!(html.contains("Payment"));
        assert!(html.contains("Review"));
    }

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn wizard_progress_applies_correct_css_classes() {
        let wizard = make_wizard(make_session());
        wizard
            .save_step(
                "shipping",
                &ShippingData {
                    address: "A".into(),
                    city: "B".into(),
                },
            )
            .await
            .unwrap();
        let markup = wizard_progress(&wizard, "payment").await;
        let html = markup.into_string();
        assert!(html.contains(r#"class="autumn-wizard-progress""#), "{html}");
        assert!(html.contains("autumn-wizard-step autumn-wizard-step--completed"));
        assert!(html.contains("autumn-wizard-step autumn-wizard-step--current"));
        assert!(html.contains("autumn-wizard-step autumn-wizard-step--upcoming"));
        assert!(
            html.contains(r#"class="autumn-wizard-step__number""#),
            "{html}"
        );
        assert!(!html.contains(r#"class="wizard-"#), "{html}");
    }

    // ── wizard_progress_from_steps (#3398) ───────────────────────────────

    #[cfg(feature = "maud")]
    #[tokio::test]
    async fn wizard_progress_from_steps_matches_wizard_progress() {
        let wizard = make_wizard(make_session());
        for current in ["shipping", "payment", "review", "unknown"] {
            let steps = wizard_progress_steps(&wizard, current).await;
            assert_eq!(
                wizard_progress_from_steps(&steps).into_string(),
                wizard_progress(&wizard, current).await.into_string(),
                "current step {current}"
            );
        }
    }

    #[cfg(feature = "maud")]
    #[test]
    fn wizard_progress_from_steps_renders_fixed_steps() {
        let step = |name: &str, state| WizardProgressStep {
            name: name.into(),
            label: title_case(name),
            state,
        };
        let html = wizard_progress_from_steps(&[
            step("shipping", StepState::Completed),
            step("payment", StepState::Current),
            step("review", StepState::Upcoming),
        ])
        .into_string();
        assert!(html.contains(r#"aria-current="step""#), "{html}");
        assert!(html.contains("Step 2 of 3"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn wizard_progress_from_steps_with_no_current_step_marks_none() {
        let step = WizardProgressStep {
            name: "shipping".into(),
            label: "Shipping".into(),
            state: StepState::Upcoming,
        };
        let html = wizard_progress_from_steps(&[step]).into_string();
        assert!(!html.contains("aria-current"), "{html}");
        assert!(html.contains("Step 1 of 1"), "{html}");
    }
}
