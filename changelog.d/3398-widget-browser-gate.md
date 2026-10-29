### Added

- **wizard:** `wizard_progress_from_steps(&[WizardProgressStep])` renders the
  step progress from steps you already have, with no session (issue #3398).
  `wizard_progress` now calls it.
- **stories:** the `/_stories` gallery has stories for pagination, flash
  messages, wizard progress and the `a11y` primitives.

### Fixed

- **stories:** the code blocks in the Source and Rendered HTML panes take
  keyboard focus, so a keyboard user can scroll a wide snippet. The gallery
  links the flash stylesheet. The Avatar story no longer shows a broken
  image.
