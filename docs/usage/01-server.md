# 01 — Server di coordinamento

Il server è il punto d'incontro: sorgente e destinazione si registrano sullo
stesso `--channel`, il server li accoppia e poi fa da **relay** per i dati. Se
il percorso diretto UDP/QUIC è abilitato, il server fa anche da broker per il
hole-punching; il relay resta comunque attivo come fallback.

Il server **non** vede mai i dati in chiaro dei backend in senso applicativo:
inoltra byte. Non conserva nulla su disco e non ha stato persistente.

## Caso minimal

```bash
rust-backup server
```

Ascolta su `0.0.0.0:7835/tcp` (controllo + relay) e `0.0.0.0:7835/udp`
(negoziazione del percorso diretto), senza segreto condiviso e senza TLS.
Accettabile solo su rete fidata o in test: **chiunque raggiunga la porta può
occupare un canale**.

## Caso tipico in produzione

```bash
sudo install -d -m 0750 -o rust-backup -g rust-backup /etc/rust-backup
openssl rand -base64 48 | sudo tee /etc/rust-backup/coordination.secret >/dev/null
sudo chmod 600 /etc/rust-backup/coordination.secret

rust-backup server \
  --bind-addr 0.0.0.0 \
  --control-port 7835 \
  --secret-file /etc/rust-backup/coordination.secret \
  --max-conns 256 \
  --udp=true
```

## Tutti i flag di `rust-backup server`

| Flag | Variabile | Default | A cosa serve |
|------|-----------|---------|--------------|
| `--bind-addr <IP>` | `RUST_BACKUP_BIND_ADDR` | `0.0.0.0` | indirizzo su cui mettersi in ascolto. `127.0.0.1` limita ai client locali, un IP specifico lega il servizio a una sola interfaccia |
| `--control-port <PORTA>` | `RUST_BACKUP_CONTROL_PORT` | `7835` | porta TCP di controllo e relay. È **anche** la porta UDP usata per il percorso diretto quando `--udp` è attivo |
| `--secret <STRINGA>` | `RUST_BACKUP_SECRET` | nessuno | segreto condiviso per l'autenticazione HMAC dei client. Sconsigliato come flag: finisce in `/proc/<pid>/cmdline` |
| `--secret-file <FILE>` | `RUST_BACKUP_SECRET_FILE` | nessuno | stesso segreto, letto da file (il newline finale viene tolto, un file vuoto è un errore). **Forma preferita** |
| `--tls-cert <FILE.pem>` | `RUST_BACKUP_TLS_CERT` | nessuno → TLS disattivo | catena di certificati PEM: la sua presenza abilita TLS sulla porta di controllo |
| `--tls-key <FILE.pem>` | `RUST_BACKUP_TLS_KEY` | nessuno | chiave privata PEM corrispondente. Obbligatoria insieme a `--tls-cert` |
| `--max-conns <N>` | `RUST_BACKUP_MAX_CONNS` | `256` | limite reale applicato due volte: al massimo N connessioni client accettate contemporaneamente **e** al massimo N substream inoltrati contemporaneamente. Un accoppiamento consuma due connessioni (sorgente + destinazione), quindi permette `N/2` trasferimenti in parallelo. Le connessioni in eccesso restano nel backlog del kernel, non vengono chiuse |
| `--udp[=true\|false]` | `RUST_BACKUP_UDP` | `true` | abilita il brokering del percorso diretto UDP/QUIC. `--udp=false` forza il solo relay TCP: utile in ambienti dove UDP è filtrato o in test deterministici |
| `-v`, `-vv` | — | `info` | alza la verbosità dei log (`debug`, `trace`) |
| `-h`, `--help` | — | — | stampa l'elenco dei flag |
| `--version` | — | — | disponibile sul comando di primo livello (`rust-backup --version`): stampa la versione del binario |

`--secret` e `--secret-file` sono **mutuamente esclusivi**: passarli insieme fa
fallire l'avvio con `--secret and --secret-file are mutually exclusive`
(exit code `2`).

## Porte da aprire

| Protocollo | Porta | Quando serve |
|------------|-------|--------------|
| TCP | `--control-port` (7835) | **sempre**: registrazione, accoppiamento e relay dei dati |
| UDP | stessa porta | solo con `--udp=true`, per la negoziazione e il traffico QUIC diretto |

Se UDP è bloccato dal firewall non succede niente di grave: i client tentano il
percorso diretto, falliscono e proseguono sul relay. Il canale non muore mai per
colpa di UDP. Per evitare del tutto il tentativo, usare `--udp=false` sul
server o `--no-udp` sui client.

