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

  // A document icon for a text file, an archive icon for a name ending in
  // .zip. The server's byte check, not this guess, decides what is accepted.
  function iconFor(file) {
    var zip = /\.zip$/i.test(file.name);
    var icon = document.createElement("span");
    icon.className = "file-icon-preview";
    icon.setAttribute("aria-hidden", "true");
    icon.textContent = zip ? "🗄" : "📄";
    return icon;
  }

  function render() {
    list.textContent = "";
    files.forEach(function (file, index) {
      var item = document.createElement("li");
      var objectUrl = null;
      if (file.type.indexOf("image/") === 0) {
        var img = document.createElement("img");
        objectUrl = URL.createObjectURL(file);
        img.src = objectUrl;
        img.alt = "Attached image " + (index + 1);
        item.appendChild(img);
      } else {
        var tile = document.createElement("span");
        tile.className = "file-tile";
        tile.appendChild(iconFor(file));
        var name = document.createElement("span");
        name.className = "file-name";
        name.textContent = file.name;
        tile.appendChild(name);
        item.appendChild(tile);
      }
      var remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "Remove";
      remove.setAttribute("aria-label", "Remove attachment " + (index + 1));
      remove.addEventListener("click", function () {
        if (objectUrl) URL.revokeObjectURL(objectUrl);
        files.splice(index, 1);
        sync();
        render();
      });
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
