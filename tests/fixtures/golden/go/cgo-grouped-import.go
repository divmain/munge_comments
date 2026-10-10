package cgo

import (
	"fmt"

	/*
	#include <math.h>
	*/
	"C"
	// A comment above a non-C spec is ordinary prose.
	"os"
)

// Sqrt prints a square root computed in C.
func Sqrt(x float64) {
	fmt.Fprintln(os.Stdout, float64(C.sqrt(C.double(x))))
}
