# 02 — Trasporto: i flag comuni a tutti i moduli

Tutto quello che sta in questa pagina vale identico per `postgres`, `mongodb`,
`filesystem` e `s3`, sia sul lato `source` sia sul lato `destination`. I
parametri specifici del backend sono nelle pagine dei singoli moduli.

## Caso minimal

Il minimo indispensabile del trasporto sono **due** flag, uguali sui due lati:

```bash
rust-backup <modulo> source      --to 127.0.0.1:7835 --channel demo  [PARAMETRI MODULO]
rust-backup <modulo> destination --to 127.0.0.1:7835 --channel demo  [PARAMETRI MODULO] --yes
```

Se il server è stato avviato con un segreto, diventano tre:
`--secret-file /run/secrets/rb`.

## Tutti i flag di trasporto

| Flag | Variabile | Default | A cosa serve |
|------|-----------|---------|--------------|
| `--to <host:porta>` | `RUST_BACKUP_TO` | — (obbligatorio) | indirizzo del server di coordinamento. Un prefisso `https://` attiva il TLS lato client, `http://` lo esclude esplicitamente. Indicare sempre la porta |
| `--channel <id>` | `RUST_BACKUP_CHANNEL` | — (obbligatorio) | identificativo del rendezvous. Sorgente e destinazione si incontrano solo se è identico. Un canale = un trasferimento alla volta |
| `--secret <stringa>` | `RUST_BACKUP_SECRET` | nessuno | segreto condiviso HMAC; deve coincidere con quello del server. Da preferire in variabile d'ambiente o file |
| `--secret-file <file>` | `RUST_BACKUP_SECRET_FILE` | nessuno | stesso segreto letto da file (newline finale ignorato, file vuoto = errore). Esclusivo rispetto a `--secret` |
| `--carriers <n>` | `RUST_BACKUP_CARRIERS` | `1` | numero di flussi dati paralleli **richiesti**, intervallo `1..=32`. Il valore effettivo è negoziato (vedi sotto) |
| `--udp[=true\|false]` | `RUST_BACKUP_UDP` | attivo | tenta il percorso diretto UDP/QUIC; in caso di fallimento si usa il relay senza interrompere nulla |
| `--no-udp` | — | — | negazione esplicita di `--udp`: forza il solo relay. **Vince su `--udp` e sulla variabile d'ambiente** |
| `--insecure` | `RUST_BACKUP_INSECURE` | disattivo | salta la verifica del certificato TLS. Solo per test isolati: annulla la protezione contro un coordinatore ostile |
| `--max-rate <byte/s>` | `RUST_BACKUP_MAX_RATE` | illimitato | tetto aggregato al payload prodotto dalla sorgente. `0` o assente = nessun limite. Si imposta sul lato sorgente, dove i dati vengono generati |
| `--yes` *(solo destinazione)* | `RUST_BACKUP_YES` | disattivo | accetta automaticamente il piano, senza prompt interattivo. Indispensabile in cron, systemd e CI |
| `--config <file.yml>` | `RUST_BACKUP_CONFIG` | — | file YAML usato come **strato sottostante**: i valori non passati da CLI/ambiente vengono presi da lì |
| `-P`, `--param chiave=valore` | — | — | parametro di modulo senza flag dedicato; ripetibile; vince sui flag tipizzati |
| `-v`, `-vv` | — | `info` | verbosità dei log |
| `-h`, `--help` | — | — | elenco dei flag della specifica combinazione modulo/ruolo |

Errori tipici di configurazione (exit code `2`):

```text
--to is required (or set it in --config)
--channel is required (or set it in --config)
--secret and --secret-file are mutually exclusive
carriers=64 is outside the supported range 1..=32
```

## Carriers: richiesti ≠ negoziati

`--carriers` chiede *n* flussi dati paralleli. Il valore effettivo è il minimo
fra ciò che chiede l'operatore, ciò che il modulo sa ricostruire e ciò che
accetta la destinazione:

| Modulo | Massimo supportato | Perché |
|--------|--------------------|--------|
| `filesystem` | 32 | file indipendenti, ricostruibili in qualunque ordine |
| `postgres` | 1 | un `COPY` è legato a una connessione |
| `mongodb` | 1 | il restore usa un accumulatore di batch ordinato |
| `s3` | 1 | il multipart segue l'ordine stretto del piano |

Chiedere `--carriers 8` su PostgreSQL non è un errore: il valore viene ridotto e
la riduzione viene registrata nel log. Il numero realmente in uso compare su
entrambi i peer:

```text
module supports fewer data carriers than requested  requested=8 carriers=1
negotiated data plane carriers=4
```

Un elemento viaggia **sempre su un solo carrier** (niente striping interno): è la
regola che rende impossibile il riordino dei chunk.

## Percorso diretto UDP/QUIC e fallback

