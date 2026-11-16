// Runs in <head> before the page paints, so a saved dark theme never flashes
// light (and the light default never flashes dark). Light unless the visitor
// chose dark on this site.
(function () {
  var theme = "light";
  try {
    if (localStorage.getItem("meshguard-theme") === "dark") theme = "dark";
  } catch (e) {}
  document.documentElement.setAttribute("data-theme", theme);
  document.documentElement.classList.add("js");
})();
