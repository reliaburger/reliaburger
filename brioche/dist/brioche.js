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

    // -- Units ----------------------------------------------------------
    //
    // Each chart config carries its unit as a ladder of steps, smallest
    // first: `[{factor: 1, suffix: " B"}, {factor: 1024, suffix: " KiB"}, …]`.
    // The ladders and the rules below are mirrored by src/brioche/units.rs,
    // whose unit tests pin the boundaries (1023 B / 1 KiB, 999 µs / 1 ms).

    var PLAIN = [{ factor: 1, suffix: "" }];

    // The largest step whose factor doesn't exceed `magnitude`; zero uses
    // the base (factor 1) step. Rust: `pick_step`.
    function pickStep(steps, magnitude) {
        var chosen = steps[0];
        for (var i = 0; i < steps.length; i++) {
            var fits = magnitude === 0 ? steps[i].factor === 1 : steps[i].factor <= magnitude;
            if (fits) chosen = steps[i];
        }
        return chosen;
    }

    // Three significant figures at most, trailing zeros dropped. Rust:
    // `format_in`.
    function formatIn(step, value) {
        var scaled = value / step.factor;
        var magnitude = Math.abs(scaled);
        var decimals = magnitude >= 100 ? 0 : magnitude >= 10 ? 1 : 2;
        return String(Number(scaled.toFixed(decimals))) + step.suffix;
    }

    // One value in its own best step, for the legend. Rust:
    // `ChartUnit::format`.
    function formatValue(steps, value) {
        return formatIn(pickStep(steps, Math.abs(value)), value);
    }

    // Tick spacings of 1, 2 and 5 times each step, so a byte axis ticks
    // every 2 MiB rather than every 2,000,000 bytes. The smallest step also
    // gets fractions and the largest big multiples, for values outside the
    // ladder.
    function increments(steps) {
        var multiples = [1, 2, 5, 10, 20, 50, 100, 200, 500];
        var out = [];
        steps.forEach(function (step, i) {
            var extra = [];
            if (i === 0) extra = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5];
            if (i === steps.length - 1) extra = extra.concat([1e3, 2e3, 5e3, 1e4, 2e4, 5e4, 1e5, 2e5, 5e5, 1e6]);
            multiples.concat(extra).forEach(function (m) { out.push(m * step.factor); });
        });
        return out.sort(function (a, b) { return a - b; });
    }

    // Tick labels sharing the step chosen by the largest tick. Rust:
    // `ChartUnit::format_axis`.
    function formatAxis(steps, splits) {
        var largest = 0;
        for (var i = 0; i < splits.length; i++) {
            largest = Math.max(largest, Math.abs(splits[i]));
        }
        var step = pickStep(steps, largest);
        return splits.map(function (v) { return formatIn(step, v); });
    }

    // The newest non-empty value of a series, for the legend when nobody is
    // hovering over the chart.
    function latest(values) {
        for (var i = values.length - 1; i >= 0; i--) {
            if (values[i] != null) return values[i];
        }
        return null;
    }

    // -- Theme ----------------------------------------------------------

    // Canvas text can't use CSS variables, so read the tokens brioche.css
    // defines once and hand uPlot plain colours.
    function theme() {
        var style = getComputedStyle(document.documentElement);
        function token(name, fallback) {
            return style.getPropertyValue(name).trim() || fallback;
        }
        return {
            text: token("--fg-muted", "#b4bccb"),
            grid: token("--chart-grid", "#2f3b5c")
        };
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
        var steps = cfg.unit && cfg.unit.length ? cfg.unit : PLAIN;
        var colours = theme();
        // Without a cursor on the chart uPlot asks for the value at a null
        // index; show the newest value (and "latest" for the time) instead
        // of its "--" placeholder.
        var series = [{
            label: "Time",
            value: function (u, v, sidx, idx) {
                return idx == null ? "latest" : new Date(v * 1000).toLocaleTimeString();
            }
        }];
        for (var i = 0; i < chart.series.length; i++) {
            series.push({
                label: chart.series[i].label,
                stroke: COLOURS[i % COLOURS.length],
                width: 2,
                spanGaps: true,
                value: function (u, v, sidx, idx) {
                    var shown = idx == null ? latest(u.data[sidx]) : v;
                    return shown == null ? "no data" : formatValue(steps, shown);
                }
            });
        }
        var axis = {
            stroke: colours.text,
            grid: { stroke: colours.grid, width: 1 },
            ticks: { stroke: colours.grid, width: 1 }
        };
        var opts = {
            width: el.clientWidth || 400,
            height: 200,
            series: series,
            axes: [axis, Object.assign({}, axis, {
                size: 64,
                incrs: increments(steps),
                values: function (u, splits) { return formatAxis(steps, splits); }
            })],
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
