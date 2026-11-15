package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

var menu = []string{"cheeseburger", "double smash", "mushroom swiss", "veggie deluxe"}

// Row is one line of the burger: a slice an instance served, or a gap where
// no instance of the layer answered.
type Row struct {
	Layer     string   `json:"layer"`
	Version   string   `json:"version,omitempty"`
	Style     string   `json:"style,omitempty"`
	Node      string   `json:"node,omitempty"`
	Instance  string   `json:"instance,omitempty"`
	LatencyMS *float64 `json:"latency_ms,omitempty"`
	Missing   string   `json:"missing,omitempty"` // why the layer isn't there
	text, svg string
	height    int
}

// Burger is what the counter assembled for one request.
type Burger struct {
	Counter  string `json:"counter"`
	Complete bool   `json:"complete"`
	Rows     []Row  `json:"layers"`
	Cooked   uint64 `json:"patties_cooked"`
}

// Order is what GET /order returns.
type Order struct {
	Number   uint64   `json:"number"`
	Burger   string   `json:"burger"`
	Counter  string   `json:"counter"`
	Complete bool     `json:"complete"`
	Missing  []string `json:"missing,omitempty"`
}

// tally is one counter's count of the patties the batch cooked.
type tally struct {
	Counter string `json:"counter"`
	Cooked  uint64 `json:"cooked"`
}

type sighting struct {
	row      Row
	lastSeen time.Time
}

type tallySighting struct {
	cooked uint64
	at     time.Time
}

// counter is the front of the shop. It finds every layer by its service
// name, stacks what answers and leaves a gap for what doesn't.
type counter struct {
	node, instance string
	urlFor         func(service string) string
	client         *http.Client
	// Each request asks every layer this many times over fresh connections.
	// The cluster balances connections round-robin, shared by every caller
	// on the node, so ask more often than there are patties.
	probes  int
	timeout time.Duration
	// How long an instance that answered stays on the burger without
	// answering again, so a triple patty doesn't flicker when other traffic
	// takes a turn of the round-robin.
	remember time.Duration
	now      func() time.Time
	metrics  *metrics
	orders   atomic.Uint64

	mu      sync.Mutex
	seen    map[string]map[string]sighting // layer -> node/instance -> last answer
	cooked  map[string]bool                // the patties reported here
	tallies map[string]tallySighting       // counter -> its last tally
}

// tallyMemory is how long a counter's tally counts after it last answered.
const tallyMemory = time.Minute

func newCounter(node, instance string, urlFor func(string) string) *counter {
	return &counter{
		node:     node,
		instance: instance,
		urlFor:   urlFor,
		// A fresh connection per request, so each one is balanced anew.
		client:   &http.Client{Transport: &http.Transport{DisableKeepAlives: true}},
		probes:   6,
		timeout:  2 * time.Second,
		remember: 5 * time.Second,
		now:      time.Now,
		metrics:  newMetrics(),
		seen:     map[string]map[string]sighting{},
		cooked:   map[string]bool{},
		tallies:  map[string]tallySighting{},
	}
}

func (c *counter) id() string { return c.node + " #" + c.instance }

func (c *counter) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /{$}", c.burger)
	mux.HandleFunc("GET /burger.svg", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "image/svg+xml")
		w.Header().Set("Cache-Control", "no-store")
		fmt.Fprint(w, renderSVG(c.assemble(r.Context())))
	})
	mux.HandleFunc("GET /order", c.order)
	mux.HandleFunc("POST /cooked", c.reportCooked)
	mux.HandleFunc("GET /tally", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, tally{Counter: c.id(), Cooked: c.ownTally()})
	})
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("GET /metrics", c.metrics.serve)
	return c.metrics.wrap(mux)
}

// burger answers curl with the terminal burger, a browser with the SVG page
// and a script with JSON.
func (c *counter) burger(w http.ResponseWriter, r *http.Request) {
	burger := c.assemble(r.Context())
	accept := r.Header.Get("Accept")
	switch {
	case strings.Contains(accept, "text/html"):
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		fmt.Fprint(w, renderHTML(burger))
	case strings.Contains(accept, "application/json"):
		writeJSON(w, burger)
	default:
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		fmt.Fprint(w, renderText(burger))
	}
}

func (c *counter) order(w http.ResponseWriter, r *http.Request) {
	number := c.orders.Add(1)
	burger := c.assemble(r.Context())
	order := Order{
		Number:   number,
		Burger:   menu[(number-1)%uint64(len(menu))],
		Counter:  c.id(),
		Complete: burger.Complete,
	}
	for _, row := range burger.Rows {
		if row.Missing != "" {
			order.Missing = append(order.Missing, row.Layer)
		}
	}
	writeJSON(w, order)
}

func (c *counter) reportCooked(w http.ResponseWriter, r *http.Request) {
	var report struct {
		Patty string `json:"patty"`
	}
	if err := json.NewDecoder(r.Body).Decode(&report); err != nil || report.Patty == "" {
		http.Error(w, `want {"patty": "<batch>/<profile>/<index>"}`, http.StatusBadRequest)
		return
	}
	c.mu.Lock()
	// A retried task reports the same patty again; count it once.
	c.cooked[report.Patty] = true
	cooked := uint64(len(c.cooked))
	c.mu.Unlock()
	writeJSON(w, tally{Counter: c.id(), Cooked: cooked})
}

