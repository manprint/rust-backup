# 10 — Variabili d'ambiente

Ogni flag della CLI ha la sua variabile `RUST_BACKUP_<NOME_IN_MAIUSCOLO_SNAKE>`.
Le uniche eccezioni sono `-v/--verbose`, `-P/--param` e `--no-udp` (che è la
negazione esplicita di `--udp`, il quale legge `RUST_BACKUP_UDP`).

## Caso minimal

Tutta l'invocazione può stare nell'ambiente, tranne il modulo e il ruolo:

```bash
export RUST_BACKUP_TO=127.0.0.1:7835
export RUST_BACKUP_CHANNEL=demo
export RUST_BACKUP_ROOT=/srv/data

rust-backup filesystem source
```

## Precedenza

**CLI > ambiente > YAML > default interno.** Un flag passato sulla riga di
comando vince sulla variabile; la variabile vince sul file `--config`; il file
vince sul default compilato nel programma.

## Trasporto (tutti i moduli, entrambi i ruoli)

| Variabile | Flag equivalente | Default |
|-----------|------------------|---------|
| `RUST_BACKUP_TO` | `--to` | — |
| `RUST_BACKUP_CHANNEL` | `--channel` | — |
| `RUST_BACKUP_SECRET` | `--secret` | nessuno |
| `RUST_BACKUP_SECRET_FILE` | `--secret-file` | nessuno |
| `RUST_BACKUP_CARRIERS` | `--carriers` | `1` |
| `RUST_BACKUP_UDP` | `--udp` | `true` |
| `RUST_BACKUP_INSECURE` | `--insecure` | disattivo |
| `RUST_BACKUP_MAX_RATE` | `--max-rate` | illimitato |
| `RUST_BACKUP_YES` | `--yes` | disattivo |
| `RUST_BACKUP_CONFIG` | `--config` | — |

## Server di coordinamento

| Variabile | Flag equivalente | Default |
|-----------|------------------|---------|
| `RUST_BACKUP_BIND_ADDR` | `--bind-addr` | `0.0.0.0` |
| `RUST_BACKUP_CONTROL_PORT` | `--control-port` | `7835` |
| `RUST_BACKUP_SECRET` | `--secret` | nessuno |
| `RUST_BACKUP_SECRET_FILE` | `--secret-file` | nessuno |
| `RUST_BACKUP_TLS_CERT` | `--tls-cert` | nessuno |
| `RUST_BACKUP_TLS_KEY` | `--tls-key` | nessuno |
| `RUST_BACKUP_MAX_CONNS` | `--max-conns` | `256` |
| `RUST_BACKUP_UDP` | `--udp` | `true` |

## Sessioni (`run`)

| Variabile | Flag equivalente | Default |
|-----------|------------------|---------|
| `RUST_BACKUP_CONFIG` | `--config` | — |
| `RUST_BACKUP_PARALLEL_TARGETS` | `--parallel-targets` | `1` |
| `RUST_BACKUP_FAIL_FAST` | `--fail-fast` | disattivo |

## Parametri dei moduli

| Variabile | Flag equivalente | Moduli che la usano |
|-----------|------------------|---------------------|
| `RUST_BACKUP_HOST` | `--host` | postgres, mongodb |
| `RUST_BACKUP_PORT` | `--port` | postgres, mongodb |
| `RUST_BACKUP_USER` | `--user` | postgres, mongodb |
| `RUST_BACKUP_PASSWORD` | `--password` | postgres, mongodb |
| `RUST_BACKUP_DATABASE` | `--database` | postgres, mongodb |
| `RUST_BACKUP_SSLMODE` | `--sslmode` | postgres |
| `RUST_BACKUP_ADMIN` | `--admin` | postgres (destinazione) |
| `RUST_BACKUP_URI` | `--uri` | mongodb |
| `RUST_BACKUP_AUTH_DB` | `--auth-db` | mongodb |
| `RUST_BACKUP_ROOT` | `--root` | filesystem |
| `RUST_BACKUP_NO_PRESERVE_OWNERSHIP` | `--no-preserve-ownership` | filesystem (destinazione) |
| `RUST_BACKUP_ALLOW_ATIME_UPDATES` | `--allow-atime-updates` | filesystem (sorgente) |
| `RUST_BACKUP_FOLLOW_SYMLINKS` | `--follow-symlinks` | filesystem (rifiutato) |
| `RUST_BACKUP_PRESERVE_XATTR` | `--preserve-xattr` | filesystem (rifiutato) |
| `RUST_BACKUP_BUCKET` | `--bucket` | s3 |
| `RUST_BACKUP_ENDPOINT` | `--endpoint` | s3 |
| `RUST_BACKUP_REGION` | `--region` | s3 |
| `RUST_BACKUP_PREFIX` | `--prefix` | s3 |
| `RUST_BACKUP_ACCESS_KEY` | `--access-key` | s3 |
| `RUST_BACKUP_SECRET_KEY` | `--secret-key` | s3 |
| `RUST_BACKUP_PATH_STYLE` | `--path-style` | s3 |
| `RUST_BACKUP_OVERWRITE` | `--overwrite` | tutti i moduli (destinazione) |
| `RUST_BACKUP_EXTENSION_VERSION` | `--extension-version` | postgres (destinazione) |
| `RUST_BACKUP_HOT_BACKUP` | `--hot-backup` | postgres (sorgente e destinazione) |