1. I due peer si registrano sul canale e aprono subito il relay TCP: il canale è
   utilizzabile da subito.
2. In parallelo tentano il hole-punching UDP (scoperta dell'indirizzo pubblico
   via STUN, vedi `RUST_BACKUP_STUN_SERVERS`) e, se riesce, promuovono i carrier
   a QUIC diretto.
3. Se il percorso diretto non si stabilisce o cade, quel carrier torna al relay
   già caldo. Il trasferimento non si interrompe e UDP non decide mai se il
   canale è vivo.

Per forzare il solo relay (NAT esotici, policy di rete, test riproducibili):
`--no-udp` su entrambi i lati, oppure `--udp=false` sul server.

## TLS lato client

Non c'è un flag: decide lo schema di `--to`.

```bash
# in chiaro
--to coordinatore.example:7835
# TLS verificato sulle CA di sistema (il nome deve combaciare col certificato)
--to https://coordinatore.example:7835
# TLS senza verifica del certificato — SOLO test
--to https://10.0.0.7:7835 --insecure
```

## Limitare la banda

```bash
# 20 MB/s aggregati di payload
rust-backup filesystem source --to coord:7835 --channel fs --root /srv/data --max-rate 20000000
```

Il limite agisce sul payload applicativo, non sull'overhead di trasporto, ed è
aggregato su tutti i carrier. Poiché la catena è a controspinta
(*backpressure*), rallentare la sorgente rallenta anche la lettura dal backend:
è il modo corretto di non saturare né la rete né il disco di origine.

## Conferma del piano (`--yes`)

Senza `--yes`, la destinazione stampa il piano ricevuto e l'esito del preflight e
attende `yes` su stdin. Rispondere qualunque altra cosa (o chiudere lo stdin)
significa rifiutare: exit code `4`, nessuna modifica applicata.

```text
Backup plan  module=filesystem  mode=Copy1to1  created=2026-09-15T19:00:51Z
  filesystem tree /srv/data (3 entries)
  items: 2   estimated: 97.66 KiB   integrity: blake3
Proceed with restore? [yes/no]
```

In esecuzioni non interattive serve `--yes` (o `auto_accept: true` nello YAML),
altrimenti il processo resta in attesa fino al timeout.

## Booleani a tre stati

`--overwrite`, `--admin`, `--path-style`, `--follow-symlinks`,
`--no-preserve-ownership` e `--preserve-xattr` hanno tre stati: **non passato**,
**vero**, **falso**.

```bash
--overwrite            # vero
--overwrite=false      # falso esplicito: annulla un overwrite: true nello YAML
# --overwrite false    # ERRORE: il valore va attaccato con '=' 
```

Il valore separato da spazio è rifiutato di proposito, perché si "mangerebbe"
l'argomento successivo. Un booleano entra nella configurazione solo se è stato
effettivamente passato: per questo un `false` esplicito riesce a sovrascrivere un
`true` che arriva dallo YAML.

## `-P chiave=valore`

Serve per i parametri di modulo che non hanno un flag dedicato
(`sslrootcert`, `create_bucket`, `allow_unsupported_objects`,
`allow_skipped_namespaces`, `preserve_ownership`, …) ed è ripetibile.

Il tipo del valore si deduce dalla forma:

| Scritto così | Interpretato come |
|--------------|-------------------|
| `-P create_bucket=true` | booleano |
| `-P port=6000` | numero |
| `-P prefix=archivio/` | stringa |
| `-P 'password="123456"'` | stringa (le virgolette forzano il tipo) |
| `-P 'database="007"'` | stringa `007`, non il numero 7 |

Un `-P` vince sul flag tipizzato corrispondente e sullo YAML.

## Timeout

| Variabile | Default | Significato |
|-----------|---------|-------------|
| `RUST_BACKUP_PLAN_TIMEOUT` | `600` s | attesa dello scambio del piano con il peer. Se la controparte non si presenta entro questo tempo, il comando fallisce |
| `RUST_BACKUP_VERIFY_TIMEOUT` | `86400` s | attesa, lato sorgente, della verifica per rilettura fatta dalla destinazione. È separato dal tempo di trasferimento perché la rilettura è una seconda lettura completa del backend |

Su dataset molto grandi il valore da alzare è quasi sempre il secondo.

## Cosa cercare nei log

```text
negotiated data plane carriers=1            carrier effettivi concordati
destination read-back verification started  la destinazione sta rileggendo ciò che ha scritto
BACKUP VERIFIED: source unchanged; destination read-back matches
RESTORE VERIFIED: persisted destination matches source
items 2/2  97.66 KiB/97.66 KiB  (100.0%)  status="verified"
```

Le due righe `VERIFIED` riportano lo **stesso** BLAKE3 del payload completo: se i
digest differiscono o una delle due righe manca, il trasferimento non è
certificato, qualunque cosa dica il resto dell'output.
