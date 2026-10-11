package server

import (
	"fmt"
	"net/http/httptest"
)

// ExampleHandleInvoice shows the JSON body the handler writes.
func ExampleHandleInvoice() {
	// Create a recorder.
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("GET", "/invoice?id=inv-7", nil)
	HandleInvoice(rec, req)
	// Print the body.
	fmt.Print(rec.Body.String())
	// Output: {"id":"inv-7","totalCents":0}
}

// The zero value must encode with totalCents present (no omitempty): the
// ledger treats a missing amount as "unknown", not as zero.
func ExampleInvoice_zero() {
	fmt.Println(Invoice{}.TotalCents)
	// Output: 0
}
