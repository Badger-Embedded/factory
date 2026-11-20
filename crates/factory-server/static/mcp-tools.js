// The filter, the count and the "All on" and "All off" buttons above a
// project's tool list. The filter hides the tools whose name does not hold the
// typed text. "All on" and "All off" set every shown tool and fire a change
// event, so the save bar notes the change. Delegated from the document, so it
// still works after "Refresh connection" swaps the list in again. Without this
// script the controls stay hidden.
(function () {
  document.documentElement.classList.add("mcp-tools-js");

  function count(list) {
    var boxes = list.querySelectorAll(".mcp-tool input[type=checkbox]");
    var on = 0;
    boxes.forEach(function (box) {
      if (box.checked) on += 1;
    });
    var label = list.querySelector(".mcp-tool-count");
    if (label) label.textContent = on + " of " + boxes.length + " on";
  }

  document.addEventListener("input", function (event) {
    if (!event.target.classList.contains("mcp-tool-filter")) return;
    var list = event.target.closest(".mcp-tools");
    var text = event.target.value.trim().toLowerCase();
    list.querySelectorAll(".mcp-tool").forEach(function (tool) {
      tool.hidden = text !== "" && tool.dataset.tool.indexOf(text) < 0;
    });
  });

  document.addEventListener("change", function (event) {
    var list = event.target.closest && event.target.closest(".mcp-tools");
    if (list) count(list);
  });

  document.addEventListener("click", function (event) {
    var all = event.target.classList.contains("mcp-tools-all");
    if (!all && !event.target.classList.contains("mcp-tools-none")) return;
    var list = event.target.closest(".mcp-tools");
    list.querySelectorAll(".mcp-tool:not([hidden]) input[type=checkbox]").forEach(function (box) {
      if (box.checked === all) return;
      box.checked = all;
      box.dispatchEvent(new Event("change", { bubbles: true }));
    });
  });
})();
