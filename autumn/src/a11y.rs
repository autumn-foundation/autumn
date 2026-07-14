//! Typed accessible UI primitives — accessibility conformance by construction
//! (issue #1706).
//!
//! Autumn renders every view through Maud, which means an inaccessible page is
//! usually just a missing attribute away: an `<img>` with no `alt`, an
//! icon-only `<button>` with no accessible name, an `<input>` with no
//! associated `<label>`. Those omissions compile cleanly and only surface in an
//! audit (or, worse, for a user relying on assistive technology).
//!
//! The primitives in this module make the accessible name a **type-level
//! obligation** rather than a convention. Each one maps to a WCAG 2.1 success
//! criterion and is constructed so that the inaccessible form does not compile:
//!
//! - [`Img`] — WCAG 1.1.1 Non-text Content. The alt text is a mandatory
//!   positional argument of [`Img::new`]; there is no alt-less constructor.
//!   Decorative images opt in explicitly with [`Img::decorative`].
//! - [`Button`] — WCAG 4.1.2 Name, Role, Value. The accessible name is a
//!   required argument; an icon-only button routes it to `aria-label` via
//!   [`Button::icon`].
//! - [`Link`] — WCAG 2.4.4 Link Purpose (In Context) / 4.1.2 Name, Role, Value.
//!   The link text is a required argument of [`Link::new`]; an icon-only link
//!   routes its name to `aria-label` via [`Link::icon`]. There is no text-less
//!   constructor.
//! - [`MenuItem`] — WCAG 4.1.2 Name, Role, Value. A menu item carries an
//!   explicit `role="menuitem"` and a required accessible name from
//!   [`MenuItem::new`]; an icon-only item routes that name to `aria-label`.
//! - [`TextField`] — WCAG 1.3.1 Info and Relationships / 3.3.2 Labels or
//!   Instructions / 4.1.2 Name, Role, Value. A [`TextField<NoLabel>`] has no
//!   way to render; only after a label is attached (producing a
//!   [`TextField<Labeled>`]) does the type implement [`maud::Render`]. An
//!   unlabeled field is therefore unrepresentable as markup. The field can
//!   still carry presentational and validation attributes — `class`,
//!   `aria-invalid`/`aria-describedby` error wiring, and the HTML5 constraints
//!   `required`/`aria-required`/`minlength`/`maxlength`/`min`/`max`/`step` —
//!   without weakening that obligation: those setters are available in both
//!   states, but none of them supplies an accessible name, so a label is still
//!   required before the field can render.
//!
//! All three implement [`maud::Render`], so they splice directly into an
//! `html!` block:
//!
//! ```rust
//! use autumn_web::a11y::{Button, Img, TextField};
//! use autumn_web::html;
//!
//! let page = html! {
//!     (Img::new("/logo.svg", "Autumn logo"))
//!     (TextField::new("email").input_type("email").label("Email address"))
//!     (Button::new("Save").submit())
//! };
//! assert!(page.into_string().contains("alt=\"Autumn logo\""));
//! ```

use maud::{Markup, Render, html};

/// An informative or decorative image with a mandatory accessible name
/// (WCAG 1.1.1 Non-text Content).
///
/// Construct an informative image with [`Img::new`], which requires the `alt`
/// text as a positional argument — there is no way to build an `Img` without
/// one. Purely decorative images use [`Img::decorative`], which sets an
/// explicit empty `alt=""` so assistive technology skips them.
#[derive(Debug, Clone)]
pub struct Img {
    src: String,
    alt: String,
    class: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

impl Img {
    /// Build an informative image. The accessible name (`alt`) is required.
    ///
    /// ```rust
    /// use autumn_web::a11y::Img;
    /// use maud::Render;
    ///
    /// let markup = Img::new("/hero.png", "A field at sunrise").render();
    /// assert!(markup.into_string().contains("alt=\"A field at sunrise\""));
    /// ```
    pub fn new(src: impl Into<String>, alt: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            alt: alt.into(),
            class: None,
            width: None,
            height: None,
        }
    }

    /// Build a decorative image with an explicit empty `alt=""`.
    ///
    /// This documents intent: the image conveys no information, so assistive
    /// technology should ignore it. An empty `alt` is the WCAG-valid marker
    /// for a decorative image — distinct from a *missing* `alt`.
    pub fn decorative(src: impl Into<String>) -> Self {
        Self {
            src: src.into(),
            alt: String::new(),
            class: None,
            width: None,
            height: None,
        }
    }

