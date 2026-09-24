// Brioche — custom JS for chart initialisation and log streaming.
// No frameworks, no build step, no third-party requests: uPlot and HTMX are
// vendored next to this file.

(function () {
    "use strict";

    // -- Charts via uPlot -----------------------------------------------

    // One colour per line, cycled.
    var COLOURS = ["#4ecca3", "#e8a33d", "#5aa9e6", "#e05f7b", "#a78bfa", "#9fd356"];

    function initCharts(root) {
        var els = (root || document).querySelectorAll("[data-chart-config]");
        for (var i = 0; i < els.length; i++) {
            initChart(els[i]);
        }
    }

    function initChart(el) {
        if (el._chartStarted) return; // already initialised
        var cfg;
        try {
            cfg = JSON.parse(el.getAttribute("data-chart-config"));
        } catch (e) {
            return;
        }
        el._chartStarted = true;

        fetchChartData(el, cfg);
        if (cfg.refresh_secs > 0) {
            setInterval(function () {
                fetchChartData(el, cfg);
            }, cfg.refresh_secs * 1000);
        }
    }

    // Raw metric rows (`/v1/metrics` answers an array, the per-app endpoint
    // `{data: [...]}`) become one line per `instance` label, or per label
    // set when there's no instance.
    function rowsToChart(rows) {
        var byLabel = {};
        var order = [];
        var times = {};
        for (var i = 0; i < rows.length; i++) {
            var row = rows[i];
            var label = "value";
            try {
                var labels = JSON.parse(row.labels || "{}");
                label = labels.instance || (row.labels && row.labels !== "{}" ? row.labels : "value");
            } catch (e) {
                // Unparseable labels: draw under the default line.
            }
            if (!byLabel[label]) {
                byLabel[label] = {};
                order.push(label);
            }
            byLabel[label][row.timestamp] = (byLabel[label][row.timestamp] || 0) + row.value;
            times[row.timestamp] = true;
        }
        var timestamps = Object.keys(times).map(Number).sort(function (a, b) { return a - b; });
        return {
            timestamps: timestamps,
            series: order.map(function (label) {
                return {
                    label: label,
                    values: timestamps.map(function (t) {
                        return t in byLabel[label] ? byLabel[label][t] : null;
                    })
                };
            })
        };
    }

    // Accept every shape a chart endpoint answers with and return
    // `{timestamps, series: [{label, values}]}`, or null.
    function toChart(body) {
        if (Array.isArray(body)) return rowsToChart(body);
        if (body && Array.isArray(body.timestamps) && Array.isArray(body.series)) return body;
        if (body && Array.isArray(body.data)) return rowsToChart(body.data);
        return null;
    }

    // Axis ticks short enough not to run into the axis label: 8M, not
    // 8,000,000.
    function compact(value) {
        var magnitude = Math.abs(value);
        if (magnitude >= 1e9) return (value / 1e9).toFixed(1).replace(/\.0$/, "") + "G";
        if (magnitude >= 1e6) return (value / 1e6).toFixed(1).replace(/\.0$/, "") + "M";
        if (magnitude >= 1e4) return (value / 1e3).toFixed(1).replace(/\.0$/, "") + "k";
        return String(Number(value.toPrecision(3)));
    }

    function draw(el, cfg, chart) {
        var labels = chart.series.map(function (s) { return s.label; }).join("\u0000");
        var data = [chart.timestamps].concat(chart.series.map(function (s) { return s.values; }));
        // uPlot fixes its series at construction, so a new instance (or one
        // gone) means a new plot.
        if (el._uplot && el._chartLabels === labels) {
            el._uplot.setData(data);
            return;
        }
        if (el._uplot) {
            el._uplot.destroy();
        }
        var series = [{}];
        for (var i = 0; i < chart.series.length; i++) {
            series.push({
                label: chart.series[i].label,
                stroke: COLOURS[i % COLOURS.length],
                width: 2,
                spanGaps: true
            });
        }
        var opts = {
            width: el.clientWidth || 400,
            height: 200,
            series: series,
            axes: [{}, {
                label: cfg.y_label || "",
                values: function (u, splits) { return splits.map(compact); }
            }],
            scales: { x: { time: true } }
        };
        el._uplot = new uPlot(opts, data, el);
        el._chartLabels = labels;
    }

    function fetchChartData(el, cfg) {
        var now = Math.floor(Date.now() / 1000);
        var start = now - (cfg.range_secs || 3600);
        var sep = cfg.endpoint.indexOf("?") >= 0 ? "&" : "?";
        var url = cfg.endpoint + sep + "start=" + start + "&end=" + now;
        fetch(url)
            .then(function (r) { return r.json(); })
            .then(function (body) {
                var chart = toChart(body);
                if (!chart || chart.series.length === 0) return;
                draw(el, cfg, chart);
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
