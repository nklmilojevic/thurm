package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestFileBuildIsTheHexSHA256(t *testing.T) {
	path := filepath.Join(t.TempDir(), "helper")
	if err := os.WriteFile(path, []byte("abc"), 0o600); err != nil {
		t.Fatal(err)
	}
	got, err := fileBuild(path)
	if err != nil {
		t.Fatal(err)
	}
	// sha256("abc"), which thurmd's side of the comparison is tested against too.
	const want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
	if got != want {
		t.Fatalf("fileBuild = %s, want %s", got, want)
	}
}

func TestSelfBuildFingerprintsTheRunningExecutable(t *testing.T) {
	exe, err := os.Executable()
	if err != nil {
		t.Skip(err)
	}
	want, err := fileBuild(exe)
	if err != nil {
		t.Fatal(err)
	}
	if got := selfBuild(); got != want || got == "" {
		t.Fatalf("selfBuild = %q, want %q", got, want)
	}
}