I parametri senza flag dedicato (`sslrootcert`, `allow_unsupported_objects`,
`allow_skipped_namespaces`, `create_bucket`, `preserve_ownership`) **non** hanno
una variabile d'ambiente: si passano con `-P chiave=valore` o nello YAML.

## Come si scrivono i booleani

Le variabili booleane accettano tutte le grafie consuete, senza distinzione fra
maiuscole e minuscole:

```text
true / false     1 / 0     yes / no     y / n     on / off
```

Serve proprio perché `RUST_BACKUP_YES=1` è la forma che si scrive naturalmente
in una unit systemd o in un job di CI.

## Variabili senza flag corrispondente

| Variabile | Default | Significato |
|-----------|---------|-------------|
| `RUST_LOG` | `info` | filtro dei log in sintassi `tracing`, per esempio `rb_transport=debug,info`. Ha la precedenza su `-v` |
| `RUST_BACKUP_PLAN_TIMEOUT` | `600` (secondi) | attesa dello scambio del piano con il peer. `0` o testo non numerico ricadono sul default |
| `RUST_BACKUP_VERIFY_TIMEOUT` | `86400` (secondi) | attesa, lato sorgente, della verifica per rilettura della destinazione. Deliberatamente separata dal tempo di trasferimento: la rilettura è una seconda lettura completa |
| `RUST_BACKUP_IO_IDLE_TIMEOUT` | `600` (secondi) | tempo massimo **senza un solo byte di avanzamento** su una singola lettura o scrittura del payload. Non è il tempo di trasferimento: la finestra di controllo di flusso resta piena per tutto il tempo in cui la destinazione applica quello che ha già ricevuto (una parte multipart S3, un lotto `insert_many`, un `COPY` in attesa di un lock), ed è esattamente ciò che la contropressione deve permettere. Serve solo a far fallire un peer bloccato davvero; la liveness del canale è già provata dall'heartbeat di controllo (20 s) e dal reaper del server di coordinamento (60 s). `0` o testo non numerico ricadono sul default |
| `RUST_BACKUP_STUN_SERVERS` | `stun.l.google.com:19302,stun.cloudflare.com:3478` | catena STUN per il percorso diretto UDP, separata da virgole; vengono usate le prime quattro voci. `RUST_BACKUP_STUN_SERVER` (singolare) è ancora accettata e usata solo se la plurale è assente |
| `BORE_PROXY_BUFFER_SIZE` | `256 KiB` | dimensione del buffer di splice del relay; accetta suffissi (`512k`, `1MiB`) ed è limitata fra 4 KiB e 16 MiB. Eredità del trasporto `bore` |

Per il modulo `s3`, quando `--access-key`/`--secret-key` non sono impostate,
valgono le variabili standard della catena AWS (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_PROFILE`, ruolo dell'istanza).

`RUST_BACKUP_S3_TEST_FAIL_AFTER_PART` e
`RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA` sono agganci di test usati dagli
script e2e: non fanno parte della superficie di configurazione supportata. Il
secondo altera di proposito, sulla sola sorgente, il numero di righe scritto nel
piano, così l'e2e può dimostrare che la destinazione rifiuta davvero un
ripristino incompleto.

## Usarle bene

**systemd** — le variabili stanno in un file con permessi stretti, non nella
riga `ExecStart` (che è leggibile da tutti):

```ini
[Service]
EnvironmentFile=/etc/rust-backup/backup.env      # chmod 600
ExecStart=/usr/local/bin/rust-backup postgres source
```

```bash
# /etc/rust-backup/backup.env
RUST_BACKUP_TO=https://coordinatore.example:7835
RUST_BACKUP_CHANNEL=pg-prod
RUST_BACKUP_SECRET_FILE=/etc/rust-backup/coordination.secret
RUST_BACKUP_HOST=pg-source.internal
RUST_BACKUP_USER=backup_readonly
RUST_BACKUP_PASSWORD=…
RUST_BACKUP_DATABASE=app
RUST_BACKUP_SSLMODE=require
```

**Docker** — `-e VARIABILE=valore` o `--env-file`; il segreto del trasporto
resta un file montato in sola lettura.

**Attenzione a variabili dimenticate nella shell.** `RUST_BACKUP_DATABASE`
esportata e poi scordata cambia il comportamento del comando successivo senza
che si veda nella riga digitata: nei dubbi, `env | grep RUST_BACKUP`.