## TLS

```bash
rust-backup server \
  --control-port 7835 \
  --secret-file /etc/rust-backup/coordination.secret \
  --tls-cert /etc/rust-backup/tls/tls.crt \
  --tls-key  /etc/rust-backup/tls/tls.key
```

Lato client **non esiste un flag per attivare TLS**: si sceglie con lo schema di
`--to`.

- `--to coordinatore.example:7835` → TCP in chiaro;
- `--to https://coordinatore.example:7835` → handshake TLS con verifica del
  certificato sulle CA di sistema (il nome in `--to` deve corrispondere al
  certificato);
- `--to http://coordinatore.example:7835` → esplicitamente in chiaro.

Indicare **sempre la porta**: uno schema senza porta cade sul default 443
(`https://`) o 80 (`http://`). `--insecure` lato client salta la verifica del
certificato ed è ammesso solo in test isolati.

Un reverse proxy può terminare il TLS del TCP, ma la porta UDP deve arrivare
direttamente al coordinatore se il percorso QUIC è abilitato.

## Liveness: heartbeat e reaper

Un peer che sparisce senza chiudere (cavo staccato, VM uccisa) lascerebbe il
canale occupato per sempre, perché il substream non se ne accorge. Per questo il
provider invia un `Heartbeat` ogni **20 s** e il server rimuove dal registro la
voce il cui substream di controllo è silenzioso da **60 s**. Conseguenza
pratica: dopo un crash della sorgente, il suo `--channel` torna riutilizzabile
entro circa un minuto, non subito.

## systemd

`/etc/systemd/system/rust-backup-coordinator.service`:

```ini
[Unit]
Description=rust-backup coordination server
After=network-online.target
Wants=network-online.target

[Service]
User=rust-backup
Group=rust-backup
ExecStart=/usr/local/bin/rust-backup server --bind-addr 0.0.0.0 --control-port 7835 --secret-file /etc/rust-backup/coordination.secret --max-conns 256 --udp=true
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rust-backup-coordinator
sudo systemctl status rust-backup-coordinator
```

Le variabili d'ambiente funzionano anche qui: `Environment=RUST_BACKUP_MAX_CONNS=1024`
equivale a passare `--max-conns 1024`. La riga `ExecStart` è però visibile a
tutti: il segreto va lasciato in un file, mai scritto come `--secret`.

## Container

Il `CMD` dell'immagine è già il server, quindi basta:

```bash
docker run --rm -p 7835:7835/tcp -p 7835:7835/udp \
  ghcr.io/manprint/rust-backup:latest
```

Con segreto, in sola lettura e non-root (l'immagine gira già come UID 65532):

```bash
docker run -d --name rust-backup-coordinator \
  -p 7835:7835/tcp -p 7835:7835/udp \
  -v /etc/rust-backup/coordination.secret:/run/secrets/coordinator:ro \
  ghcr.io/manprint/rust-backup:latest \
  server --bind-addr 0.0.0.0 --control-port 7835 \
  --secret-file /run/secrets/coordinator --max-conns 256 --udp=true
```

Per `docker compose`, l'override delle variabili e il profilo TLS: vedi
[09 — Docker e Compose](09-docker.md).

## Verifica che sia vivo

```bash
# la porta di controllo risponde
nc -z 127.0.0.1 7835 && echo "coordinatore raggiungibile"

# giro completo di prova (due terminali, vedi il caso minimal dell'indice)
rust-backup filesystem source      --to 127.0.0.1:7835 --channel smoke --root /tmp/src
rust-backup filesystem destination --to 127.0.0.1:7835 --channel smoke --root /tmp/dst --yes
```

## `server:` nel file YAML

Un file di sessione può contenere una sezione `server:`, ma `rust-backup run
--config` **rifiuta** di avviarla:

```text
`run --config` does not start `server:`; start `rust-backup server` separately
```

Il servizio di coordinamento si avvia sempre come processo a sé: è un servizio
di lunga durata, non un target di sessione.

## Dimensionamento

- Un trasferimento = 2 connessioni di controllo + i substream dati concordati.
- `--max-conns` va quindi scelto come `2 × (trasferimenti contemporanei) +
  margine`. Con il default 256 si coprono ~128 trasferimenti simultanei.
- La banda del relay è quella del server; se sorgente e destinazione riescono a
  stabilire il percorso diretto QUIC, i dati **non** passano più dal server e il
  suo carico resta trascurabile.
- La dimensione del buffer di splice del relay si regola con
  `BORE_PROXY_BUFFER_SIZE` (default 256 KiB, valori accettati da 4 KiB a 16 MiB).
