### Breaking Changes

- **Widget class names are now `autumn-*`-namespaced (#2354):** several widgets
  emitted unprefixed class hooks that the widget stylesheet
  (`/static/css/autumn-widgets.css`) never styled — `card`, `card-header`,
  `card-title`, `card-body`, `card-footer` (`card()`), `stat-card`,
  `stat-label`, `stat-value`, `stat-link` (`stat_card()`), `search-empty`
  (`active_search_empty_state()`), `autocomplete-empty`
  (`autocomplete_empty_state()`), `alert__icon-svg` (alert icons), and the
  bare `active` hook on `nav_link()`. Those classes are now
  `autumn-card`, `autumn-card__header`, `autumn-card__title`,
  `autumn-card__body`, `autumn-card__footer`, `autumn-stat-card`,
  `autumn-stat-card__label`, `autumn-stat-card__value`,
  `autumn-stat-card__link`, `autumn-search-empty`,
  `autumn-autocomplete-empty`, `autumn-alert__icon-svg`, and
  `autumn-active` — and every one of them is backed by a rule in the widget
  stylesheet, so the widgets render styled out of the box with no app CSS.
  If your app or custom stylesheet targeted the old unprefixed hooks, update
  the selectors to the new namespaced ones ([migration
  guide](docs/migrations/next.md#widgets-widget-css-classes-are-now-autumn--prefixed-2354)).

### Fixed

- **`/_stories` widget previews render unstyled (#2354):** the built-in
  `card` and `stat-card` stories link only the widget stylesheet, which had
  no rules for the unprefixed classes the widgets emitted. Both the class
  names and the stylesheet now agree, so the gallery previews are styled.
