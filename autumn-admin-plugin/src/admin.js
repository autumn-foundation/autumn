// autumn-admin-plugin client-side helpers.
// Served from /{prefix}/static/admin.js so it works under CSP `script-src 'self'`.

(function () {
  // Select-all checkbox toggles every .row-check in the current table.
  document.addEventListener("click", function (e) {
    var t = e.target;
    if (t && t.id === "select-all") {
      document.querySelectorAll(".row-check").forEach(function (c) {
        c.checked = t.checked;
      });
    }
  });

  // Bulk-action submit: require at least one selected row, and — if the
  // selected action is marked as requiring confirmation (data-confirm="1"
  // on the option) — show the server-rendered #admin-bulk-confirm <dialog>
  // (autumn_web::widgets::modal) rather than a native browser confirm popup.
  // Once the dialog's Confirm button is clicked, the submit is re-issued
  // and allowed through (see the click handler below).
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (
      !form ||
      !form.matches ||
      !form.matches('form[action$="/actions"]')
    ) {
      return;
    }
    var checked = form.querySelectorAll(
      '.row-check:checked',
    );
    if (checked.length === 0) {
      e.preventDefault();
      window.alert("Select at least one row first.");
      return;
    }
    if (form.dataset.bulkConfirmed === "1") return;
    var sel = form.querySelector('select[name="action"]');
    if (!sel) return;
    var opt = sel.options[sel.selectedIndex];
    if (!opt || opt.dataset.confirm !== "1") return;
    var dialog = document.getElementById("admin-bulk-confirm");
    if (!dialog || !dialog.showModal) return;
    e.preventDefault();
    var detail = dialog.querySelector("[data-bulk-confirm-detail]");
    if (detail) {
      detail.textContent =
        "Apply '" + opt.text + "' to " + checked.length + " record(s)?";
    }
    dialog.autumnBulkForm = form;
    dialog.showModal();
  });

  // Bulk-action confirm dialog: clicking [data-bulk-confirm] closes the
  // dialog and re-submits the form it was opened for.
  document.addEventListener("click", function (e) {
    var confirmBtn = e.target.closest("[data-bulk-confirm]");
    if (!confirmBtn) return;
    var dialog = confirmBtn.closest("dialog");
    if (!dialog) return;
    var form = dialog.autumnBulkForm;
    dialog.close();
    if (form) {
      form.dataset.bulkConfirmed = "1";
      form.requestSubmit();
    }
  });

  // CSV import form: multipart/form-data bypasses form-field CSRF scanning, so
  // send the token as a header (CsrfLayer checks headers before reading the body).
  // Reads the token and optional custom header name from the existing csrf meta tag,
  // consistent with how the HTMX CSRF companion script works.
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (!form || !form.matches || !form.matches("#autumn-csv-import-form")) return;
    e.preventDefault();
    var meta = document.querySelector('meta[name="csrf-token"]');
    var header = (meta && meta.getAttribute("data-header")) || "X-CSRF-Token";
    var token = (meta && meta.getAttribute("content")) || "";
    var headers = token ? { [header]: token } : {};
    fetch(form.action, { method: "POST", headers: headers, body: new FormData(form) })
      .then(function (r) { return r.text(); })
      .then(function (h) { document.open(); document.write(h); document.close(); });
  });

  // Cosmetic client-side strip of blank password inputs so they aren't sent.
  // The real safety net is server-side in strip_meta_fields() using the
  // declared AdminFieldKind::Password metadata; this just avoids shipping
  // empty values over the wire.
  document.addEventListener(
    "submit",
    function (e) {
      var form = e.target;
      if (!form || !form.matches || !form.matches("form")) return;
      form
        .querySelectorAll('input[type="password"]')
        .forEach(function (i) {
          if (i.value === "") i.removeAttribute("name");
        });
    },
    true,
  );
})();
