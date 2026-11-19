(function () {
  var log = document.querySelector("ol.log");
  if (!log) return;
  var key = "factory.log-scroll:" + location.pathname;
  var saved = null;
  try {
    saved = JSON.parse(sessionStorage.getItem(key));
  } catch (e) {}
  function atBottom() {
    return log.scrollHeight - log.scrollTop - log.clientHeight < 4;
  }
  if (saved && !saved.bottom) {
    log.scrollTop = saved.top;
  } else {
    log.scrollTop = log.scrollHeight;
  }
  window.addEventListener("pagehide", function () {
    try {
      sessionStorage.setItem(
        key,
        JSON.stringify({ bottom: atBottom(), top: log.scrollTop })
      );
    } catch (e) {}
  });
})();
