# Transport contract

The coordination server accepts TCP control connections and multiplexes them with
yamux. Provider and consumer rendezvous on a channel id; the payload stream then
carries `Plan`, acknowledgement, ordered data frames, `StreamEnd`, and `Done`.
The destination verifies every expected data-bearing item and the final byte and
completion digest before declaring success.

`https://host:port` selects TLS for the control connection. The server enables it
only when both `--tls-cert` and `--tls-key` are provided. Normal certificate
validation is the default; `--insecure` is test-only. The optional shared secret
authenticates peers after the first control message and is independent of TLS.

With UDP enabled the peers exchange sanitized candidates and attempt direct QUIC;
any failure falls back to the TCP relay. Heartbeats reap dead rendezvous state.
`--no-udp` forces the relay, which is the CI baseline.

Current limits: one data carrier is implemented. Advanced NAT classification,
connectivity checks, PCP/UPnP mapping, address cache, symmetric-NAT prediction,
and multi-carrier item pinning remain future work; direct-path failure must never
break the relay path.
# Carrier contract

The source requests a carrier count in the plan exchange; the destination replies
with the safe count it will open. Both peers use the negotiated minimum before
opening any data stream, and each open/accept is bounded by the plan-exchange
timeout. Filesystem permits up to 32 carriers. PostgreSQL, MongoDB and S3
currently negotiate to one because their restore sinks are respectively a COPY
connection, a batch accumulator and an ordered multipart loop.

For a negotiated multi-carrier transfer, `item_id % carriers` selects exactly one
carrier. Every carrier identifies itself with a setup frame because relay accept
order is not identity. The destination pulls items only in plan order from their
assigned carrier; it never exposes interleaved items to a module, and it never
stripes one item. A control-plane abort is watched between item boundaries, so a
destination failure stops source reads in bounded work.

## Coordination reconnect decision

There is no transparent control-plane reconnect in v0.1. A coordination loss
before the plan exchange fails in `Connect`; a loss after exchange aborts the
current transfer with its phase-tagged error. The data plane never resumes a
half-applied restore. The unused reconnect scaffold was removed rather than
leaving an implied, untested recovery guarantee.
