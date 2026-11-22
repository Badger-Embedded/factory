// The save bar of each settings form. Once an input of a `.settings-form`
// changes, its bar reads "Unsaved changes." and leaving the page asks first.
// Saving that form does not ask; submitting a different form on the page does,
// so its unsaved changes are not lost. Without this script the bar still holds
// the form's submit button, so the form saves the same way.
(function () {
  var dirty = new Set();
  var submitting = false;

  function mark(event) {
    var form = event.target.closest && event.target.closest(".settings-form");
    if (!form || dirty.has(form)) return;
    dirty.add(form);
    var bar = form.querySelector(".save-bar");
    if (!bar) return;
    bar.classList.add("dirty");
    var note = bar.querySelector(".save-bar-note");
    if (note) note.textContent = "Unsaved changes.";
  }

  // A form the server drew with unsaved changes, such as an edit that waits
  // for a confirm, starts out dirty.
  document.querySelectorAll(".settings-form .save-bar.dirty").forEach(function (bar) {
    dirty.add(bar.closest(".settings-form"));
  });

  document.addEventListener("input", mark);
  document.addEventListener("change", mark);
  // Adding or removing a header row changes the form without an input event.
  // This listens in the capture phase, before the header-row script removes
  // the clicked row from its form.
  document.addEventListener(
    "click",
    function (event) {
      if (event.target.matches(".add-header, .remove-header")) mark(event);
    },
    true,
  );
  document.addEventListener("submit", function (event) {
    if (event.target.closest && event.target.closest(".settings-form")) {
      submitting = true;
    }
  });
  window.addEventListener("beforeunload", function (event) {
    if (submitting || dirty.size === 0) return;
    event.preventDefault();
    event.returnValue = "";
  });
})();
