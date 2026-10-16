# Plan — `autumn upgrade` for workspace members (#2469)

Issue [#2469](https://github.com/autumn-foundation/autumn/issues/2469).
This is the design record. Write it before the code.

## Problem

A workspace member cannot adopt `clippy.toml`, `rustfmt.toml`,
`rust-toolchain.toml`, `ci.yml` and `posture-gate.yml` with `autumn upgrade`.
The command removes them from the plan. Nothing writes them at the root.
The deploy templates also skip the posture attestation without a message when
the manifest is not at the repository root.

## 1. Brainstorming

| # | Option | Verdict |
|---|---|---|
| B1 | Keep the files out of scope. Only print the manual steps. | Rejected. It is the bug. |
| B2 | Write the files into the member and tell the user to move them. | Rejected. A member `clippy.toml` shadows the root one. A member workflow never runs. |
| B3 | Plan the files at the real owner (workspace root or git root). Use a `../` path from the member. | **Chosen.** It reuses the classifier, the digests, `--accept` and the write guards. |
| B4 | Add a new "root" manifest at the workspace root. | Rejected. Two baselines for one project. |
| B5 | Pass `-p <member>` to every `cargo` command in `ci.yml`. | Rejected. Many edits, easy to miss one. |
| B6 | Set `defaults.run.working-directory` in `ci.yml`. | **Chosen.** One edit. `cargo` picks the member by itself. |
| B7 | Put the member path in `POSTURE_MANIFEST` and `-p` in `POSTURE_AUDIT_FLAGS`. | **Chosen.** Both variables already drive every step and every message. |
| B8 | One workflow file per member. | Deferred. It changes check names and the PR comment marker. |
| B9 | Share one file. The first member owns it. | **Chosen.** Other members see a conflict and can `--accept`. |

## 2. Reverse brainstorming — how can this change fail?

| Failure | Guard |
|---|---|
| Write a file outside the repository. | Target must be inside the member's git repository. No git repository: plan nothing. |
| Overwrite the root file of another member. | A file with other content is a conflict. `apply` never writes a conflict. |
| Follow a symlink out of the tree. | The existing `read_current` link check covers `..` paths. |
| Claim a nested git repository's lint config. | The nested repository is its own git root. Cargo files stay out of scope. Its workflows stay local. |
| Plant a wrong workflow. | A template anchor that is missing or not unique plans no file. |
| Name the wrong member in the workflow. | Name comes from `[package] name`. The existing rule rejects bad names. |
| Silent attestation skip. | A `::warning::` step when the manifest is missing. No hard failure. |

## 3. Six thinking hats

- **White (facts).** `current_files` removes root files. `classify` keys are
  project-relative. GitHub reads workflows from the git root only.
  `hashFiles` and `subject-path` ignore `working-directory`.
- **Red (feelings).** A silent no-op looks like a pass. The user must see what
  was skipped and why.
- **Yellow (benefits).** One command adopts the gates. No new baseline format.
- **Black (risks).** Writes leave the member directory. Guards: section 2.
- **Green (new ideas).** Per-member file names for several members (B8).
  A matrix job. Both are future work.
- **Blue (process).** Red test, green code, refactor. Then review.

## Design

1. `root_owners(root)` returns the directory that owns each group of root files.
   Cargo files: the workspace root. Workflows: the git root.
   Rule: a target must be inside the member's git repository. No git
   repository: plan nothing. Workflows also need the git root to be the
   workspace root, and a plain directory path.
2. `current_files` returns `String` keys. A root file key is the relative path
   from the member, for example `../../clippy.toml`.
3. `new::member_workflow` edits a rendered workflow for one member.
   `ci.yml` gets `working-directory` and `cargo fmt -p`.
   `posture-gate.yml` gets the member manifest path and `-p`.
4. The report names what it plans at the root, in place of "out of scope".
5. Deploy templates: add a manifest-path variable and a warning step.
   `release.rs` names the variable in its relocation text.
6. Docs: replace the manual workaround in `posture-gate.md`.

## Out of scope

Per-member workflow names. A matrix workflow. Changing the `rust-cache`
`workspaces` input when the Cargo root is not the git root.
