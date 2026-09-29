### Fixed

- **CMS starter: Codex review findings from #2621 (issue #2661):**
  - *Restore ordering:* a post's discussion is now restored in the same
    transaction as its terms and status — before the status transition that
    can publish it — so the import never leaves the post publicly commentable
    with no discussion in between. Completion is recorded under a dedicated
    `_import_comments_restored` post-meta marker written in the same
    transaction as the comment rows; a retry consults the marker instead of
    treating any unrelated existing comment as a finished import, which used
    to drop the backup's whole thread. The marker is internal
    (`INTERNAL_META_KEYS`) and invisible to the meta editor.
  - *Parent validation:* `validate_parent` and `guard_page_path` now return
    `ParentCheck` — an expected refusal (`Decline`) stays distinguishable
    from an operational failure (`Err`). The importer's
    `set_post_parent` (and the creation-path prevalidation) apply
    `import_parent_outcome`: only an expected refusal drops the parent link;
    an operational failure fails the import and stays resumable instead of
    silently filing the page at the top level and marking it complete.
    Editor callers keep their 422 behavior via `ParentCheck::into_result`.
  - *Truncated replies:* `approved_reply_is_renderable` replays the
    thread-page helpers to decide whether an approved reply would actually
    appear on its page. `create_comment` refuses (422) an approved reply
    past the `MAX_THREAD_COMMENTS` render window instead of accepting a
    comment the site counts but no reader can reach; `moderate_comment`
    refuses to approve such a reply as well, and `import_comments` refuses
    to restore one — failing the restore (and rolling it back) rather than
    publishing a discussion with a hole in it. The check runs under the
    post's row lock, inside the transaction, so two concurrent replies
    cannot both see room for one.
  - *Moderation queue pager:* the queue screen now renders previous/next
    navigation with "Page X of Y", computed from the already-loaded count
    for the selected status; the requested page is clamped to the last page
    before querying, and both links preserve the `status` filter.
