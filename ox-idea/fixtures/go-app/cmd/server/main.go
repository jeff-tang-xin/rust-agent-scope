package main

import (
	"encoding/json"
	"log"
	"net/http"
	"os"
	"strconv"

	"example.com/openidea/notes/internal/store"
)

func main() {
	s := store.New()
	s.Create("Welcome", "This is the OpenIDEA Go fixture.")

	mux := http.NewServeMux()
	mux.HandleFunc("GET /notes", func(w http.ResponseWriter, r *http.Request) {
		// Set a breakpoint here (QA: debugger) and request http://localhost:8080/notes?q=openidea
		writeJSON(w, s.Search(r.URL.Query().Get("q")))
	})
	mux.HandleFunc("GET /notes/{id}", func(w http.ResponseWriter, r *http.Request) {
		id, err := strconv.Atoi(r.PathValue("id"))
		if err != nil {
			http.Error(w, "bad id", http.StatusBadRequest)
			return
		}
		n, err := s.Get(id)
		if err != nil {
			http.Error(w, err.Error(), http.StatusNotFound)
			return
		}
		writeJSON(w, n)
	})

	addr := ":8080"
	if p := os.Getenv("PORT"); p != "" {
		addr = ":" + p
	}
	log.Printf("listening on %s", addr)
	log.Fatal(http.ListenAndServe(addr, mux))
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}
