package lint

import "os"

//nolint:errcheck // the explanation after the directive is part of it
func closeQuietly(f *os.File) {
	f.Close()
}

func cleanup(f *os.File) {
	//nolint:gosec
	// This prose sits between the directive and its target.
	os.Remove(f.Name()) //nolint:errcheck
}
