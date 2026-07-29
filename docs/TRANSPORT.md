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
