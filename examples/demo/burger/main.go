// Command burger is the five-minute tour's home-built app: a tiny order
// counter that asks podinfo's backend, by its service name, which kitchen
// cooked each burger. Standard library only, so building it downloads
// nothing but the Go toolchain image.
//
//	GET /order    a burger order, cooked by whichever backend replica answered
//	GET /healthz  ok, for the health check
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"sync/atomic"
	"syscall"
	"time"
)

var menu = []string{"cheeseburger", "double smash", "mushroom swiss", "veggie deluxe"}

// Order is what GET /order returns.
type Order struct {
	Number  uint64 `json:"number"`
	Burger  string `json:"burger"`
	Cashier string `json:"cashier"`
	Kitchen string `json:"kitchen"`
}

type shop struct {
	backendURL string
	cashier    string
	client     *http.Client
	orders     atomic.Uint64
}

func newShop(backendURL, cashier string) *shop {
	return &shop{
		backendURL: backendURL,
		cashier:    cashier,
		client:     &http.Client{Timeout: 2 * time.Second},
	}
}

func (s *shop) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "ok")
	})
	mux.HandleFunc("GET /order", s.order)
	return mux
}

func (s *shop) order(w http.ResponseWriter, r *http.Request) {
	number := s.orders.Add(1)
	kitchen, err := s.kitchen(r.Context())
	if err != nil {
		http.Error(w, "the kitchen didn't answer: "+err.Error(), http.StatusBadGateway)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(Order{
		Number:  number,
		Burger:  menu[(number-1)%uint64(len(menu))],
		Cashier: s.cashier,
		Kitchen: kitchen,
	})
}

// kitchen asks podinfo's backend who it is. podinfo answers GET / with its
// runtime details, hostname included, when asked for JSON.
func (s *shop) kitchen(ctx context.Context) (string, error) {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, s.backendURL, nil)
	if err != nil {
		return "", err
	}
	request.Header.Set("Accept", "application/json")
	response, err := s.client.Do(request)
	if err != nil {
		return "", err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return "", fmt.Errorf("%s answered %s", s.backendURL, response.Status)
	}
	var info struct {
		Hostname string `json:"hostname"`
	}
	if err := json.NewDecoder(response.Body).Decode(&info); err != nil {
		return "", err
	}
	if info.Hostname == "" {
		return "", errors.New("the backend didn't say who it is")
	}
	return info.Hostname, nil
}

func env(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

func main() {
	cashier, err := os.Hostname()
	if err != nil {
		cashier = "unknown"
	}
	s := newShop(env("BACKEND_URL", "http://backend:9898/"), cashier)
	server := &http.Server{
		Addr:              ":" + env("PORT", "8080"),
		Handler:           s.routes(),
		ReadHeaderTimeout: 5 * time.Second,
	}

	// Finish in-flight orders when the orchestrator stops this instance.
	stop, cancel := signal.NotifyContext(context.Background(), syscall.SIGTERM, os.Interrupt)
	defer cancel()
	go func() {
		<-stop.Done()
		shutdown, done := context.WithTimeout(context.Background(), 10*time.Second)
		defer done()
		server.Shutdown(shutdown)
	}()

	log.Printf("burger listening on %s, kitchen at %s", server.Addr, s.backendURL)
	if err := server.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}
