# Transport contract

The coordination server accepts TCP control connections and multiplexes them with
yamux. Provider and consumer rendezvous on a channel id; the payload stream then
carries `Plan`, acknowledgement, ordered data frames, `StreamEnd`, `Done`, and a
final `CompleteAck`/`CompleteAckAck` handshake. The destination verifies every
expected data-bearing item and the final byte and completion digest before
acknowledging; the source never reports success before that acknowledgement.
The source then confirms receipt, so the destination cannot close a TLS/yamux
stream while its completion acknowledgement is still buffered.

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