    /// Set the `class` attribute.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }

    /// Set the `width` attribute (in pixels).
    #[must_use]
    pub const fn width(mut self, width: u32) -> Self {
        self.width = Some(width);
        self
    }

    /// Set the `height` attribute (in pixels).
    #[must_use]
    pub const fn height(mut self, height: u32) -> Self {
        self.height = Some(height);
        self
    }
}

impl Render for Img {
    fn render(&self) -> Markup {
        html! {
            img
                src=(self.src)
                alt=(self.alt)
                class=[self.class.as_deref()]
                width=[self.width]
                height=[self.height];
        }
    }
}

/// The `type` of a [`Button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonType {
    /// `type="button"` — an inert button driven by JavaScript/htmx.
    #[default]
    Button,
    /// `type="submit"` — submits the enclosing form.
    Submit,
    /// `type="reset"` — resets the enclosing form.
    Reset,
}

impl ButtonType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Submit => "submit",
            Self::Reset => "reset",
        }
    }
}

/// A button with a mandatory accessible name (WCAG 4.1.2 Name, Role, Value).
///
/// [`Button::new`] takes the visible text label as a required argument.
/// [`Button::icon`] builds an icon-only button whose accessible name becomes an
/// `aria-label`, so an icon button can never ship without a name. There is no
/// name-less constructor.
#[derive(Debug, Clone)]
pub struct Button {
    accessible_name: String,
    icon: Option<Markup>,
    kind: ButtonType,
    class: Option<String>,
}

impl Button {
    /// Build a button with a visible text label, which is also its accessible
    /// name. The label is required.
    ///
    /// ```rust
    /// use autumn_web::a11y::Button;
    /// use maud::Render;
    ///
    /// assert!(Button::new("Save").render().into_string().contains("Save"));
    /// ```
    pub fn new(accessible_name: impl Into<String>) -> Self {
        Self {
            accessible_name: accessible_name.into(),
            icon: None,
            kind: ButtonType::default(),
            class: None,
        }
    }

    /// Build an icon-only button. The `accessible_name` becomes the button's
    /// `aria-label`, since the icon carries no text for assistive technology.
    ///
    /// ```rust
    /// use autumn_web::a11y::Button;
    /// use autumn_web::html;
    /// use maud::Render;
    ///
    /// let trash = html! { span aria-hidden="true" { "🗑" } };
    /// let markup = Button::icon(trash, "Delete item").render().into_string();
    /// assert!(markup.contains("aria-label=\"Delete item\""));
    /// ```
    pub fn icon(icon: Markup, accessible_name: impl Into<String>) -> Self {
        Self {
            accessible_name: accessible_name.into(),
            icon: Some(icon),
            kind: ButtonType::default(),
            class: None,
        }
    }

    /// Set the button `type` explicitly.
    #[must_use]
    pub const fn kind(mut self, kind: ButtonType) -> Self {
        self.kind = kind;
        self
    }

    /// Shorthand for `type="submit"`.
    #[must_use]
    pub const fn submit(mut self) -> Self {
        self.kind = ButtonType::Submit;
        self
    }

    /// Shorthand for `type="button"` (the default).
    #[must_use]
    pub const fn button(mut self) -> Self {
        self.kind = ButtonType::Button;
        self
    }

    /// Set the `class` attribute.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

impl Render for Button {
    fn render(&self) -> Markup {
        self.icon.as_ref().map_or_else(
            || {
                html! {
                    button type=(self.kind.as_str()) class=[self.class.as_deref()] {
                        (self.accessible_name)
                    }
                }
            },
            |icon| {
                html! {
                    button
                        type=(self.kind.as_str())
                        aria-label=(self.accessible_name)
                        class=[self.class.as_deref()] {
                        (icon)
                    }
                }
            },
        )
    }
}

/// A hyperlink with a mandatory accessible name (WCAG 2.4.4 Link Purpose (In
/// Context) / 4.1.2 Name, Role, Value).
///
/// [`Link::new`] takes the visible link text as a required positional argument,
/// so a text-less link cannot be built. [`Link::icon`] builds an icon-only link
/// whose accessible name becomes an `aria-label`. There is no name-less
/// constructor.
#[derive(Debug, Clone)]
pub struct Link {
    href: String,
    accessible_name: String,
    icon: Option<Markup>,
    class: Option<String>,
    new_tab: bool,
}

impl Link {
    /// Build a link with visible text, which is also its accessible name. Both
    /// the `href` and the `text` are required.
    ///
    /// ```rust
    /// use autumn_web::a11y::Link;
    /// use maud::Render;
    ///
    /// let markup = Link::new("/about", "About us").render().into_string();
    /// assert!(markup.contains("href=\"/about\""));
    /// assert!(markup.contains(">About us<"));
    /// ```
    pub fn new(href: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            href: href.into(),
            accessible_name: text.into(),
            icon: None,
            class: None,
            new_tab: false,
        }
    }

