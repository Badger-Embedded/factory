// The save bar of each settings form. Once an input of a `.settings-form`
// changes, its bar reads "Unsaved changes." and leaving the page asks first.
// Without this script the bar still holds the form's submit button, so the form
// saves the same way.
(function () {
  var dirty = new Set();
  var submitting = null;

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
  // Adding or removing a header or argument row changes the form without an
  // input event. This listens in the capture phase, before the row script
  // removes the clicked row from its form.
  document.addEventListener(
    "click",
    function (event) {
      if (event.target.matches(".add-header, .remove-header, .add-arg, .remove-arg")) mark(event);
    },
    true,
  );
  document.addEventListener("submit", function (event) {
    submitting =
      (event.target.closest && event.target.closest(".settings-form")) || null;
  });
  window.addEventListener("beforeunload", function (event) {
    var unsaved = false;
    dirty.forEach(function (form) {
      if (form !== submitting) unsaved = true;
    });
    if (!unsaved) return;
    event.preventDefault();
    event.returnValue = "";
  });
})();
