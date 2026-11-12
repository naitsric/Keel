// Package keelsample is a sample repo for Keel's tests.
package keelsample

import "strconv"

// Digits writes n in base 10.
func Digits(n int) string {
	return strconv.Itoa(n)
}
