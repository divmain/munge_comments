//go:build !wasm

// Package server exposes the billing API over HTTP.
package server

import (
	"encoding/json"
	"net/http"
	"time"
)

// Invoice is an invoice.
type Invoice struct {
	// The ID.
	ID string `json:"id"`
	// Cents rather than a float: the ledger service rejects amounts that
	// do not round-trip exactly through its decimal type.
	TotalCents int64 `json:"totalCents"`
}

// requestTimeout matches the load balancer's idle timeout minus a margin,
// so we always answer before the balancer cuts the connection and the
// client retries a request that may already have been billed.
const requestTimeout = 25 * time.Second

// HandleInvoice handles the invoice request.
func HandleInvoice(w http.ResponseWriter, r *http.Request) {
	// Set the content type.
	w.Header().Set("Content-Type", "application/json")
	inv := Invoice{ID: r.URL.Query().Get("id")}
	// encode it, ignore err since headers r already sent anyway
	_ = json.NewEncoder(w).Encode(inv) //nolint:errcheck
}
