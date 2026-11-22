(function () {
  if (!("Notification" in window)) return;
  var STORE = "factory.notified";
  var LOCK = "factory-notifications";
  var pending = false;

  function stored() {
    try {
      var state = JSON.parse(localStorage.getItem(STORE));
      if (state && Array.isArray(state.keys) && Array.isArray(state.snapshot)) return state;
    } catch (e) {}
    return null;
  }

  function newer(a, b) {
    return a[0] > b[0] || (a[0] === b[0] && a[1] > b[1]);
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

  function update(data) {
    var state = stored();
    if (state && !newer(data.snapshot, state.snapshot)) return;
    var seen = state ? state.keys : [];
    data.items.forEach(function (item) {
      if (state && seen.indexOf(item.key) === -1) show(item);
    });
    try {
      localStorage.setItem(
        STORE,
        JSON.stringify({
          snapshot: data.snapshot,
          keys: data.items.map(function (item) {
            return item.key;
          }),
        })
      );
    } catch (e) {}
  }

  function fetchAndUpdate() {
    return fetch("/notifications", { headers: { Accept: "application/json" } })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (data) {
        if (data && data.enabled && Array.isArray(data.snapshot)) update(data);
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
      ? navigator.locks.request(LOCK, fetchAndUpdate)
      : fetchAndUpdate();
    run.then(done, done);
  }

  poll();
  setInterval(poll, 5000);
})();
