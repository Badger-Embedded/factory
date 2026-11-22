// The browser's notification permission on the Notifications settings page.
// It fills the permission line and shows the Allow button while the browser
// has not been asked. Without the Notification API or this script the block
// stays hidden.
(function () {
  var box = document.getElementById("notification-permission");
  if (!box || !("Notification" in window)) return;
  var line = box.querySelector(".permission-state");
  var button = box.querySelector("button");

  function show() {
    var permission = Notification.permission;
    button.hidden = permission !== "default";
    if (permission === "granted") {
      line.textContent = "Browser permission: granted.";
    } else if (permission === "denied") {
      line.textContent =
        "Browser permission: blocked. Unblock this site in the browser's site settings to get notifications.";
    } else {
      line.textContent = "Browser permission: not asked yet.";
    }
    box.hidden = false;
  }

  button.addEventListener("click", function () {
    var result = Notification.requestPermission();
    if (result && result.then) result.then(show);
    else show();
  });
  show();
})();
