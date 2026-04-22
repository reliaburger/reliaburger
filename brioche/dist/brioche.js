// Brioche — custom JS for chart initialisation and log streaming.
// Total: ~100 lines. No frameworks, no build step.

(function () {
    "use strict";

    // -- Chart initialisation via uPlot ---------------------------------

    function initCharts(root) {
        var els = (root || document).querySelectorAll("[data-chart-config]");
        for (var i = 0; i < els.length; i++) {
            initChart(els[i]);
        }
    }

    function initChart(el) {
        if (el._uplot) return; // already initialised
        var cfg;
        try {
            cfg = JSON.parse(el.getAttribute("data-chart-config"));
        } catch (e) {
            return;
        }

        var width = el.clientWidth || 400;
        var opts = {
            width: width,
            height: 200,
            title: cfg.title,
            series: [
                {},
                { label: cfg.y_label || "value", stroke: "#4ecca3", width: 2 }
            ],
            axes: [
                {},
                { label: cfg.y_label || "" }
            ],
            scales: { x: { time: true } }
        };

        // Start with empty data; fetch will populate it.
        var data = [[], []];
        var plot = new uPlot(opts, data, el);
        el._uplot = plot;

        fetchChartData(el, cfg, plot);
        if (cfg.refresh_secs > 0) {
            setInterval(function () {
                fetchChartData(el, cfg, plot);
            }, cfg.refresh_secs * 1000);
        }
    }

    function fetchChartData(el, cfg, plot) {
        var now = Math.floor(Date.now() / 1000);
        var start = now - (cfg.range_secs || 3600);
        var sep = cfg.endpoint.indexOf("?") >= 0 ? "&" : "?";
        var url = cfg.endpoint + sep + "start=" + start + "&end=" + now;
        fetch(url)
            .then(function (r) { return r.json(); })
            .then(function (rows) {
                if (!Array.isArray(rows) || rows.length === 0) return;
                var ts = [], vals = [];
                for (var i = 0; i < rows.length; i++) {
                    ts.push(rows[i].timestamp);
                    vals.push(rows[i].value);
                }
                plot.setData([ts, vals]);
            })
            .catch(function () {
                // Metrics unavailable — leave chart empty.
            });
    }

    // -- Log streaming via SSE ------------------------------------------

    function initLogStreams(root) {
        var els = (root || document).querySelectorAll("[data-log-stream]");
        for (var i = 0; i < els.length; i++) {
            initLogStream(els[i]);
        }
    }

    function initLogStream(el) {
        if (el._eventsource) return;
        var url = el.getAttribute("data-log-stream");
        if (!url) return;

        var source = new EventSource(url);
        el._eventsource = source;

        source.onmessage = function (event) {
            var line = document.createElement("div");
            line.className = "log-line";
            line.textContent = event.data;
            el.appendChild(line);
            // Auto-scroll to bottom.
            el.scrollTop = el.scrollHeight;
        };

        source.onerror = function () {
            // SSE auto-reconnects; nothing to do.
        };
    }

    // -- Lifecycle hooks ------------------------------------------------

    document.addEventListener("DOMContentLoaded", function () {
        initCharts();
        initLogStreams();
    });

    // Re-init charts after HTMX swaps new content into the DOM.
    document.addEventListener("htmx:afterSettle", function (evt) {
        initCharts(evt.detail.target);
        initLogStreams(evt.detail.target);
    });
})();
