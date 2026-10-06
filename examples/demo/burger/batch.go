package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"strconv"
)

// batch performs real CPU and memory work without a network or mutable image.
// Its result is deterministic for (profile, index), making retries inspectable.
func batch(args []string, output io.Writer) error {
	if len(args) != 2 {
		return fmt.Errorf("usage: burger batch small|large INDEX")
	}
	index, err := strconv.ParseUint(args[1], 10, 32)
	if err != nil {
		return fmt.Errorf("invalid task index: %w", err)
	}
	size, rounds := 8<<20, 2
	switch args[0] {
	case "small":
	case "large":
		size, rounds = 32<<20, 8
	default:
		return fmt.Errorf("unknown resource profile %q", args[0])
	}
	memory := make([]byte, size)
	for at := range memory {
		memory[at] = byte(uint64(at)*31 + index)
	}
	digest := sha256.Sum256(memory)
	for round := 1; round < rounds; round++ {
		copy(memory, digest[:])
		digest = sha256.Sum256(memory)
	}
	return json.NewEncoder(output).Encode(struct {
		Profile string `json:"profile"`
		Index   uint64 `json:"index"`
		Digest  string `json:"digest"`
	}{args[0], index, hex.EncodeToString(digest[:])})
}
