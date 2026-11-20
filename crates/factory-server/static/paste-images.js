(function () {
  var textarea = document.getElementById("request");
  var input = document.getElementById("images");
  var list = document.getElementById("pasted-images");
  if (!textarea || !input || !list || !window.DataTransfer) return;
  var files = [];

  function sync() {
    var transfer = new DataTransfer();
    files.forEach(function (file) {
      transfer.items.add(file);
    });
    input.files = transfer.files;
  }

  function render() {
    list.textContent = "";
    files.forEach(function (file, index) {
      var item = document.createElement("li");
      var img = document.createElement("img");
      img.src = URL.createObjectURL(file);
      img.alt = "Attached image " + (index + 1);
      var remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "Remove";
      remove.setAttribute("aria-label", "Remove image " + (index + 1));
      remove.addEventListener("click", function () {
        URL.revokeObjectURL(img.src);
        files.splice(index, 1);
        sync();
        render();
      });
      item.appendChild(img);
      item.appendChild(remove);
      list.appendChild(item);
    });
  }

  input.addEventListener("change", function () {
    files = Array.prototype.slice.call(input.files);
    render();
  });

  textarea.addEventListener("paste", function (event) {
    var items = (event.clipboardData && event.clipboardData.items) || [];
    var added = false;
    Array.prototype.forEach.call(items, function (item) {
      if (item.kind === "file" && item.type.indexOf("image/") === 0) {
        var file = item.getAsFile();
        if (file) {
          files.push(file);
          added = true;
        }
      }
    });
    if (added) {
      sync();
      render();
    }
  });
})();
