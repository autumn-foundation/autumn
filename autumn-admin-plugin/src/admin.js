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
  // preventDefault() always runs once confirmation is required, regardless
  // of <dialog>.showModal support, so a destructive submit is never let
  // through unconfirmed: browsers without it fall back to window.confirm().
  // Once confirmed, the submit is re-issued and allowed through (see the
  // click handler below), which also clears the one-shot flag below.
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
    if (form.dataset.bulkConfirmed === "1") {
      delete form.dataset.bulkConfirmed;
      return;
    }
    var sel = form.querySelector('select[name="action"]');
    if (!sel) return;
    var opt = sel.options[sel.selectedIndex];
    if (!opt || opt.dataset.confirm !== "1") return;
    e.preventDefault();
    var message =
      "Apply '" + opt.text + "' to " + checked.length + " record(s)?";
    var dialog = document.getElementById("admin-bulk-confirm");
    if (!dialog || !dialog.showModal) {
      if (window.confirm(message)) {
        form.dataset.bulkConfirmed = "1";
        // Deferred: requestSubmit() called synchronously from within this
        // same form's still-dispatching submit handler is a no-op per the
        // HTML spec's reentrancy guard ("if form's firing submit event is
        // true, then return"). Scheduling it after the current dispatch
        // finishes lets the resubmit actually happen.
        setTimeout(function () {
          form.requestSubmit();
        }, 0);
      }
      return;
    }
    var detail = dialog.querySelector("[data-bulk-confirm-detail]");
    if (detail) detail.textContent = message;
    dialog.showModal();
  });

  // Bulk-action confirm dialog: clicking [data-bulk-confirm] re-submits the
  // bulk-action form. Closing the dialog itself is handled by the button's
  // own command="close"/data-modal-close attributes (see templates.rs),
  // routed through the same shared mechanism modal_close_button uses — no
  // bespoke close call needed here.
  document.addEventListener("click", function (e) {
    var confirmBtn = e.target.closest("[data-bulk-confirm]");
    if (!confirmBtn) return;
    var form = document.querySelector('form[action$="/actions"]');
    if (!form) return;
    form.dataset.bulkConfirmed = "1";
    form.requestSubmit();
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
