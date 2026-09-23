# Guida d'uso di `rust-backup`

Questa cartella è **l'unica fonte di verità** per l'utilizzo di `rust-backup`.
Ogni funzionalità ha il suo file, ogni file inizia con il **caso minimal** —
il minimo indispensabile per far girare quella modalità — e prosegue con tutte
le modalità operative, tutti i flag e il significato di ogni flag.

I documenti in `docs/modules/` descrivono la *fedeltà* di ciascun modulo (cosa
viene copiato, cosa viene rifiutato); i documenti in `docs/plans/` sono storia
di progetto. Per l'uso quotidiano vale quello che c'è qui.

## Caso minimal (giro completo in tre comandi)

Tre processi, una macchina sola, nessun segreto, nessun TLS: serve solo a
vedere il meccanismo funzionare.

```bash
# 1) server di coordinamento (terminale 1)
rust-backup server

# 2) sorgente: legge /srv/data in sola lettura e attende il destinatario (terminale 2)
rust-backup filesystem source --to 127.0.0.1:7835 --channel demo --root /srv/data

# 3) destinazione: ripristina in /srv/restore senza chiedere conferma (terminale 3)
rust-backup filesystem destination --to 127.0.0.1:7835 --channel demo --root /srv/restore --yes
```

Il trasferimento è finito bene solo se compaiono entrambe queste righe, con lo
stesso BLAKE3, e l'exit code è `0`:

```text
BACKUP VERIFIED: source unchanged; destination read-back matches   items=2 bytes=100005 blake3=64a7…
RESTORE VERIFIED: persisted destination matches source             items=2 bytes=100005 blake3=64a7…
done: items 2/2  97.66 KiB/97.66 KiB  (100.0%)  8.86 KiB/s average, 11s in total  status="verified"
```

## Indice