    /// Build an icon-only link. The `accessible_name` becomes the link's
    /// `aria-label`, since the icon carries no text for assistive technology.
    ///
    /// ```rust
    /// use autumn_web::a11y::Link;
    /// use autumn_web::html;
    /// use maud::Render;
    ///
    /// let glyph = html! { span aria-hidden="true" { "gh" } };
    /// let markup = Link::icon("https://example.com", glyph, "GitHub")
    ///     .render()
    ///     .into_string();
    /// assert!(markup.contains("aria-label=\"GitHub\""));
    /// ```
    pub fn icon(href: impl Into<String>, icon: Markup, accessible_name: impl Into<String>) -> Self {
        Self {
            href: href.into(),
            accessible_name: accessible_name.into(),
            icon: Some(icon),
            class: None,
            new_tab: false,
        }
    }

    /// Open the link in a new browsing context (`target="_blank"`), adding
    /// `rel="noopener noreferrer"` so the opened page cannot reach back through
    /// `window.opener`.
    #[must_use]
    pub const fn new_tab(mut self) -> Self {
        self.new_tab = true;
        self
    }

    /// Set the `class` attribute.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

impl Render for Link {
    fn render(&self) -> Markup {
        let target = self.new_tab.then_some("_blank");
        let rel = self.new_tab.then_some("noopener noreferrer");
        self.icon.as_ref().map_or_else(
            || {
                html! {
                    a href=(self.href) target=[target] rel=[rel] class=[self.class.as_deref()] {
                        (self.accessible_name)
                    }
                }
            },
            |icon| {
                html! {
                    a
                        href=(self.href)
                        aria-label=(self.accessible_name)
                        target=[target]
                        rel=[rel]
                        class=[self.class.as_deref()] {
                        (icon)
                    }
                }
            },
        )
    }
}

/// An interactive menu item with a mandatory accessible name and an explicit
/// `role="menuitem"` (WCAG 4.1.2 Name, Role, Value).
///
/// [`MenuItem::new`] takes the visible label as a required argument. By default
/// the item renders as a `<button type="button" role="menuitem">`; attaching an
/// [`href`](MenuItem::href) renders it as an `<a role="menuitem">` instead.
/// Adding an [`icon`](MenuItem::icon) routes the accessible name to
/// `aria-label`, so an icon-only item can never ship without a name. There is
/// no name-less constructor.
#[derive(Debug, Clone)]
pub struct MenuItem {
    accessible_name: String,
    href: Option<String>,
    icon: Option<Markup>,
    class: Option<String>,
}

impl MenuItem {
    /// Build a menu item with a visible label, which is also its accessible
    /// name. The label is required.
    ///
    /// ```rust
    /// use autumn_web::a11y::MenuItem;
    /// use maud::Render;
    ///
    /// let markup = MenuItem::new("Settings").render().into_string();
    /// assert!(markup.contains("role=\"menuitem\""));
    /// assert!(markup.contains(">Settings<"));
    /// ```
    pub fn new(accessible_name: impl Into<String>) -> Self {
        Self {
            accessible_name: accessible_name.into(),
            href: None,
            icon: None,
            class: None,
        }
    }

    /// Render the item as a link (`<a role="menuitem" href=…>`) rather than the
    /// default button.
    #[must_use]
    pub fn href(mut self, href: impl Into<String>) -> Self {
        self.href = Some(href.into());
        self
    }

