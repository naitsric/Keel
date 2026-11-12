package keelsample

import (
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestADependencySetupDownloadedIsThere(t *testing.T) {
	if diff := cmp.Diff("124", Digits(124)); diff != "" {
		t.Fatal(diff)
	}
}
