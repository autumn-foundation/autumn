//! Roles and capabilities — WordPress's authorization model, as types.
//!
//! WordPress stores a user's capabilities as a serialized PHP array in
//! usermeta, seeded from their role at the moment the role was assigned. That
//! is why changing a role's definition does not retroactively change existing
//! users, and why a corrupted `wp_capabilities` blob silently strips someone's
//! access. Here the role is the only stored fact and the capability set is
//! *derived* from it on every check, so the two can never disagree.
//!
//! Every capability check in the app goes through [`Role::can`]. Nothing
//! compares role strings inline — a `role == "administrator"` scattered through
//! handlers is exactly how a permission check gets forgotten on the fifth
//! screen.

use std::fmt;

/// The five WordPress core roles, most privileged first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Somebody who can read the site and comment. The default for a new signup.
    Subscriber,
    /// Writes posts but cannot publish them; an editor reviews and publishes.
    Contributor,
    /// Writes and publishes their own posts, and uploads media.
    Author,
    /// Full control over all content, comments and taxonomies — but not over
    /// users, settings or the theme.
    Editor,
    /// Everything.
    Administrator,
}

/// A single permission. Names match WordPress's own capability slugs so the
/// mapping is checkable against its documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Read the site, including private content the user authored.
    Read,
    /// Create and edit one's own drafts.
    EditPosts,
    /// Edit content authored by somebody else.
    EditOthersPosts,
    /// Edit content that is already published.
    EditPublishedPosts,
    /// Move one's own content out of `draft` and into `publish`.
    PublishPosts,
    /// Trash one's own content.
    DeletePosts,
    /// Trash content authored by somebody else.
    DeleteOthersPosts,
    /// See content in `private` status that somebody else authored.
    ReadPrivatePosts,
    /// Create, rename and delete terms in any taxonomy.
    ManageCategories,
    /// Approve, unapprove, spam and trash comments.
    ModerateComments,
    /// Add files to the media library.
    UploadFiles,
    /// Change site settings.
    ManageOptions,
    /// Edit menus, widgets and the active theme's options.
    EditThemeOptions,
    /// See the user list.
    ListUsers,
    /// Create, edit and delete users, and change their roles.
    EditUsers,
    /// Run the exporter and the importer.
    ExportContent,
    /// Import content from an export file.
    ImportContent,
}

impl Capability {
    /// The capability's WordPress slug, for display in the admin UI.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::EditPosts => "edit_posts",
            Self::EditOthersPosts => "edit_others_posts",
            Self::EditPublishedPosts => "edit_published_posts",
            Self::PublishPosts => "publish_posts",
            Self::DeletePosts => "delete_posts",
            Self::DeleteOthersPosts => "delete_others_posts",
            Self::ReadPrivatePosts => "read_private_posts",
            Self::ManageCategories => "manage_categories",
            Self::ModerateComments => "moderate_comments",
            Self::UploadFiles => "upload_files",
            Self::ManageOptions => "manage_options",
            Self::EditThemeOptions => "edit_theme_options",
            Self::ListUsers => "list_users",
            Self::EditUsers => "edit_users",
            Self::ExportContent => "export",
            Self::ImportContent => "import",
        }
    }
}

/// Every capability, in the order the admin's role reference renders them.
pub const ALL_CAPABILITIES: &[Capability] = &[
    Capability::Read,
    Capability::EditPosts,
    Capability::EditOthersPosts,
    Capability::EditPublishedPosts,
    Capability::PublishPosts,
    Capability::DeletePosts,
    Capability::DeleteOthersPosts,
    Capability::ReadPrivatePosts,
    Capability::ManageCategories,
    Capability::ModerateComments,
    Capability::UploadFiles,
    Capability::ManageOptions,
    Capability::EditThemeOptions,
    Capability::ListUsers,
    Capability::EditUsers,
    Capability::ExportContent,
    Capability::ImportContent,
];

/// Every role, least privileged first.
pub const ALL_ROLES: &[Role] = &[
    Role::Subscriber,
    Role::Contributor,
    Role::Author,
    Role::Editor,
    Role::Administrator,
];