    /// Add a leading icon. The visible label from [`MenuItem::new`] moves to
    /// `aria-label`, keeping the accessible name intact for icon-only items.
    #[must_use]
    pub fn icon(mut self, icon: Markup) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Set the `class` attribute.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

impl Render for MenuItem {
    fn render(&self) -> Markup {
        // An icon-only item routes its accessible name to `aria-label`; a text
        // item renders the name as visible content.
        let aria_label = self.icon.is_some().then_some(self.accessible_name.as_str());
        self.href.as_deref().map_or_else(
            || {
                html! {
                    button
                        type="button"
                        role="menuitem"
                        aria-label=[aria_label]
                        class=[self.class.as_deref()] {
                        @match &self.icon {
                            Some(icon) => (icon),
                            None => (self.accessible_name),
                        }
                    }
                }
            },
            |href| {
                html! {
                    a href=(href) role="menuitem" aria-label=[aria_label] class=[self.class.as_deref()] {
                        @match &self.icon {
                            Some(icon) => (icon),
                            None => (self.accessible_name),
                        }
                    }
                }
            },
        )
    }
}

/// Typestate marker: a [`TextField`] with no label attached yet. This state
/// does **not** implement [`maud::Render`], so it cannot be turned into markup.
#[derive(Debug, Clone, Copy)]
pub struct NoLabel;

/// Typestate marker: a [`TextField`] whose label has been attached. Only this
/// state implements [`maud::Render`].
#[derive(Debug, Clone, Copy)]
pub struct Labeled;

/// How a [`TextField`]'s accessible name is provided.
#[derive(Debug, Clone)]
enum LabelSource {
    /// A visible `<label for=…>` element.
    Visible(String),
    /// An `aria-label` attribute (no visible label).
    Aria(String),
    /// An `aria-labelledby` reference to an existing element's `id`.
    LabelledBy(String),
}

/// A text input whose label association is enforced by the type system
/// (WCAG 1.3.1 Info and Relationships, 3.3.2 Labels or Instructions,
/// 4.1.2 Name, Role, Value).
///
/// [`TextField::new`] returns a `TextField<NoLabel>`, which has no way to
/// render. Attaching a label with [`label`](TextField::label),
/// [`aria_label`](TextField::aria_label), or
/// [`labelled_by`](TextField::labelled_by) consumes it and returns a
/// `TextField<Labeled>`, the only state that implements [`maud::Render`]. An
/// unlabeled field therefore cannot be turned into markup — the accessible
/// name obligation is discharged at compile time.
///
/// The value/type/required attributes — along with the presentational
/// [`class`](TextField::class) / [`label_class`](TextField::label_class), the error-wiring
/// [`aria_invalid`](TextField::aria_invalid) /
/// [`described_by`](TextField::described_by), and the HTML5 validation
/// constraints [`aria_required`](TextField::aria_required) /
/// [`minlength`](TextField::minlength) / [`maxlength`](TextField::maxlength) /
/// [`min`](TextField::min) / [`max`](TextField::max) / [`step`](TextField::step)
/// — are chainable in either state. None of them provides an accessible name, so
/// none of them lifts the compile-time label obligation: they can be set on a
/// `TextField<NoLabel>`, but the field still cannot be rendered until a label is
/// attached.
#[derive(Debug, Clone)]
pub struct TextField<State> {
    name: String,
    input_type: String,
    value: Option<String>,
    required: bool,
    aria_required: bool,
    class: Option<String>,
    label_class: Option<String>,
    aria_invalid: Option<bool>,
    described_by: Option<String>,
    minlength: Option<u32>,
    maxlength: Option<u32>,
    min: Option<String>,
    max: Option<String>,
    step: Option<String>,
    label: Option<LabelSource>,
    _state: std::marker::PhantomData<State>,
}

impl TextField<NoLabel> {
    /// Start building a text field with the given form `name`. The returned
    /// value has no label yet and cannot be rendered until one is attached.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            input_type: "text".to_owned(),
            value: None,
            required: false,
            aria_required: false,
            class: None,
            label_class: None,
            aria_invalid: None,
            described_by: None,
            minlength: None,
            maxlength: None,
            min: None,
            max: None,
            step: None,
            label: None,
            _state: std::marker::PhantomData,
        }
    }

    /// Attach a visible `<label for=…>` and transition to the renderable
    /// [`Labeled`] state.
    #[must_use]
    pub fn label(self, text: impl Into<String>) -> TextField<Labeled> {
        self.with_label(LabelSource::Visible(text.into()))
    }

    /// Attach an `aria-label` (no visible label) and transition to the
    /// renderable [`Labeled`] state.
    #[must_use]
    pub fn aria_label(self, text: impl Into<String>) -> TextField<Labeled> {
        self.with_label(LabelSource::Aria(text.into()))
    }

