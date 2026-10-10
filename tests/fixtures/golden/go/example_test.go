package greet_test

import "fmt"

// ExampleHello shows the greeting.
func ExampleHello() {
	// Print the greeting.
	fmt.Println("hello")
	// Output: hello
}

// ExampleUnordered shows unordered output.
func ExampleUnordered() {
	fmt.Println("b")
	fmt.Println("a")
	// Unordered output:
	// a
	// b
}

// Examplefoo is not an example (lowercase after the prefix).
func Examplefoo() {
	// Output: not checked
}
