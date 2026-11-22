(function () {
  // Shows a browser notification for each job of the user that needs them or
  // failed. The server lists such jobs with a key that changes on every new
  // event, and the keys already shown live in localStorage, so a reload or a
  // second tab does not repeat one.
  if (!("Notification" in window)) return;
  var STORE = "factory.notified";

  function stored() {
    try {
      var list = JSON.parse(localStorage.getItem(STORE));
      return Array.isArray(list) ? list : null;
    } catch (e) {
      return null;
    }
  }

  function show(item) {
    var verb = item.kind === "failed" ? "failed" : "needs you";
    var note = new Notification("Job #" + item.job + " " + verb, {
      body: item.title,
      tag: item.key,
    });
    note.onclick = function () {
      window.focus();
      window.open(item.url);
      note.close();
    };
  }

  function poll() {
    if (Notification.permission !== "granted") return;
    fetch("/notifications", { headers: { Accept: "application/json" } })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (data) {
        if (!data || !data.enabled) return;
        var seen = stored();
        var first = seen === null;
        seen = seen || [];
        data.items.forEach(function (item) {
          if (seen.indexOf(item.key) === -1 && !first) show(item);
        });
        var keys = data.items.map(function (item) {
          return item.key;
        });
        try {
          localStorage.setItem(STORE, JSON.stringify(keys));
        } catch (e) {}
      })
      .catch(function () {});
  }

  poll();
  setInterval(poll, 5000);
})();
