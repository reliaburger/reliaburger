// One of the site's two scripts: it plays the tour recordings with the
// vendored asciinema player (assets/asciinema/). The recording is always on
// show; the player and the recording load from this site as the figure nears
// the viewport, so a visitor who never scrolls that far doesn't fetch them.
// Without JavaScript the <noscript> link and the written tour still work.
(function () {
  "use strict";

  var figures = document.querySelectorAll(".recording[data-cast]");
  var assets = null;

  function load(tag, attributes) {
    return new Promise(function (resolve, reject) {
      var element = document.createElement(tag);
      Object.keys(attributes).forEach(function (name) {
        element.setAttribute(name, attributes[name]);
      });
      element.onload = resolve;
      element.onerror = reject;
      document.head.appendChild(element);
    });
  }

  function playerAssets() {
    if (!assets) {
      assets = Promise.all([
        load("link", { rel: "stylesheet", href: "./assets/asciinema/asciinema-player.css" }),
        load("script", { src: "./assets/asciinema/asciinema-player.min.js" }),
      ]).catch(function (error) {
        assets = null;
        throw error;
      });
    }
    return assets;
  }

  figures.forEach(function (figure) {
    var screen = figure.querySelector(".recording-screen");
    var speeds = figure.querySelector(".recording-speed");
    var chapters = figure.querySelector(".recording-chapters");
    var cast = figure.getAttribute("data-cast");
    var poster = figure.getAttribute("data-poster");
    var player = null;
    var speed = 1;
    var started = false;
    var hasPlayed = false;
    var trimmedGaps = [];

    // The player takes its speed when it's created, so a new speed means a new
    // player that picks up where the old one was.
    function create(startAt, play) {
      if (player) player.dispose();
      player = window.AsciinemaPlayer.create(cast, screen, {
        poster: startAt ? undefined : poster,
        startAt: startAt || undefined,
        autoPlay: play,
        preload: true,
        speed: speed,
        fit: "width",
        terminalFontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
      });
      var playing = play;
      player.addEventListener("play", function () { playing = true; });
      player.addEventListener("pause", function () { playing = false; });
      player.addEventListener("ended", function () { playing = false; });
      player.isPlaying = function () { return playing; };
    }

    // The player reports its own trimmed time but starts from the recording's.
    function recordingTime(playerTime) {
      var time = playerTime;
      trimmedGaps.forEach(function (gap) {
        if (gap.trimmed <= playerTime) time = gap.at + playerTime - gap.trimmed;
      });
      return time;
    }

    function setSpeed(button) {
      speed = Number(button.getAttribute("data-speed"));
      speeds.querySelectorAll("button").forEach(function (other) {
        other.setAttribute("aria-pressed", String(other === button));
      });
      if (!player) return;
      var observed = player;
      Promise.resolve(hasPlayed ? observed.getCurrentTime() : 0).then(function (at) {
        if (player === observed) create(at > 0 ? recordingTime(at) : 0, observed.isPlaying());
      });
    }

    // A recording marks its own steps with "m" events, so the buttons and the
    // poster follow every re-recording. A button starts on the recording's own
    // time, while a poster is on the player's, which trims idle time.
    function chaptersFromMarkers() {
      return fetch(cast).then(function (response) {
        if (!response.ok) throw new Error(response.statusText);
        return response.text();
      }).then(function (text) {
        var lines = text.split("\n");
        var idleLimit = JSON.parse(lines[0]).idle_time_limit || Infinity;
        var posterChapter = figure.getAttribute("data-poster-chapter");
        var inPosterChapter = false;
        var at = 0;
        var trimmed = 0;
        lines.slice(1).forEach(function (line) {
          if (!line || line.charAt(0) === "#") return;
          var event = JSON.parse(line);
          var before = trimmed;
          at += event[0];
          trimmed += Math.min(event[0], idleLimit);
          if (event[0] > idleLimit) trimmedGaps.push({ trimmed: trimmed, at: at });
          if (event[1] !== "m") return;
          // The chapter's last frame: between its last output and the next
          // chapter, because a poster at an output's own time leaves it out.
          if (inPosterChapter) poster = "npt:" + (before + trimmed) / 2;
          inPosterChapter = event[2] === posterChapter;
          var button = document.createElement("button");
          button.type = "button";
          button.textContent = event[2];
          button.setAttribute("data-start", String(at));
          chapters.appendChild(button);
        });
      });
    }

    function start() {
      if (started) return;
      started = true;
      playerAssets().then(function () {
        figure.classList.add("has-player");
        speeds.hidden = false;
        if (figure.getAttribute("data-chapters") !== "markers") {
          if (chapters) chapters.hidden = false;
          create(0, false);
          return;
        }
        // Without its chapters the recording still plays, with its marks.
        return chaptersFromMarkers().then(function () {
          chapters.hidden = false;
        }).catch(function () {}).then(function () { create(0, false); });
      }).catch(function () {
        started = false;
      });
    }

    // Rendering the poster also seeks the player. Only user interaction
    // makes that position worth preserving across a speed change.
    screen.addEventListener("pointerdown", function () { hasPlayed = true; });
    screen.addEventListener("keydown", function () { hasPlayed = true; });

    speeds.addEventListener("click", function (event) {
      var button = event.target.closest("button[data-speed]");
      if (button) setSpeed(button);
    });
    // Chapters seek the original recording. Its timestamps and measured
    // durations stay intact even when the visitor skips a stage.
    if (chapters) chapters.addEventListener("click", function (event) {
      var button = event.target.closest("button[data-start]");
      if (!button || !player) return;
      var at = Number(button.getAttribute("data-start"));
      if (!Number.isFinite(at) || at < 0) return;
      hasPlayed = true;
      create(at, true);
    });
    // Start a screen or so early, so the poster frame is ready when it's seen.
    if (!("IntersectionObserver" in window)) {
      start();
      return;
    }
    var observer = new IntersectionObserver(function (entries) {
      if (!entries.some(function (entry) { return entry.isIntersecting; })) return;
      observer.disconnect();
      start();
    }, { rootMargin: "600px 0px" });
    observer.observe(figure);
  });
})();
