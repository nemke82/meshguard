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

  // Decorative animations: silent loops that play only while on screen. They
  // stay paused for visitors who ask for reduced motion or to save data, and
  // the button pauses or plays them (WCAG 2.2.2).
  // A press of the button wins over the preferences until the page reloads.
  var reduceMq = window.matchMedia("(prefers-reduced-motion: reduce)");
  var conn = navigator.connection;
  function autoplayAllowed() {
    return !reduceMq.matches && !(conn && conn.saveData);
  }
  var clips = [];
  function onPreferenceChange() {
    clips.forEach(function (c) {
      c.preferenceChanged();
    });
  }
  if (reduceMq.addEventListener) reduceMq.addEventListener("change", onPreferenceChange);
  else if (reduceMq.addListener) reduceMq.addListener(onPreferenceChange);
  if (conn && conn.addEventListener) conn.addEventListener("change", onPreferenceChange);

  Array.prototype.forEach.call(document.querySelectorAll(".media-video"), function (fig) {
    var video = fig.querySelector("video");
    var btn = fig.querySelector(".video-toggle");
    if (!video || !btn) return;
    var label = btn.getAttribute("aria-label").replace(/^Play /, "");
    var chosen = null; // null until the visitor presses the button
    var wanted = autoplayAllowed();
    var visible = false;

    function sync() {
      var playing = !video.paused;
      fig.classList.toggle("is-playing", playing);
      btn.setAttribute("aria-label", (playing ? "Pause " : "Play ") + label);
    }
    function update() {
      if (wanted && visible) {
        var p = video.play();
        if (p && p.catch) p.catch(function () {});
      } else if (!video.paused) {
        video.pause();
      }
    }

    video.addEventListener("play", sync);
    video.addEventListener("pause", sync);
    btn.hidden = false;
    btn.addEventListener("click", function () {
      wanted = chosen = video.paused;
      if (wanted) visible = true;
      update();
    });
    clips.push({
      preferenceChanged: function () {
        if (chosen !== null) return;
        wanted = autoplayAllowed();
        update();
      },
    });

    if ("IntersectionObserver" in window) {
      new IntersectionObserver(function (entries) {
        visible = entries[0].isIntersecting;
        update();
      }, { threshold: 0.25 }).observe(video);
    } else {
      visible = true;
      update();
    }
  });

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
