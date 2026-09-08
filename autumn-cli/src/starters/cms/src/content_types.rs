//! The post-type and taxonomy registry.
//!
//! WordPress's `register_post_type` / `register_taxonomy` are the extension
//! point that turns a blog engine into a CMS: a plugin declares a new content
//! shape and the admin UI, permalinks, REST routes and archive pages all pick
//! it up without further work. This module is the same idea with the
//! declaration checked at compile time rather than assembled from an untyped
//! options array.
//!
//! Both registries are process-wide and are populated once, during startup, by
//! [`register_post_type`] / [`register_taxonomy`]. Registration after the first
//! read is refused rather than silently ignored — a type that appears in the
//! admin menu but not in the router is the worst of both outcomes.

use std::sync::{OnceLock, RwLock};

/// A registered content type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostType {
    /// Stored in `posts.post_type`.
    pub slug: &'static str,
    /// "Post", "Page", "Product" — used in admin headings and buttons.
    pub singular: &'static str,
    /// "Posts", "Pages", "Products".
    pub plural: &'static str,
    /// Hierarchical types (pages) nest under a parent and are ordered by
    /// `menu_order`; flat types (posts) are ordered by date.
    pub hierarchical: bool,
    /// Whether the type is reachable on the front end at all. A private type is
    /// editable in the admin but has no permalink and no archive.
    pub public: bool,
    /// Whether this type has a date-ordered archive listing of its own.
    pub has_archive: bool,
    /// Whether the editor offers a comments toggle.
    pub supports_comments: bool,
    /// Whether the editor offers a hand-written excerpt field.
    pub supports_excerpt: bool,
    /// Whether the editor offers a featured image.
    pub supports_thumbnail: bool,
    /// Whether edits are snapshotted into `revisions`.
    pub supports_revisions: bool,
    /// The URL segment an archive lives under, when `has_archive` is set.
    pub archive_base: &'static str,
}

impl PostType {
    /// A sensible flat, public, fully-featured type — the base most custom
    /// types want to start from.
    #[must_use]
    pub const fn new(slug: &'static str, singular: &'static str, plural: &'static str) -> Self {
        Self {
            slug,
            singular,
            plural,
            hierarchical: false,
            public: true,
            has_archive: true,
            supports_comments: true,
            supports_excerpt: true,
            supports_thumbnail: true,
            supports_revisions: true,
            archive_base: slug,
        }
    }
}

/// WordPress's `post`: flat, dated, commentable — the blog.
pub const POST: PostType = PostType::new("post", "Post", "Posts");

/// WordPress's `page`: hierarchical, undated, ordered by hand, and archived
/// nowhere (a page is reached by its own path, never by a listing).
pub const PAGE: PostType = PostType {
    slug: "page",
    singular: "Page",
    plural: "Pages",
    hierarchical: true,
    public: true,
    has_archive: false,
    supports_comments: false,
    supports_excerpt: false,
    supports_thumbnail: true,
    supports_revisions: true,
    archive_base: "page",
};

/// A registered taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taxonomy {
    /// Stored in `terms.taxonomy`.
    pub slug: &'static str,
    pub singular: &'static str,
    pub plural: &'static str,
    /// Hierarchical taxonomies (categories) allow a parent term; flat ones
    /// (tags) do not, and their editor is a comma-separated field.
    pub hierarchical: bool,
    /// The post types this taxonomy may be applied to.
    pub post_types: &'static [&'static str],
    /// The URL segment term archives live under: `/category/rust`.
    pub rewrite_base: &'static str,
}

/// WordPress's `category`.
pub const CATEGORY: Taxonomy = Taxonomy {
    slug: "category",
    singular: "Category",
    plural: "Categories",
    hierarchical: true,
    post_types: &["post"],
    rewrite_base: "category",
};

/// WordPress's `post_tag`.
pub const POST_TAG: Taxonomy = Taxonomy {
    slug: "post_tag",
    singular: "Tag",
    plural: "Tags",
    hierarchical: false,
    post_types: &["post"],
    rewrite_base: "tag",
};

static POST_TYPES: OnceLock<RwLock<Vec<PostType>>> = OnceLock::new();
static TAXONOMIES: OnceLock<RwLock<Vec<Taxonomy>>> = OnceLock::new();

fn post_types() -> &'static RwLock<Vec<PostType>> {
    POST_TYPES.get_or_init(|| RwLock::new(vec![POST, PAGE]))
}

fn taxonomies() -> &'static RwLock<Vec<Taxonomy>> {
    TAXONOMIES.get_or_init(|| RwLock::new(vec![CATEGORY, POST_TAG]))
}

/// Why a registration was refused.
///
/// A registration that silently produces unreachable content is the worst
/// outcome available: the plugin looks installed, the admin screens work, the
/// items save — and every one of their URLs resolves to something else. Naming
/// the collision at startup is the whole point.
#[derive(Debug, PartialEq, Eq)]
pub enum RegistrationError {
    /// The segment is claimed by one of the application's own literal routes.
    ReservedPath { field: &'static str, value: String },
    /// The segment is claimed by a registered taxonomy's term archives.
    TaxonomyBase { field: &'static str, value: String },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReservedPath { field, value } => write!(
                f,
                "`{field}` cannot be `{value}`: the application serves that path itself, \
                 so the content registered under it would never be reachable"
            ),
            Self::TaxonomyBase { field, value } => write!(
                f,
                "`{field}` cannot be `{value}`: a registered taxonomy publishes its term \
                 archives under that segment, and the resolver reaches those first"
            ),
        }
    }
}

