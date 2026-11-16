// MeshGuard website: theme toggle and the mobile menu. No tracking, no
// third-party scripts. The page works without JavaScript; this only adds the
// dark-theme switch and collapses the menu on small screens.
(function () {
  var root = document.documentElement;
  var themeColor = document.querySelector('meta[name="theme-color"]');
  var toggle = document.getElementById("theme-toggle");

  function current() {
    return root.getAttribute("data-theme") === "dark" ? "dark" : "light";
  }

  function apply(theme, save) {
    root.setAttribute("data-theme", theme);
    if (themeColor) themeColor.setAttribute("content", theme === "dark" ? "#0f1715" : "#f8faf9");
    if (toggle) {
      var dark = theme === "dark";
      toggle.setAttribute("aria-pressed", String(dark));
      toggle.setAttribute("aria-label", dark ? "Dark theme on. Switch to light theme" : "Switch to dark theme");
      toggle.title = dark ? "Switch to light theme" : "Switch to dark theme";
    }
    if (save) {
      try {
        localStorage.setItem("meshguard-theme", theme);
      } catch (e) {}
    }
  }

  apply(current(), false);
  if (toggle) {
    toggle.hidden = false;
    toggle.addEventListener("click", function () {
      apply(current() === "dark" ? "light" : "dark", true);
    });
  }

  // Mobile menu
  var menuBtn = document.getElementById("menu-toggle");
  var menu = document.getElementById("nav-menu");
  if (!menuBtn || !menu) return;

  function setOpen(open) {
    menu.classList.toggle("open", open);
    menuBtn.setAttribute("aria-expanded", String(open));
    menuBtn.setAttribute("aria-label", open ? "Close menu" : "Open menu");
  }

  menuBtn.addEventListener("click", function () {
    setOpen(menuBtn.getAttribute("aria-expanded") !== "true");
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && menuBtn.getAttribute("aria-expanded") === "true") {
      setOpen(false);
      menuBtn.focus();
    }
  });
  menu.addEventListener("click", function (e) {
    if (e.target.closest("a")) setOpen(false);
  });
  window.matchMedia("(min-width: 900px)").addEventListener("change", function (mq) {
    if (mq.matches) setOpen(false);
  });
})();
