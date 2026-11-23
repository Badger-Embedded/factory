(function () {
  var kinds = [
    { add: "add-header", remove: "remove-header", list: ".mcp-headers", row: ".mcp-header-row" },
    { add: "add-arg", remove: "remove-arg", list: ".mcp-args", row: ".mcp-arg-row" },
  ];
  document.addEventListener("click", function (event) {
    var button = event.target;
    kinds.forEach(function (kind) {
      if (button.classList.contains(kind.add)) {
        var field = button.closest(".field");
        var list = field && field.querySelector(kind.list);
        var template = field && field.querySelector("template");
        if (!list || !template) return;
        var row = template.content.firstElementChild.cloneNode(true);
        list.appendChild(row);
        row.querySelector("input").focus();
      } else if (button.classList.contains(kind.remove)) {
        var gone = button.closest(kind.row);
        if (gone) gone.remove();
      }
    });
  });
})();