func (c *counter) ownTally() uint64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return uint64(len(c.cooked))
}

type statusError int

func (s statusError) Error() string { return fmt.Sprintf("answered %d", int(s)) }

// fetch GETs a service's path as JSON into out, and says how long it took.
func (c *counter) fetch(ctx context.Context, service, path string, out any) (time.Duration, error) {
	ctx, cancel := context.WithTimeout(ctx, c.timeout)
	defer cancel()
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, c.urlFor(service)+path, nil)
	if err != nil {
		return 0, err
	}
	started := time.Now()
	response, err := c.client.Do(request)
	if err != nil {
		return 0, err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return 0, statusError(response.StatusCode)
	}
	if err := json.NewDecoder(response.Body).Decode(out); err != nil {
		return 0, err
	}
	return time.Since(started), nil
}

// describe says in a few words why a layer didn't answer.
func describe(err error) string {
	var status statusError
	var dns *net.DNSError
	var netErr net.Error
	switch {
	case errors.As(err, &status):
		return status.Error()
	case errors.Is(err, context.DeadlineExceeded), errors.As(err, &netErr) && netErr.Timeout():
		return "timed out"
	case errors.Is(err, syscall.ECONNREFUSED):
		return "not answering"
	// The cluster refuses a connection to a service with no healthy
	// instance, or one a fault partitions off.
	case errors.Is(err, syscall.EPERM):
		return "blocked by the cluster"
	case errors.As(err, &dns):
		return "no such service"
	default:
		return "unreachable"
	}
}

type probeResult struct {
	row Row
	err error
}

// assemble asks every layer for its slice and stacks the answers top to
// bottom, one row per instance that answered.
func (c *counter) assemble(ctx context.Context) Burger {
	results := make([][]probeResult, len(layerOrder))
	tallies := make([]*tally, c.probes)
	var wg sync.WaitGroup
	for i, name := range layerOrder {
		results[i] = make([]probeResult, c.probes)
		for p := range c.probes {
			wg.Add(1)
			go func() {
				defer wg.Done()
				var slice Slice
				took, err := c.fetch(ctx, name, "/layer", &slice)
				latency := float64(took.Microseconds()) / 1000
				results[i][p] = probeResult{Row{
					Layer: name, Version: slice.Version, Style: slice.Style,
					Node: slice.Node, Instance: slice.Instance, LatencyMS: &latency,
					text: slice.Text, svg: slice.SVG, height: slice.Height,
				}, err}
			}()
		}
	}
	for p := range c.probes {
		wg.Add(1)
		go func() {
			defer wg.Done()
			var t tally
			if _, err := c.fetch(ctx, "counter", "/tally", &t); err == nil && t.Counter != "" {
				tallies[p] = &t
			}
		}()
	}
	wg.Wait()

	c.mu.Lock()
	defer c.mu.Unlock()
	now := c.now()
	burger := Burger{Counter: c.id(), Complete: true}
	for i, name := range layerOrder {
		rows, reason := c.stack(name, results[i], now)
		if len(rows) == 0 {
			burger.Complete = false
			burger.Rows = append(burger.Rows, Row{Layer: name, Missing: reason})
			continue
		}
		burger.Rows = append(burger.Rows, rows...)
	}
	burger.Cooked = c.sumTallies(tallies, now)
	return burger
}

// stack merges a layer's answers into what the counter has seen and
// returns the instances to draw, by node and instance, so every counter
// stacks them in the same order. With no answer at all it forgets the layer
// and returns why.
func (c *counter) stack(name string, results []probeResult, now time.Time) ([]Row, string) {
	seen := c.seen[name]
	if seen == nil {
		seen = map[string]sighting{}
		c.seen[name] = seen
	}
	reason := ""
	answered := false
	for _, result := range results {
		if result.err != nil {
			reason = describe(result.err)
			continue
		}
		answered = true
		key := result.row.Node + "/" + result.row.Instance
		seen[key] = sighting{row: result.row, lastSeen: now}
	}
	if !answered {
		delete(c.seen, name)
		return nil, reason
	}
	var sightings []sighting
	for key, s := range seen {
		if now.Sub(s.lastSeen) > c.remember {
			delete(seen, key)
			continue
		}
		sightings = append(sightings, s)
	}
	sort.Slice(sightings, func(i, j int) bool {
		a, b := sightings[i].row, sightings[j].row
		return a.Node+"/"+a.Instance < b.Node+"/"+b.Instance
	})
	rows := make([]Row, len(sightings))
	for i, s := range sightings {
		rows[i] = s.row
	}
	return rows, ""
}

// sumTallies adds up the patties every counter reported, this one included.
// Each counter only knows the reports that reached it, and the service
// spreads them across all of them.
func (c *counter) sumTallies(answers []*tally, now time.Time) uint64 {
	for _, t := range answers {
		if t != nil {
			c.tallies[t.Counter] = tallySighting{t.Cooked, now}
		}
	}
	c.tallies[c.id()] = tallySighting{uint64(len(c.cooked)), now}
	var sum uint64
	for counter, t := range c.tallies {
		if now.Sub(t.at) > tallyMemory {
			delete(c.tallies, counter)
			continue
		}
		sum += t.cooked
	}
	return sum
}

func writeJSON(w http.ResponseWriter, value any) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(value)
}
