(function () {
  const picker = document.getElementById("project-picker");
  const summary = picker.querySelector("summary");
  const panel = picker.querySelector(".picker-panel");
  const current = picker.querySelector(".picker-current");

  function fitPanel() {
    panel.style.maxHeight = "";
    if (!picker.open) return;
    const room = window.innerHeight - panel.getBoundingClientRect().top - 8;
    panel.style.maxHeight = Math.max(room, 120) + "px";
  }

  picker.addEventListener("toggle", function () {
    summary.setAttribute("aria-expanded", picker.open ? "true" : "false");
    fitPanel();
  });
  window.addEventListener("resize", fitPanel);
  document.addEventListener("click", function (event) {
    if (picker.open && !picker.contains(event.target)) picker.open = false;
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape" && picker.open) {
      picker.open = false;
      summary.focus();
    }
  });

  let focusedHref = null;
  document.body.addEventListener("htmx:beforeSwap", function (event) {
    const target = event.detail.target;
    if (!target || target.id !== "picker-list") return;
    const active = document.activeElement;
    focusedHref = active && target.contains(active) ? active.getAttribute("href") : null;
  });
  document.body.addEventListener("htmx:afterSwap", function (event) {
    const target = event.detail.target;
    if (!target || target.id !== "picker-list") return;
    const list = document.getElementById("picker-list");
    const row = list.querySelector('a[aria-current="page"] .picker-name');
    if (row) current.textContent = row.textContent;
    if (focusedHref !== null) {
      for (const link of list.querySelectorAll("a")) {
        if (link.getAttribute("href") === focusedHref) link.focus();
      }
      focusedHref = null;
    }
  });
  // The selected project was removed elsewhere, so reopen the board on one that remains.
  document.body.addEventListener("htmx:responseError", function (event) {
    const target = event.detail.target;
    if (target && target.id === "picker-list" && event.detail.xhr.status === 404) {
      window.location.replace("/");
    }
  });
})();
