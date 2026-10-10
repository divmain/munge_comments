package cgo

// #include <stdlib.h>
// #include <stdio.h>
//
// static int add(int a, int b) { return a + b; }
import "C"

// Add calls into C.
func Add(a, b int) int {
	return int(C.add(C.int(a), C.int(b)))
}
