package gen

//go:generate stringer -type=Color

// Color is a palette entry.
type Color int

//go:noinline
func pick() Color { return 0 }

var _ = pick
