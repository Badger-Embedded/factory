// The "Add header" and "Remove" buttons on the MCP server forms. Each header
// name sits in its own text box inside a `.mcp-headers` list. "Add header"
// clones the list's last row as an empty box; "Remove" drops its row. The forms
// still work without this script: you add one name per save and remove a name by
// clearing its box. Delegated from the document, so it covers every server form
// on the settings page, including rows added after load.
(function () {
  document.addEventListener("click", function (event) {
    var button = event.target;
    if (button.classList.contains("add-header")) {
      var field = button.closest(".field");
      var list = field && field.querySelector(".mcp-headers");
      var rows = list && list.querySelectorAll(".mcp-header-row");
      if (!rows || !rows.length) return;
      var row = rows[rows.length - 1].cloneNode(true);
      var input = row.querySelector("input");
      input.value = "";
      list.appendChild(row);
      input.focus();
    } else if (button.classList.contains("remove-header")) {
      var row = button.closest(".mcp-header-row");
      if (row) row.remove();
    }
  });
})();
