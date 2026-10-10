package count

func Count(xs []int) int {
	// start from zero
	n := 0
	for range xs {
		n++ // one more
	}
	return n
}