| Documento | Contenuto |
|-----------|-----------|
| [01 — Server di coordinamento](01-server.md) | avvio del server, TLS, `--max-conns`, systemd, container |
| [02 — Trasporto](02-trasporto.md) | flag comuni a tutti i moduli: `--to`, `--channel`, secret, carriers, UDP/QUIC, rate limit |
| [03 — PostgreSQL](03-postgres.md) | backup/restore cluster PostgreSQL 10+ |
| [04 — MongoDB](04-mongodb.md) | backup/restore MongoDB 4–8 |
| [05 — Filesystem](05-filesystem.md) | backup/restore alberi POSIX, permessi e ownership |
| [06 — S3 / MinIO](06-s3.md) | backup/restore bucket S3-compatibili |
| [07 — Sessioni YAML](07-sessioni-yaml.md) | `rust-backup run --config`, schema completo del file |
| [08 — Dry-run `plan`](08-plan.md) | analisi della sorgente senza trasferire nulla |
| [09 — Docker e Compose](09-docker.md) | uso dell'immagine per server e client |
| [10 — Variabili d'ambiente](10-variabili-ambiente.md) | mappa completa flag ⇄ variabile, e le variabili senza flag |
| [11 — Codici di uscita e diagnostica](11-codici-uscita.md) | significato degli exit code ed errori frequenti |

## Come funziona, in due paragrafi

`rust-backup` non produce un archivio. Mette in comunicazione **due processi** —
una *sorgente* (sola lettura) e una *destinazione* (scrittura) — attraverso un
**server di coordinamento** che li fa incontrare su un `--channel` e poi fa da
relay TCP; se la rete lo permette i due lati si promuovono a un percorso diretto
UDP/QUIC e il relay resta come fallback. Nessun dato tocca il disco come file
temporaneo: i chunk (≤ 1 MiB) vanno da backend a socket a backend, e la
destinazione lenta rallenta direttamente la lettura della sorgente.

La sorgente analizza il backend e produce un **piano autosufficiente**; la
destinazione lo valida (preflight), chiede conferma (o la salta con `--yes`),
applica, poi **rilegge dal backend** ciò che ha scritto e ricalcola i BLAKE3 per
oggetto e del payload completo. Solo quando la destinazione ha dimostrato la
rilettura e la sorgente ha ridimostrato la propria immutabilità l'exit code è
`0`. Qualsiasi altra cosa è un fallimento con exit code diverso da zero — mai
un "backup parziale certificato".

## I tre comandi e le due parti

```text
rust-backup server [OPZIONI]                        il servizio di rendezvous/relay
rust-backup <modulo> source      [PARAMETRI]        lato sorgente (sola lettura)
rust-backup <modulo> destination [PARAMETRI]        lato destinazione (ripristino)
rust-backup run --config sessione.yml               più target in una sessione
rust-backup plan <modulo> [PARAMETRI]               dry-run: stampa solo il piano
```

`<modulo>` ∈ `postgres` | `mongodb` | `filesystem` | `s3`.

Regole operative che valgono sempre:

- **Sorgente e destinazione devono concordare su `--to`, `--channel` e secret.**
  Canali diversi non si incontrano; secret diversi falliscono l'autenticazione.
- **Si avvia prima la sorgente**: registra il canale e attende. La destinazione
  che arriva su un canale libero attende a sua volta, ma tenere l'ordine rende i
  log leggibili.
- **Un canale ospita un trasferimento alla volta.** Per più trasferimenti in
  parallelo servono `--channel` diversi.
- **La sorgente non viene mai modificata.** È l'invariante centrale del
  programma: la sua impronta viene verificata prima e dopo ogni esecuzione,
  comprese quelle interrotte, e una deriva chiude con exit code `6`.

## Precedenza della configurazione

Dalla più forte alla più debole: **CLI > variabili d'ambiente > YAML**.

Ogni flag ha la sua variabile `RUST_BACKUP_<NOME_IN_MAIUSCOLO>` (`--auth-db` →
`RUST_BACKUP_AUTH_DB`). Le uniche eccezioni sono `-v/--verbose`, `-P/--param` e
`--no-udp`. Un flag non passato "cade" sulla variabile d'ambiente e poi sul
valore YAML di `--config`; solo se manca ovunque vale il default interno.
Dettagli in [10 — Variabili d'ambiente](10-variabili-ambiente.md).

## Le credenziali vanno nell'ambiente, non nella riga di comando

Su Linux `/proc/<pid>/cmdline` è leggibile da chiunque, mentre
`/proc/<pid>/environ` è leggibile solo dal proprietario del processo. Password,
URI MongoDB e chiavi S3 passate come flag restano visibili a ogni utente locale
per tutta la durata del trasferimento.

```bash
# NO
rust-backup postgres source … --password 'S3gr3t0'

# SÌ
RUST_BACKUP_PASSWORD='S3gr3t0' rust-backup postgres source …
```

Per il segreto del trasporto esiste anche `--secret-file`, che è la forma
preferibile: il file viene letto, ripulito dal newline finale e rifiutato se
vuoto.

## Interruzione di un'esecuzione

`SIGINT` (Ctrl-C) e `SIGTERM` interrompono in modo pulito: l'elemento in scrittura
sulla destinazione viene rimosso, nessuna riga `VERIFIED` viene stampata e il
comando esce con errore. `SIGKILL` non lascia eseguire nessuna pulizia; in quel
caso la destinazione "sporca" viene rifiutata alla corsa successiva (per il
filesystem: la root di destinazione deve essere assente o vuota).

## Verbosità e log

I log vanno su **stderr** (`stdout` resta libero per la stampa del piano di
`plan`). `-v` porta il livello a `debug`, `-vv` a `trace`; `RUST_LOG` ha la
precedenza su entrambi e accetta la sintassi dei filtri `tracing`
(`RUST_LOG=rb_transport=debug,info`). Per tutta l'esecuzione, ogni 5 secondi,
viene emessa una riga di avanzamento che inizia con la **fase** in corso e
termina con da quanto tempo vi si trova (`… 1m45s in this stage`):

| fase | lato | cosa riporta |
|---|---|---|
| `connecting:` | entrambi | attesa del peer e del piano |
| `audit:` | sorgente | impronta per il controllo di immutabilità (prima e dopo la copia; mai con `--hot-backup`) |
| `analyze:` | sorgente | lettura della sorgente e costruzione del piano |
| `preflight:` | destinazione | controlli del piano sulla destinazione |
| `transfer:` | entrambi | `items 42/1462  878.80 KiB/~1.41 GiB  (0.1%)  25.11 KiB/s`: il totale con `~` è la **stima** del piano (PostgreSQL la calcola dalle dimensioni su disco, spesso maggiori del flusso reale), la velocità è quella degli ultimi 5 secondi |
| `finalize:` | destinazione | lavoro dopo i dati, per i moduli che lo hanno: su PostgreSQL indici, vincoli e `REFRESH` delle viste materializzate, `step 120/7800` |
| `verify:` | destinazione | rilettura di quanto scritto: prima il confronto del catalogo, poi `items 384/1462  170.45 MiB/1.03 GiB  (16.4%)` sui byte effettivi |
| `waiting:` | sorgente | payload inviato, in attesa del verdetto della destinazione; con una destinazione 0.0.15 o successiva la riga riporta anche la sua fase: `waiting: payload sent (2 items, 952.72 MiB), 7s in this stage; destination: verify: reading the restored tables back, items 0/2  619.00 MiB/952.72 MiB  (65.0%) …` |

Alla fine della fase `transfer:` il totale stimato viene sostituito da quello
reale. L'ultima riga dice `done:` (con la velocità media e la durata totale) o
`failed during <fase>:`.
