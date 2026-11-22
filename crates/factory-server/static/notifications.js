(function () {
  if (!("Notification" in window)) return;
  var STORE = "factory.notified";
  var LOCK = "factory-notifications";
  var pending = false;

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

  function fetchAndShow() {
    return fetch("/notifications", { headers: { Accept: "application/json" } })
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
        try {
          localStorage.setItem(
            STORE,
            JSON.stringify(
              data.items.map(function (item) {
                return item.key;
              })
            )
          );
        } catch (e) {}
      })
      .catch(function () {});
  }

  function poll() {
    if (pending || Notification.permission !== "granted") return;
    pending = true;
    var done = function () {
      pending = false;
    };
    var run = navigator.locks
      ? navigator.locks.request(LOCK, fetchAndShow)
      : fetchAndShow();
    run.then(done, done);
  }

  poll();
  setInterval(poll, 5000);
})();