    /// Reference an existing element's `id` via `aria-labelledby` and
    /// transition to the renderable [`Labeled`] state.
    #[must_use]
    pub fn labelled_by(self, id: impl Into<String>) -> TextField<Labeled> {
        self.with_label(LabelSource::LabelledBy(id.into()))
    }

    fn with_label(self, label: LabelSource) -> TextField<Labeled> {
        TextField {
            name: self.name,
            input_type: self.input_type,
            value: self.value,
            required: self.required,
            aria_required: self.aria_required,
            class: self.class,
            label_class: self.label_class,
            aria_invalid: self.aria_invalid,
            described_by: self.described_by,
            minlength: self.minlength,
            maxlength: self.maxlength,
            min: self.min,
            max: self.max,
            step: self.step,
            label: Some(label),
            _state: std::marker::PhantomData,
        }
    }
}

impl<State> TextField<State> {
    /// Set the input `type` (e.g. `"email"`, `"password"`). Available before or
    /// after a label is attached.
    #[must_use]
    pub fn input_type(mut self, input_type: impl Into<String>) -> Self {
        self.input_type = input_type.into();
        self
    }

    /// Set the input's initial `value`.
    #[must_use]
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// Mark the input as `required`.
    #[must_use]
    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Add a mirroring `aria-required="true"` alongside the native `required`
    /// attribute, matching the ARIA wiring the scaffold generator emits for
    /// non-nullable fields. Available before or after a label is attached.
    ///
    /// ```rust
    /// use autumn_web::a11y::TextField;
    /// use maud::Render;
    ///
    /// let markup = TextField::new("name")
    ///     .required()
    ///     .aria_required()
    ///     .label("Name")
    ///     .render()
    ///     .into_string();
    /// assert!(markup.contains("aria-required=\"true\""));
    /// ```
    #[must_use]
    pub const fn aria_required(mut self) -> Self {
        self.aria_required = true;
        self
    }

    /// Set the `class` attribute on the `<input>`. Available before or after a
    /// label is attached.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }

    /// Sets the `class` attribute on the visible `<label>` element.
    ///
    /// Only affects rendering when a visible label was set via [`label`](Self::label);
    /// `aria_label`/`labelled_by` variants render no `<label>` element.
    #[must_use]
    pub fn label_class(mut self, class: impl Into<String>) -> Self {
        self.label_class = Some(class.into());
        self
    }

    /// Set `aria-invalid` to `"true"` or `"false"`, wiring the field to its
    /// validation state. When left unset the attribute is omitted entirely.
    ///
    /// ```rust
    /// use autumn_web::a11y::TextField;
    /// use maud::Render;
    ///
    /// let markup = TextField::new("email")
    ///     .aria_invalid(true)
    ///     .label("Email address")
    ///     .render()
    ///     .into_string();
    /// assert!(markup.contains("aria-invalid=\"true\""));
    /// ```
    #[must_use]
    pub const fn aria_invalid(mut self, invalid: bool) -> Self {
        self.aria_invalid = Some(invalid);
        self
    }

    /// Reference the `id` of the element describing this field (typically an
    /// inline error container) via `aria-describedby`, so assistive technology
    /// announces the error alongside the input.
    ///
    /// ```rust
    /// use autumn_web::a11y::TextField;
    /// use maud::Render;
    ///
    /// let markup = TextField::new("email")
    ///     .described_by("email-error")
    ///     .aria_invalid(true)
    ///     .label("Email address")
    ///     .render()
    ///     .into_string();
    /// assert!(markup.contains("aria-describedby=\"email-error\""));
    /// ```
    #[must_use]
    pub fn described_by(mut self, id: impl Into<String>) -> Self {
        self.described_by = Some(id.into());
        self
    }

    /// Set the HTML5 `minlength` validation constraint (minimum character
    /// count), matching the scaffold generator's `text{min=…}` DSL modifier.
    #[must_use]
    pub const fn minlength(mut self, minlength: u32) -> Self {
        self.minlength = Some(minlength);
        self
    }

    /// Set the HTML5 `maxlength` validation constraint (maximum character
    /// count), matching the scaffold generator's `text{max=…}` DSL modifier.
    #[must_use]
    pub const fn maxlength(mut self, maxlength: u32) -> Self {
        self.maxlength = Some(maxlength);
        self
    }

