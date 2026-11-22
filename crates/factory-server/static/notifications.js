(function () {
  if (!("Notification" in window)) return;
  var STORE = "factory.notified";
  var pending = false;

  function stored() {
    try {
      var state = JSON.parse(localStorage.getItem(STORE));
      if (state && Array.isArray(state.keys) && typeof state.at === "number") return state;
    } catch (e) {}
    return null;
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
    if (pending || Notification.permission !== "granted") return;
    pending = true;
    var started = Date.now();
    var done = function () {
      pending = false;
    };
    fetch("/notifications", { headers: { Accept: "application/json" } })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (data) {
        if (!data || !data.enabled) return;
        var state = stored();
        if (state && started < state.at) return;
        var seen = state ? state.keys : [];
        data.items.forEach(function (item) {
          if (state && seen.indexOf(item.key) === -1) show(item);
        });
        try {
          localStorage.setItem(
            STORE,
            JSON.stringify({
              at: started,
              keys: data.items.map(function (item) {
                return item.key;
              }),
            })
          );
        } catch (e) {}
      })
      .catch(function () {})
      .then(done);
  }

  poll();
  setInterval(poll, 5000);
})();
