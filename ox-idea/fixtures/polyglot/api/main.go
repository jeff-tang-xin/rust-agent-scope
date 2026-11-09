package main

import (
	"fmt"
	"log"
	"net/http"
)

type Health struct {
	Status  string `json:"status"`
	Version string `json:"version"`
}

func greeting(name string) string {
	if name == "" {
		name = "world"
	}
	return fmt.Sprintf("Hello, %s!", name)
}

func main() {
	http.HandleFunc("/hello", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintln(w, greeting(r.URL.Query().Get("name")))
	})
	// FIXME: health check should report database connectivity
	http.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, `{"status":"ok"}`)
	})
	log.Fatal(http.ListenAndServe(":8080", nil))
}
