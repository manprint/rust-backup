# Transport contract

The coordination server accepts TCP control connections and multiplexes them with
yamux. Provider and consumer rendezvous on a channel id; the payload stream then
carries `Plan`, acknowledgement, ordered data frames, `StreamEnd`, `Done`, and a
final `VerificationAck`/`CompleteAckAck`/`VerificationComplete` handshake. After
validating the received stream, the destination reopens its persisted backend,
verifies the restorable catalog/metadata, rereads every data item, and reproduces
the source BLAKE3 commitments. Only then does it send the evidence-bearing
`VerificationAck`.
The source verifies its own full post-run fingerprint and sends
`CompleteAckAck`; the destination answers `VerificationComplete`. The source
then closes and the destination verifies that close. This explicit four-step
exchange works consistently across relay and direct QUIC streams and prevents a
peer from treating an unobserved semantic acknowledgement as success. The old
evidence-free `CompleteAck` can be decoded for a precise compatibility error but
is rejected as unsuccessful.

## Control-plane bounds

Every control frame is null-delimited JSON bounded by `MAX_FRAME_LENGTH`
(8 KiB, bore parity). The bound is enforced while *reading*, so a peer that
streams bytes without ever sending the delimiter is refused instead of growing
the server's buffer without limit; it also covers the worst frame the server
itself emits, a full IPv6 candidate set forwarded as `UdpPunch`.

Frames a peer owes immediately have a deadline: the first `Register`/`Connect`,
the authentication challenge and its answer all use a bounded read
(`HANDSHAKE_TIMEOUT`). Long-lived loops — the shared control loop and the client
keepalive — deliberately stay untimed and rely on the heartbeat plus the
recv-deadline reaper instead. A relayed substream owes its readiness byte within
`STREAM_READY_TIMEOUT`; that read holds a `--max-conns` permit, so it must never
block indefinitely.

A channel pairs exactly **one** source with **one** destination. The channel id
is claimed atomically, so two sources that register at the same moment cannot
both believe they own it and the loser can never evict the winner. A second
destination on a live channel is refused with an explicit reason rather than
having its relayed substreams interleaved with the first one's on the same
source. Both slots are released as soon as the owning control connection ends,
so a retry takes the channel over normally.

The client's control TCP socket is tuned exactly like the server's accepted
socket (`tune_tcp`: nodelay plus keepalive) because that single connection
carries the whole multiplexed relay data plane. The TLS handshake on it is
bounded by the same network deadline as the TCP connect.

`https://host:port` selects TLS for the control connection. The server enables it
only when both `--tls-cert` and `--tls-key` are provided. Normal certificate
validation is the default; `--insecure` is test-only. The optional shared secret
authenticates peers after the first control message and is independent of TLS.

With UDP enabled the peers exchange sanitized candidates and attempt direct QUIC;
any failure during direct setup falls back to the TCP relay. Every direct bidi
stream starts with an explicit readiness byte; QUIC does not expose a stream to
the acceptor until the opener writes, so omitting it deadlocks plan exchange.
Heartbeats reap dead rendezvous state.
`--no-udp` forces the relay, which is the CI baseline.

Current UDP support includes STUN reflexive candidates, authenticated
connectivity checks, a bounded learned-address cache and authenticated sibling
QUIC carriers. Pure NAT-plan classification with symmetric-port prediction,
PCP/UPnP mapping and the full privileged NAT outcome matrix remain future work;
direct-path failure during setup must never break the relay path.

Loss of an already active QUIC byte stream fails the current transfer closed; it
does not migrate a delivered prefix to relay. Transparent migration would need
sequence acknowledgements and replay, and would contradict v0.1's rule against
resuming a half-applied restore. The source is still audited for immutability and
the destination removes its active partial item.
# Carrier contract

The source requests a carrier count in the plan exchange; the destination replies
with the safe count it will open. Both peers use the negotiated minimum before
opening any data stream, and each open/accept is bounded by the plan-exchange
timeout. Filesystem permits up to 32 carriers. PostgreSQL, MongoDB and S3
currently negotiate to one because their restore sinks are respectively a COPY
connection, a batch accumulator and an ordered multipart loop.

Current peers also negotiate `separate_data_streams=true`: the plan, abort and
completion exchange stays on a dedicated control stream even when the negotiated
carrier count is one. This avoids concurrently polling split halves of one yamux
stream, which can stall a payload writer. The field defaults to false when absent,
so either side can still interoperate with the legacy multiplexed layout.

For a negotiated transfer, `item_id % carriers` selects exactly one carrier.
Every data carrier identifies itself with a setup frame because relay accept
order is not identity. The destination pulls items only in plan order from their
assigned carrier; it never exposes interleaved items to a module, and it never
stripes one item. A control-plane abort interrupts rate-limit waits and blocked
chunk writes, so a destination failure stops source work without waiting for an
item boundary. Every spawned control watcher is joined or explicitly cancelled.

## Coordination reconnect decision

There is no transparent control-plane reconnect in v0.1. A coordination loss
before the plan exchange fails in `Connect`; a loss after exchange aborts the
current transfer with its phase-tagged error. The data plane never resumes a
half-applied restore. The unused reconnect scaffold was removed rather than
leaving an implied, untested recovery guarantee.
