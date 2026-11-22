(function () {
  var row = document.querySelector(".status-row.has-source");
  var link = row && row.querySelector(".source-link");
  if (!link || !window.ResizeObserver) return;
  var usage = row.querySelector("#usage");
  function measure() {
    row.classList.remove("clear");
    row.classList.add("corner");
    row.style.setProperty("--source-w", link.offsetWidth + "px");
    row.style.setProperty("--source-h", link.offsetHeight + "px");
    var box = link.getBoundingClientRect();
    var strip = usage && usage.getBoundingClientRect();
    if (strip && strip.height > 0 && strip.top < box.bottom && strip.right > box.left) {
      row.classList.add("clear");
    }
  }
  measure();
  var observer = new ResizeObserver(measure);
  observer.observe(row);
  observer.observe(link);
  if (usage) observer.observe(usage);
})();
