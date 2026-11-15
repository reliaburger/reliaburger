package main

import (
	"fmt"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"
)

// buckets are Prometheus's default latency buckets, in seconds.
var buckets = []float64{0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10}

// metrics counts and times requests in the Prometheus text format, so the
// node can scrape them with `metrics = {}`. A few dozen lines instead of the
// Prometheus client library keep the image's build offline.
type metrics struct {
	mu        sync.Mutex
	requests  map[[2]string]uint64 // (code, path) -> count
	latencies map[string]*histogram
}

type histogram struct {
	counts []uint64 // one per bucket, cumulative when written out
	count  uint64
	sum    float64
}

func newMetrics() *metrics {
	return &metrics{requests: map[[2]string]uint64{}, latencies: map[string]*histogram{}}
}

type statusRecorder struct {
	http.ResponseWriter
	code int
}

func (s *statusRecorder) WriteHeader(code int) {
	s.code = code
	s.ResponseWriter.WriteHeader(code)
}

// wrap counts every request the mux serves except health checks and scrapes,
// labelled by the route that matched, so the label set stays small.
func (m *metrics) wrap(mux http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		started := time.Now()
		recorder := &statusRecorder{ResponseWriter: w, code: http.StatusOK}
		mux.ServeHTTP(recorder, r)
		// ServeMux records the pattern it matched on the request.
		path := routeLabel(r.Pattern)
		if path == "/healthz" || path == "/metrics" {
			return
		}
		m.observe(fmt.Sprint(recorder.code), path, time.Since(started))
	})
}

func routeLabel(pattern string) string {
	if pattern == "" {
		return "other"
	}
	if _, path, ok := strings.Cut(pattern, " "); ok {
		pattern = path
	}
	return strings.TrimSuffix(pattern, "{$}")
}

func (m *metrics) observe(code, path string, took time.Duration) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.requests[[2]string{code, path}]++
	h, ok := m.latencies[path]
	if !ok {
		h = &histogram{counts: make([]uint64, len(buckets))}
		m.latencies[path] = h
	}
	seconds := took.Seconds()
	for i, bound := range buckets {
		if seconds <= bound {
			h.counts[i]++
		}
	}
	h.count++
	h.sum += seconds
}

func (m *metrics) serve(w http.ResponseWriter, _ *http.Request) {
	m.mu.Lock()
	defer m.mu.Unlock()
	w.Header().Set("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
	fmt.Fprintln(w, "# HELP http_requests_total Requests served, by status code and route.")
	fmt.Fprintln(w, "# TYPE http_requests_total counter")
	keys := make([][2]string, 0, len(m.requests))
	for key := range m.requests {
		keys = append(keys, key)
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i][1]+keys[i][0] < keys[j][1]+keys[j][0] })
	for _, key := range keys {
		fmt.Fprintf(w, "http_requests_total{code=%q,path=%q} %d\n", key[0], key[1], m.requests[key])
	}
	fmt.Fprintln(w, "# HELP http_request_duration_seconds How long requests took, by route.")
	fmt.Fprintln(w, "# TYPE http_request_duration_seconds histogram")
	paths := make([]string, 0, len(m.latencies))
	for path := range m.latencies {
		paths = append(paths, path)
	}
	sort.Strings(paths)
	for _, path := range paths {
		h := m.latencies[path]
		for i, bound := range buckets {
			fmt.Fprintf(w, "http_request_duration_seconds_bucket{path=%q,le=\"%g\"} %d\n", path, bound, h.counts[i])
		}
		fmt.Fprintf(w, "http_request_duration_seconds_bucket{path=%q,le=\"+Inf\"} %d\n", path, h.count)
		fmt.Fprintf(w, "http_request_duration_seconds_sum{path=%q} %g\n", path, h.sum)
		fmt.Fprintf(w, "http_request_duration_seconds_count{path=%q} %d\n", path, h.count)
	}
}
