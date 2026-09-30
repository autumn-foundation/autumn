### Fixed

- **🧭 Wayfinder: redisplay a refused menu item or widget in
  `examples/cms`'s Appearance screen instead of the generic error page
  (refused-submission error paths with input kept 0/4 → 4/4):**
  `create_menu_item` (`POST /admin/appearance/menus/{id}/items`) and
  `create_widget` (`POST /admin/appearance/widgets`) returned the error for a
  blank label, a parent that no longer exists, an oversized widget
  title/body, or a full menu/sidebar, so the administrator lost the screen
  and everything typed into the card. Each now redisplays the Appearance
  screen at 422 with the card's values kept, the reason shown in the card
  (`role="alert"`, tied to the first field with `aria-describedby`) and that
  field focused. The menu that refused an item is always shown with the message, even
  when it is not on the current page of menus.
