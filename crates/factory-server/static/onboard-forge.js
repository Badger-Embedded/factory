(function () {
  var radios = document.querySelectorAll('input[name="forge"]');
  var repo = document.getElementById("repo");
  var hints = document.querySelectorAll("[data-forge]");
  function update() {
    var chosen = document.querySelector('input[name="forge"]:checked');
    if (!chosen) return;
    repo.placeholder = chosen.dataset.placeholder;
    hints.forEach(function (hint) {
      hint.hidden = hint.dataset.forge !== chosen.value;
    });
  }
  radios.forEach(function (radio) {
    radio.addEventListener("change", update);
  });
  update();
})();