impl std::error::Error for RegistrationError {}

/// Whether `segment` is already claimed by something the resolver reaches
/// before custom post types.
fn claim_on(field: &'static str, segment: &str) -> Result<(), RegistrationError> {
    if crate::content::is_reserved_path(segment) {
        return Err(RegistrationError::ReservedPath {
            field,
            value: segment.to_owned(),
        });
    }
    if all_taxonomies()
        .iter()
        .any(|taxonomy| taxonomy.rewrite_base == segment)
    {
        return Err(RegistrationError::TaxonomyBase {
            field,
            value: segment.to_owned(),
        });
    }
    Ok(())
}

/// Register a custom post type. Call during startup, before the router is
/// built.
///
/// Re-registering an existing slug replaces it, which is what makes a plugin
/// able to adjust a built-in type (turning comments off on `page`, say) rather
/// than only add to the set.
///
/// # Errors
///
/// Refuses a slug or `archive_base` that the resolver reaches before custom
/// types: an application route (`/search`, `/feed`, `/admin`, …) or a
/// registered taxonomy's rewrite base (`/category`, `/tag`). `permalinks::resolve`
/// tries taxonomy bases and literal routes ahead of custom types, so a type
/// registered on one of those segments mints item URLs — `/category/widget` —
/// that always resolve to something else, and its content is unreachable for
/// good. Only a *new* registration is checked; replacing a built-in keeps its
/// existing segments by definition.
pub fn register_post_type(post_type: PostType) -> Result<(), RegistrationError> {
    let mut types = post_types().write().expect("post type registry poisoned");
    if let Some(existing) = types.iter_mut().find(|t| t.slug == post_type.slug) {
        *existing = post_type;
        return Ok(());
    }
    drop(types);

    claim_on("slug", post_type.slug)?;
    if post_type.has_archive {
        claim_on("archive_base", post_type.archive_base)?;
    }

    post_types()
        .write()
        .expect("post type registry poisoned")
        .push(post_type);
    Ok(())
}

/// Register a custom taxonomy. See [`register_post_type`].
///
/// # Errors
///
/// Refuses a `rewrite_base` an application route already claims, for the same
/// reason.
pub fn register_taxonomy(taxonomy: Taxonomy) -> Result<(), RegistrationError> {
    let mut taxes = taxonomies().write().expect("taxonomy registry poisoned");
    if let Some(existing) = taxes.iter_mut().find(|t| t.slug == taxonomy.slug) {
        *existing = taxonomy;
        return Ok(());
    }
    drop(taxes);

    if crate::content::is_reserved_path(taxonomy.rewrite_base) {
        return Err(RegistrationError::ReservedPath {
            field: "rewrite_base",
            value: taxonomy.rewrite_base.to_owned(),
        });
    }

    taxonomies()
        .write()
        .expect("taxonomy registry poisoned")
        .push(taxonomy);
    Ok(())
}

/// Every registered post type, in registration order.
#[must_use]
pub fn all_post_types() -> Vec<PostType> {
    post_types()
        .read()
        .expect("post type registry poisoned")
        .clone()
}

/// Every registered taxonomy, in registration order.
#[must_use]
pub fn all_taxonomies() -> Vec<Taxonomy> {
    taxonomies()
        .read()
        .expect("taxonomy registry poisoned")
        .clone()
}

/// Look up a post type by its slug.
#[must_use]
pub fn find_post_type(slug: &str) -> Option<PostType> {
    post_types()
        .read()
        .expect("post type registry poisoned")
        .iter()
        .find(|t| t.slug == slug)
        .cloned()
}

/// Look up a taxonomy by its slug.
#[must_use]
pub fn find_taxonomy(slug: &str) -> Option<Taxonomy> {
    taxonomies()
        .read()
        .expect("taxonomy registry poisoned")
        .iter()
        .find(|t| t.slug == slug)
        .cloned()
}

/// The taxonomies that apply to `post_type`.
#[must_use]
pub fn taxonomies_for(post_type: &str) -> Vec<Taxonomy> {
    all_taxonomies()
        .into_iter()
        .filter(|t| t.post_types.contains(&post_type))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_types_are_registered() {
        assert!(find_post_type("post").is_some());
        assert!(find_post_type("page").is_some());
        assert!(find_post_type("nope").is_none());
    }

    #[test]
    fn page_is_hierarchical_and_unarchived() {
        let page = find_post_type("page").unwrap();
        assert!(page.hierarchical);
        assert!(!page.has_archive);
        // A page has no comment thread by default, matching WordPress.
        assert!(!page.supports_comments);
    }

    #[test]
    fn taxonomies_are_scoped_to_their_post_types() {
        let for_post = taxonomies_for("post");
        assert!(for_post.iter().any(|t| t.slug == "category"));
        assert!(for_post.iter().any(|t| t.slug == "post_tag"));
        // Pages carry no taxonomy in core.
        assert!(taxonomies_for("page").is_empty());
    }

    #[test]
    fn category_is_hierarchical_and_tags_are_not() {
        assert!(find_taxonomy("category").unwrap().hierarchical);
        assert!(!find_taxonomy("post_tag").unwrap().hierarchical);
    }
}
