(function () {
  var bar = document.getElementById("permission-bar");
  if (!bar || !("Notification" in window)) return;
  var button = bar.querySelector("button");

  function show() {
    bar.hidden = Notification.permission !== "default";
  }

  button.addEventListener("click", function () {
    var result = Notification.requestPermission();
    if (result && result.then) result.then(show);
    else show();
  });
  show();
})();