impl Role {
    /// Parse a stored role string.
    ///
    /// An unrecognised value degrades to [`Role::Subscriber`] rather than
    /// panicking or defaulting upward: a typo, a hand-edited row or an imported
    /// account must never end up with *more* access than its column names.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "administrator" | "admin" => Self::Administrator,
            "editor" => Self::Editor,
            "author" => Self::Author,
            "contributor" => Self::Contributor,
            _ => Self::Subscriber,
        }
    }

    /// The slug stored in `users.role`.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Administrator => "administrator",
            Self::Editor => "editor",
            Self::Author => "author",
            Self::Contributor => "contributor",
            Self::Subscriber => "subscriber",
        }
    }

    /// The human label shown in the admin.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Administrator => "Administrator",
            Self::Editor => "Editor",
            Self::Author => "Author",
            Self::Contributor => "Contributor",
            Self::Subscriber => "Subscriber",
        }
    }

    /// Whether this role holds `capability`.
    ///
    /// The matrix is WordPress core's, written out in full rather than as a
    /// privilege ladder: roles are *not* strictly nested in WordPress (an
    /// Author can publish but cannot moderate comments, while an Editor can do
    /// both), so `role >= Author` is the wrong shape for the question.
    #[must_use]
    pub const fn can(self, capability: Capability) -> bool {
        use Capability as C;
        match self {
            Self::Administrator => true,
            Self::Editor => matches!(
                capability,
                C::Read
                    | C::EditPosts
                    | C::EditOthersPosts
                    | C::EditPublishedPosts
                    | C::PublishPosts
                    | C::DeletePosts
                    | C::DeleteOthersPosts
                    | C::ReadPrivatePosts
                    | C::ManageCategories
                    | C::ModerateComments
                    | C::UploadFiles
            ),
            Self::Author => matches!(
                capability,
                C::Read
                    | C::EditPosts
                    | C::EditPublishedPosts
                    | C::PublishPosts
                    | C::DeletePosts
                    | C::UploadFiles
            ),
            // The defining constraint of a Contributor: they may write and
            // delete their own drafts, but hold neither `publish_posts` nor
            // `edit_published_posts` — so once an editor publishes their work,
            // they can no longer change it.
            Self::Contributor => matches!(capability, C::Read | C::EditPosts | C::DeletePosts),
            Self::Subscriber => matches!(capability, C::Read),
        }
    }

    /// Whether this role can reach the admin back-office at all.
    ///
    /// A Subscriber holds only `read`, so the admin would be an empty shell for
    /// them; they are sent to the front end instead. This mirrors WordPress
    /// hiding the dashboard from subscribers.
    #[must_use]
    pub const fn can_access_admin(self) -> bool {
        self.can(Capability::EditPosts)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Whether `role`, acting as user `actor_id`, may edit the post authored by
/// `author_id` in `status`.
///
/// This is the composite check WordPress spells across `edit_post`,
/// `edit_others_posts` and `edit_published_posts`, and it is the one place the
/// three combine — every edit path calls it rather than re-deriving the rule.
#[must_use]
pub fn can_edit_post(role: Role, actor_id: i64, author_id: i64, status: &str) -> bool {
    if !role.can(Capability::EditPosts) {
        return false;
    }
    if author_id != actor_id && !role.can(Capability::EditOthersPosts) {
        return false;
    }
    // "Published" here means anything the public can already reach, which
    // includes `private` (visible to logged-in readers) — a Contributor must
    // not be able to edit either after the fact.
    let is_live = matches!(status, "publish" | "private" | "future");
    if is_live && !role.can(Capability::EditPublishedPosts) {
        return false;
    }
    true
}

/// Whether `role`, acting as `actor_id`, may trash the post authored by
/// `author_id`.
#[must_use]
pub fn can_delete_post(role: Role, actor_id: i64, author_id: i64) -> bool {
    if !role.can(Capability::DeletePosts) {
        return false;
    }
    author_id == actor_id || role.can(Capability::DeleteOthersPosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_role_degrades_to_subscriber() {
        // The security-relevant direction: never upward.
        assert_eq!(Role::parse(""), Role::Subscriber);
        assert_eq!(Role::parse("superuser"), Role::Subscriber);
        assert_eq!(Role::parse("ADMINISTRATOR"), Role::Administrator);
    }

    #[test]
    fn contributor_cannot_publish_or_edit_published() {
        let role = Role::Contributor;
        assert!(role.can(Capability::EditPosts));
        assert!(!role.can(Capability::PublishPosts));
        assert!(!role.can(Capability::EditPublishedPosts));
        // Their own draft: editable. Their own published post: not.
        assert!(can_edit_post(role, 7, 7, "draft"));
        assert!(!can_edit_post(role, 7, 7, "publish"));
        // Somebody else's draft: not, either.
        assert!(!can_edit_post(role, 7, 8, "draft"));
    }

    #[test]
    fn author_owns_only_their_own_content() {
        let role = Role::Author;
        assert!(can_edit_post(role, 7, 7, "publish"));
        assert!(!can_edit_post(role, 7, 8, "draft"));
        assert!(can_delete_post(role, 7, 7));
        assert!(!can_delete_post(role, 7, 8));
        // An Author publishes but does not moderate.
        assert!(role.can(Capability::PublishPosts));
        assert!(!role.can(Capability::ModerateComments));
    }

    #[test]
    fn editor_has_all_content_caps_but_no_admin_caps() {
        let role = Role::Editor;
        assert!(can_edit_post(role, 7, 8, "publish"));
        assert!(role.can(Capability::ModerateComments));
        assert!(role.can(Capability::ManageCategories));
        // The line WordPress draws between Editor and Administrator.
        assert!(!role.can(Capability::ManageOptions));
        assert!(!role.can(Capability::EditUsers));
        assert!(!role.can(Capability::EditThemeOptions));
    }

    #[test]
    fn administrator_holds_every_capability() {
        for capability in ALL_CAPABILITIES {
            assert!(
                Role::Administrator.can(*capability),
                "administrator must hold {}",
                capability.slug()
            );
        }
    }

    #[test]
    fn subscriber_is_kept_out_of_the_admin() {
        assert!(!Role::Subscriber.can_access_admin());
        assert!(Role::Contributor.can_access_admin());
    }

    #[test]
    fn role_slugs_round_trip() {
        for role in ALL_ROLES {
            assert_eq!(Role::parse(role.slug()), *role);
        }
    }
}
