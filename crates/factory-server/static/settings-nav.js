(function () {
  var nav = document.querySelector(".settings-nav");
  var current = nav && nav.querySelector('[aria-current="page"]');
  if (!current || nav.scrollHeight <= nav.clientHeight) return;
  var navBox = nav.getBoundingClientRect();
  var box = current.getBoundingClientRect();
  var style = getComputedStyle(nav);
  var top = navBox.top + parseFloat(style.paddingTop);
  var bottom = navBox.bottom - parseFloat(style.paddingBottom);
  if (box.bottom > bottom) nav.scrollTop += box.bottom - bottom;
  else if (box.top < top) nav.scrollTop -= top - box.top;
})();
