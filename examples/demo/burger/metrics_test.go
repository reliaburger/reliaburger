package main

import (
	"net/http"
	"strings"
	"testing"
)

func TestRequestsAreCountedAndTimedForPrometheus(t *testing.T) {
	m := newMetrics()
	mux := http.NewServeMux()
	mux.HandleFunc("GET /{$}", func(w http.ResponseWriter, _ *http.Request) {})
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {})
	handler := m.wrap(mux)
	for _, path := range []string{"/", "/", "/healthz", "/nowhere"} {
		get(t, handler, path, "")
	}
	response := get(t, http.HandlerFunc(m.serve), "/metrics", "")
	if got := response.Header().Get("Content-Type"); !strings.HasPrefix(got, "text/plain; version=0.0.4") {
		t.Errorf("Content-Type %q", got)
	}
	text := response.Body.String()
	for _, want := range []string{
		"# TYPE http_requests_total counter",
		`http_requests_total{code="200",path="/"} 2`,
		`http_requests_total{code="404",path="other"} 1`,
		"# TYPE http_request_duration_seconds histogram",
		`http_request_duration_seconds_bucket{path="/",le="+Inf"} 2`,
		`http_request_duration_seconds_count{path="/"} 2`,
		`http_request_duration_seconds_sum{path="/"} `,
	} {
		if !strings.Contains(text, want) {
			t.Errorf("no %q in\n%s", want, text)
		}
	}
	if strings.Contains(text, "healthz") {
		t.Errorf("health checks are counted:\n%s", text)
	}
}
