package main

import (
	"fmt"
	"html"
	"strings"
)

// gapLine stands where a missing layer would be, as wide as a layer's line.
const gapLine = "\x1b[2m · · · · · · · · · · · ·  \x1b[0m"

func human(layer string) string { return strings.ReplaceAll(layer, "-", " ") }

func latency(row Row) string {
	if row.LatencyMS == nil {
		return ""
	}
	return fmt.Sprintf("%6.1f ms", *row.LatencyMS)
}

// renderText draws the burger for a terminal: one ANSI-coloured line per
// layer, with its version and the instance that served it.
func renderText(b Burger) string {
	var out strings.Builder
	out.WriteString("\n")
	for _, row := range b.Rows {
		if row.Missing != "" {
			fmt.Fprintf(&out, "  %s   %s%-11s missing: %s%s\n", gapLine, fg(160), human(row.Layer), row.Missing, reset)
			continue
		}
		fmt.Fprintf(&out, "  %s   %-11s %-3s %-14s %s #%s %s\n",
			row.text, human(row.Layer), row.Version, row.Style, row.Node, row.Instance, latency(row))
	}
	fmt.Fprintf(&out, "\n  patties cooked: %d · served by counter %s\n\n", b.Cooked, b.Counter)
	return out.String()
}

// The SVG: layers on the left, what served them on the right.
const (
	svgWidth   = 800
	svgLabelX  = 280
	svgGap     = 4
	svgMissing = 18
)

func renderSVG(b Burger) string {
	var body strings.Builder
	y := 10
	for _, row := range b.Rows {
		if row.Missing != "" {
			fmt.Fprintf(&body, `<g transform="translate(0,%d)"><rect x="8" y="1" width="244" height="%d" rx="6" fill="none" stroke="#b0a090" stroke-dasharray="6 5"/>`+
				`<text x="%d" y="13" fill="#b3261e">%s missing: %s</text></g>`,
				y, svgMissing-2, svgLabelX, human(row.Layer), html.EscapeString(row.Missing))
			y += svgMissing + svgGap
			continue
		}
		fmt.Fprintf(&body, `<g transform="translate(0,%d)">%s<text x="%d" y="%d">%s %s · %s · %s #%s · %s</text></g>`,
			y, row.svg, svgLabelX, row.height/2+5, human(row.Layer), row.Version,
			html.EscapeString(row.Style), html.EscapeString(row.Node), html.EscapeString(row.Instance),
			strings.TrimSpace(latency(row)))
		y += row.height + svgGap
	}
	fmt.Fprintf(&body, `<text x="8" y="%d" class="footer">patties cooked: %d · served by counter %s</text>`,
		y+22, b.Cooked, html.EscapeString(b.Counter))
	return fmt.Sprintf(`<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" viewBox="0 0 %d %d" `+
		`font-family="ui-monospace, Menlo, monospace" font-size="13" fill="#3b2a1a">%s</svg>`,
		svgWidth, y+34, svgWidth, y+34, body.String())
}

// renderHTML is the browser's page: the same burger as SVG, fetched again
// every second.
func renderHTML(b Burger) string {
	return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Reliaburger</title>
<style>
  body { margin: 0; padding: 24px 16px; background: #fdf6ec; color: #3b2a1a;
         font-family: ui-sans-serif, system-ui, sans-serif; }
  h1 { font-size: 20px; margin: 0 0 16px; }
  #burger svg { max-width: 100%; height: auto; }
</style>
</head>
<body>
<h1>Your burger, layer by layer</h1>
<div id="burger">` + renderSVG(b) + `</div>
<script>
  // Redraw from /burger.svg every second; keep the last burger if a fetch fails.
  const burger = document.getElementById("burger");
  setInterval(() => {
    fetch("/burger.svg", { cache: "no-store" })
      .then((response) => response.ok ? response.text() : Promise.reject(response.status))
      .then((svg) => { burger.innerHTML = svg; })
      .catch(() => {});
  }, 1000);
</script>
</body>
</html>
`
}
