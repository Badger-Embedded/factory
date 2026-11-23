(function () {
  const picker = document.getElementById("project-picker");
  const summary = picker.querySelector("summary");
  picker.addEventListener("toggle", function () {
    summary.setAttribute("aria-expanded", picker.open ? "true" : "false");
  });
  document.addEventListener("click", function (event) {
    if (picker.open && !picker.contains(event.target)) picker.open = false;
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape" && picker.open) {
      picker.open = false;
      summary.focus();
    }
  });
})();