    /// Set the HTML5 `min` validation constraint for numeric inputs. The value
    /// is passed through verbatim (e.g. an integer or decimal bound), matching
    /// the scaffold generator's numeric `{min=…}` DSL modifier.
    #[must_use]
    pub fn min(mut self, min: impl Into<String>) -> Self {
        self.min = Some(min.into());
        self
    }

    /// Set the HTML5 `max` validation constraint for numeric inputs, matching
    /// the scaffold generator's numeric `{max=…}` DSL modifier.
    #[must_use]
    pub fn max(mut self, max: impl Into<String>) -> Self {
        self.max = Some(max.into());
        self
    }

    /// Set the HTML5 `step` attribute for numeric inputs (e.g. `"any"` for a
    /// constrained float), matching the scaffold generator's `step="any"` on
    /// `f32`/`f64` fields.
    #[must_use]
    pub fn step(mut self, step: impl Into<String>) -> Self {
        self.step = Some(step.into());
        self
    }
}

impl Render for TextField<Labeled> {
    fn render(&self) -> Markup {
        // `label` is always `Some` in the `Labeled` state — it is set on every
        // transition out of `NoLabel` — but match defensively rather than
        // unwrap. The label source only changes the label wiring (a preceding
        // `<label for=…>` vs an `aria-label`/`aria-labelledby` on the input);
        // every other attribute is shared, so compute the label bits once and
        // emit a single `<input>`.
        let (visible_label, aria_label, aria_labelledby) = match &self.label {
            Some(LabelSource::Visible(text)) => (Some(text.as_str()), None, None),
            Some(LabelSource::Aria(text)) => (None, Some(text.as_str()), None),
            Some(LabelSource::LabelledBy(id)) => (None, None, Some(id.as_str())),
            None => (None, None, None),
        };
        let aria_invalid = self
            .aria_invalid
            .map(|invalid| if invalid { "true" } else { "false" });
        let aria_required = self.aria_required.then_some("true");
        html! {
            @if let Some(text) = visible_label {
                label for=(self.name) class=[self.label_class.as_deref()] { (text) }
            }
            input
                type=(self.input_type)
                id=(self.name)
                name=(self.name)
                aria-label=[aria_label]
                aria-labelledby=[aria_labelledby]
                class=[self.class.as_deref()]
                value=[self.value.as_deref()]
                minlength=[self.minlength]
                maxlength=[self.maxlength]
                min=[self.min.as_deref()]
                max=[self.max.as_deref()]
                step=[self.step.as_deref()]
                aria-invalid=[aria_invalid]
                aria-describedby=[self.described_by.as_deref()]
                aria-required=[aria_required]
                required[self.required];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn img_new_requires_and_renders_alt() {
        let markup = Img::new("/logo.png", "Company logo").render().into_string();
        assert!(markup.contains("src=\"/logo.png\""));
        assert!(markup.contains("alt=\"Company logo\""));
    }

    #[test]
    fn img_decorative_sets_empty_alt() {
        let markup = Img::decorative("/divider.png").render().into_string();
        assert!(markup.contains("alt=\"\""));
    }

    #[test]
    fn img_escapes_alt_text() {
        let markup = Img::new("/x.png", "a \"quoted\" & <tagged> name")
            .render()
            .into_string();
        assert!(!markup.contains("<tagged>"));
        assert!(markup.contains("&lt;tagged&gt;"));
    }

    #[test]
    fn button_new_renders_visible_label() {
        let markup = Button::new("Save").submit().render().into_string();
        assert!(markup.contains("type=\"submit\""));
        assert!(markup.contains(">Save<"));
        assert!(!markup.contains("aria-label"));
    }

    #[test]
    fn button_icon_uses_aria_label() {
        let icon = html! { span aria-hidden="true" { "x" } };
        let markup = Button::icon(icon, "Close dialog").render().into_string();
        assert!(markup.contains("aria-label=\"Close dialog\""));
    }

    #[test]
    fn text_field_visible_label_is_associated() {
        let markup = TextField::new("email")
            .input_type("email")
            .label("Email address")
            .render()
            .into_string();
        assert!(markup.contains("<label for=\"email\">Email address</label>"));
        assert!(markup.contains("id=\"email\""));
        assert!(markup.contains("name=\"email\""));
        assert!(markup.contains("type=\"email\""));
    }

    #[test]
    fn text_field_aria_label_variant() {
        let markup = TextField::new("q")
            .aria_label("Search")
            .render()
            .into_string();
        assert!(markup.contains("aria-label=\"Search\""));
    }

    #[test]
    fn text_field_labelled_by_variant() {
        let markup = TextField::new("q")
            .labelled_by("search-heading")
            .render()
            .into_string();
        assert!(markup.contains("aria-labelledby=\"search-heading\""));
    }

    #[test]
    fn text_field_required_and_value() {
        let markup = TextField::new("name")
            .value("Ada")
            .required()
            .label("Name")
            .render()
            .into_string();
        assert!(markup.contains("value=\"Ada\""));
        assert!(markup.contains("required"));
    }

    #[test]
    fn text_field_carries_class_and_error_wiring() {
        let markup = TextField::new("email")
            .input_type("email")
            .class("autumn-field__input autumn-field__input--invalid")
            .aria_invalid(true)
            .described_by("email-error")
            .label("Email address")
            .render()
            .into_string();
        assert!(
            markup.contains("class=\"autumn-field__input autumn-field__input--invalid\""),
            "{markup}"
        );
        assert!(markup.contains("aria-invalid=\"true\""), "{markup}");
        assert!(
            markup.contains("aria-describedby=\"email-error\""),
            "{markup}"
        );
        // The label obligation is still discharged via the visible label.
        assert!(
            markup.contains("<label for=\"email\">Email address</label>"),
            "{markup}"
        );
    }

    #[test]
    fn text_field_aria_invalid_false_renders_explicitly() {
        let markup = TextField::new("email")
            .aria_invalid(false)
            .label("Email address")
            .render()
            .into_string();
        assert!(markup.contains("aria-invalid=\"false\""), "{markup}");
    }

    #[test]
    fn text_field_aria_invalid_omitted_when_unset() {
        let markup = TextField::new("email")
            .label("Email address")
            .render()
            .into_string();
        assert!(!markup.contains("aria-invalid"), "{markup}");
        assert!(!markup.contains("aria-describedby"), "{markup}");
    }

    #[test]
    fn text_field_string_length_constraints() {
        let markup = TextField::new("title")
            .minlength(3)
            .maxlength(120)
            .required()
            .aria_required()
            .label("Title")
            .render()
            .into_string();
        assert!(markup.contains("minlength=\"3\""), "{markup}");
        assert!(markup.contains("maxlength=\"120\""), "{markup}");
        assert!(markup.contains("required"), "{markup}");
        assert!(markup.contains("aria-required=\"true\""), "{markup}");
    }

    #[test]
    fn text_field_numeric_constraints() {
        let markup = TextField::new("ratio")
            .input_type("number")
            .min("0")
            .max("1")
            .step("any")
            .label("Ratio")
            .render()
            .into_string();
        assert!(markup.contains("type=\"number\""), "{markup}");
        assert!(markup.contains("min=\"0\""), "{markup}");
        assert!(markup.contains("max=\"1\""), "{markup}");
        assert!(markup.contains("step=\"any\""), "{markup}");
    }

    #[test]
    fn text_field_label_class_is_emitted() {
        let html = TextField::new("email")
            .label("Email address")
            .label_class("autumn-field__label")
            .render()
            .into_string();
        assert!(
            html.contains(
                r#"<label for="email" class="autumn-field__label">Email address</label>"#
            ),
            "got: {html}"
        );
    }

    #[test]
    fn text_field_label_class_omitted_when_unset() {
        let html = TextField::new("email")
            .label("Email address")
            .render()
            .into_string();
        assert!(
            html.contains(r#"<label for="email">Email address</label>"#),
            "got: {html}"
        );
        assert!(
            !html.contains("class="),
            "label should have no class when unset, got: {html}"
        );
    }

    #[test]
    fn text_field_new_attrs_survive_aria_label_variant() {
        // The new attributes are set on the `NoLabel` builder before the label
        // transition; they must survive `with_label` into the `Labeled` state.
        let markup = TextField::new("q")
            .class("search")
            .aria_invalid(true)
            .aria_label("Search")
            .render()
            .into_string();
        assert!(markup.contains("aria-label=\"Search\""), "{markup}");
        assert!(markup.contains("class=\"search\""), "{markup}");
        assert!(markup.contains("aria-invalid=\"true\""), "{markup}");
    }
}
