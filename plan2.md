1. **Extend `ProvideActuatorState` trait:** Add a `feature_flags(&self) -> Option<&crate::feature_flags::FeatureFlagService>` method in `autumn/src/actuator.rs`, with a default implementation returning `None`.
2. **Implement `feature_flags` in `AppState`:** Add this method to the `ProvideActuatorState` implementation in `autumn/src/state.rs` so that it returns the `FeatureFlagService` extension.
3. **Add `feature_flags_endpoint`:** In `autumn/src/actuator.rs`, implement `pub(crate) async fn feature_flags_endpoint` to handle GET requests to `/actuator/feature-flags` and return the list of flags as JSON. Wire this into `actuator_router_with_prefix` (available when `sensitive` is true).
4. **Update `autumn-cli/src/monitor.rs` structs:** Add a `FeatureFlagsResponse` struct (`#[derive(Debug, Deserialize, Default, Clone)]` holding a list of `FlagConfig`-like structs). Add `feature_flags` to `DashboardState`.
5. **Update `autumn-cli/src/monitor.rs` poll logic:** Implement `fetch_feature_flags` and call it from `DashboardState::poll`. Update `run_loop` to cycle through 5 tabs (`active_tab % 5`).
6. **Update `autumn-cli/src/monitor.rs` drawing logic:** In `draw_header`, add `"Feature Flags"` to `tab_titles`. In `draw`, call `draw_feature_flags_tab(frame, main_chunks[1], state)` for `state.active_tab == 4`. Implement `draw_feature_flags_tab` using a `Table` widget.
7. **Verification:** Run `cargo check --all-targets --all-features` and `git diff` to ensure changes are correct.
8. **Testing:** Run `cargo test -p autumn-web --lib` and `cargo test -p autumn-cli --lib` to ensure no regressions.
9. **Pre-commit:** Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.
10. **Submit PR:** Submit the change with Title: `🌟 Nova: Feature Flags Monitor TUI` and the appropriate description fields.
