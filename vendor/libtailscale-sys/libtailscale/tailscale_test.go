// Copyright (c) Tailscale Inc & AUTHORS
// SPDX-License-Identifier: BSD-3-Clause

package main

import (
	"io"
	"net"
	"syscall"
	"testing"
)

func socketPair(t *testing.T) [2]int {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_LOCAL, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { syscall.Close(fds[0]); syscall.Close(fds[1]) })
	return fds
}

// Keep the sent descriptor open until after accept: SCM_RIGHTS must allocate
// a different number, reproducing the scheduling that broke peer lookup.
func TestAcceptedPeers(t *testing.T) {
	queue := socketPair(t)
	l := &listener{fd: queue[1], m: make(map[int]net.IP)}
	peers := []string{"100.64.0.2", "fd7a:115c:a1e0::1234"}
	type accepted struct {
		fd   int
		peer string
	}
	var connections []accepted
	var pairs [][2]int
	// Queue both messages before receiving either to check message boundaries.
	for _, peer := range peers {
		pair := socketPair(t)
		pairs = append(pairs, pair)
		addr := &net.TCPAddr{IP: net.ParseIP(peer), Port: 54321}
		if err := l.sendConn(pair[0], addr); err != nil {
			t.Fatal(err)
		}
	}
	for i, pair := range pairs {
		fd, err := l.acceptConn(queue[0])
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { syscall.Close(fd) })
		if fd == pair[0] {
			t.Fatal("test did not duplicate the sent descriptor")
		}
		connections = append(connections, accepted{fd, peers[i]})
		// Descriptor metadata travels on the listener; the connection's
		// application bytes must arrive untouched for the TLS handshake.
		payload := []byte("TLS application bytes")
		if _, err := syscall.Write(pair[1], payload); err != nil {
			t.Fatal(err)
		}
		got := make([]byte, len(payload))
		n, err := syscall.Read(fd, got)
		if err != nil {
			t.Fatal(err)
		}
		if string(got[:n]) != string(payload) {
			t.Fatalf("payload = %q", got[:n])
		}
	}
	// A later accept must not overwrite an earlier connection's identity.
	for _, conn := range connections {
		got, err := l.remoteIP(conn.fd)
		if err != nil {
			t.Fatal(err)
		}
		if got != conn.peer {
			t.Errorf("peer = %q, want %q", got, conn.peer)
		}
	}
}

func TestAcceptedPeerWithPartialMetadata(t *testing.T) {
	queue, pair := socketPair(t), socketPair(t)
	l := &listener{fd: queue[1], m: make(map[int]net.IP)}
	peer := net.ParseIP("fd7a:115c:a1e0::5678").To16()
	if err := syscall.Sendmsg(queue[1], peer[:1], syscall.UnixRights(pair[0]), nil, 0); err != nil {
		t.Fatal(err)
	}
	if _, err := syscall.Write(queue[1], peer[1:]); err != nil {
		t.Fatal(err)
	}
	fd, err := l.acceptConn(queue[0])
	if err != nil {
		t.Fatal(err)
	}
	defer syscall.Close(fd)
	got, err := l.remoteIP(fd)
	if err != nil {
		t.Fatal(err)
	}
	if got != "fd7a:115c:a1e0::5678" {
		t.Fatalf("peer = %q", got)
	}
}

func TestAcceptClosedListener(t *testing.T) {
	queue := socketPair(t)
	l := &listener{m: make(map[int]net.IP)}
	if err := syscall.Shutdown(queue[1], syscall.SHUT_WR); err != nil {
		t.Fatal(err)
	}
	if _, err := l.acceptConn(queue[0]); err != io.EOF {
		t.Fatalf("accept error = %v, want EOF", err)
	}
}
