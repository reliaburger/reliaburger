// Command burger is the five-minute tour's demo: a burger assembled from
// layers, each its own app, so you can watch the cluster deploy, roll back,
// scale and lose them one at a time. Standard library only, so building it
// downloads nothing but the Go toolchain image.
//
// One binary plays every part:
//
//	LAYER=cheese VERSION=v2 burger   a layer: GET /layer returns its slice
//	burger                           the counter: GET / stacks the layers it finds
//	burger diner                     orders from the counter twice a second
//	burger batch small|large INDEX   cooks a patty for a batch task
//
// Every part answers GET /healthz and serves Prometheus metrics at /metrics.
package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"
)

func env(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}

// nodeName is the node an instance runs on. Containers share the node's
// hostname, and a laptop cluster's Lima VMs prefix the node's name with
// "lima-", so drop that to match what `relish status` calls the node.
func nodeName(hostname string) string {
	return strings.TrimPrefix(hostname, "lima-")
}

// newInstanceID names this process, so a restarted or replaced instance
// shows up as a new one on the burger.
func newInstanceID() string {
	var id [2]byte
	rand.Read(id[:])
	return hex.EncodeToString(id[:])
}

// counterURL is where the diner and the batch find the counter: by its
// service name, as any client in the cluster would.
func counterURL() string { return env("COUNTER_URL", "http://counter:8080") }

func main() {
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "batch":
			cook(os.Args[2:])
			return
		case "diner":
			dine()
			return
		default:
			log.Fatalf("unknown command %q; want batch or diner, or none to serve", os.Args[1])
		}
	}
	hostname, err := os.Hostname()
	if err != nil {
		hostname = "unknown"
	}
	node, instance := nodeName(hostname), newInstanceID()
	port := env("PORT", "8080")

	var handler http.Handler
	if name := os.Getenv("LAYER"); name != "" {
		l, err := newLayer(name, env("VERSION", "v1"), node, instance)
		if err != nil {
			log.Fatal(err)
		}
		handler = l.routes()
		log.Printf("%s %s (%s) on %s #%s", name, l.version, l.look.Style, node, instance)
	} else {
		c := newCounter(node, instance, func(service string) string {
			return "http://" + service + ":" + port
		})
		handler = c.routes()
		log.Printf("counter on %s #%s, asking for %s", node, instance, strings.Join(layerOrder, ", "))
	}
	serve(":"+port, handler)
}

func serve(address string, handler http.Handler) {
	server := &http.Server{Addr: address, Handler: handler, ReadHeaderTimeout: 5 * time.Second}
	// Finish in-flight requests when the orchestrator stops this instance.
	stop, cancel := signal.NotifyContext(context.Background(), syscall.SIGTERM, os.Interrupt)
	defer cancel()
	go func() {
		<-stop.Done()
		shutdown, done := context.WithTimeout(context.Background(), 10*time.Second)
		defer done()
		server.Shutdown(shutdown)
	}()
	if err := server.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}

var shortClient = &http.Client{Timeout: 3 * time.Second}

// cook runs one batch task: the work, then a report to the counter so the
// burger shows the tally. The work's result is the task's output; a counter
// that can't be reached is logged, not a failed task.
func cook(args []string) {
	if err := batch(args, os.Stdout); err != nil {
		log.Print(err)
		os.Exit(1)
	}
	patty := env("RELIABURGER_BATCH_ID", "local") + "/" + strings.Join(args, "/")
	if err := reportCooked(counterURL(), patty); err != nil {
		log.Printf("cooked patty %s, but couldn't tell the counter: %v", patty, err)
	}
}

func reportCooked(base, patty string) error {
	body, _ := json.Marshal(map[string]string{"patty": patty})
	response, err := shortClient.Post(base+"/cooked", "application/json", bytes.NewReader(body))
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return fmt.Errorf("the counter answered %s", response.Status)
	}
	return nil
}

// dine orders a burger twice a second, forever, so the metrics and faults
// in the tour have traffic to act on. A failed order is logged and the
// diner carries on.
func dine() {
	base := counterURL()
	log.Printf("diner ordering from %s", base)
	for {
		if err := orderOnce(base); err != nil {
			log.Print(err)
		}
		time.Sleep(500 * time.Millisecond)
	}
}

func orderOnce(base string) error {
	response, err := shortClient.Get(base + "/order")
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return fmt.Errorf("the counter answered %s", response.Status)
	}
	return nil
}
