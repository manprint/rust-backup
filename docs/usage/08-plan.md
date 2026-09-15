# 08 — Dry-run: `rust-backup plan`

`plan` apre **solo** la sorgente, la analizza in sola lettura e stampa il piano
che verrebbe trasferito. Non contatta nessun server di coordinamento, non apre
nessun canale, non scrive niente da nessuna parte. È il modo per rispondere a
"cosa succederebbe?" prima di far partire un trasferimento.

## Caso minimal

```bash
rust-backup plan filesystem --root /srv/data
```

## Sintassi

```text
rust-backup plan <MODULO> [PARAMETRI DEL MODULO]
```

`<MODULO>` ∈ `postgres` | `mongodb` | `filesystem` | `s3`.

Il ruolo **non** si indica: `plan` è per definizione un'analisi del lato
sorgente. Scrivere `rust-backup plan postgres source …` è un errore
(`unexpected argument 'source' found`).

I flag accettati sono tutti e soli i parametri di modulo — gli stessi di
[03 PostgreSQL](03-postgres.md), [04 MongoDB](04-mongodb.md),
[05 Filesystem](05-filesystem.md), [06 S3](06-s3.md) — più:

| Flag | Variabile | A cosa serve |
|------|-----------|--------------|
| `-P`, `--param chiave=valore` | — | parametri di modulo senza flag dedicato (ripetibile) |
| `--config <file.yml>` | `RUST_BACKUP_CONFIG` | usa come base il target `module` + `role: source` del file YAML |
| `-v`, `-vv` | — | verbosità dei log (che restano su stderr) |

I flag di trasporto (`--to`, `--channel`, `--secret`, `--carriers`, …) **non**
sono accettati: non c'è nessuna connessione da stabilire.

## Esempi per modulo

```bash
# PostgreSQL: l'intero cluster visibile all'utente di sola lettura
RUST_BACKUP_PASSWORD='…' rust-backup plan postgres --host pg-source.internal --user backup_readonly

# PostgreSQL: un solo database
RUST_BACKUP_PASSWORD='…' rust-backup plan postgres --host pg-source.internal --user backup_readonly --database app

# MongoDB
RUST_BACKUP_URI="mongodb://backup_readonly:pw@mongo-source.internal:27017/?authSource=admin" \
rust-backup plan mongodb --database app --auth-db admin

# S3 / MinIO
RUST_BACKUP_ACCESS_KEY=… RUST_BACKUP_SECRET_KEY=… \
rust-backup plan s3 --endpoint http://127.0.0.1:9000 --path-style --bucket sorgente

# Partendo da un file di sessione già scritto
rust-backup plan postgres --config /secure/path/postgres-session.yml
```

## Come si legge l'output

Il piano va su **stdout** (i log su stderr), quindi è salvabile e confrontabile:

```bash
rust-backup plan filesystem --root /srv/data > piano-oggi.txt
diff piano-ieri.txt piano-oggi.txt
```

```text
Backup plan  module=filesystem  mode=Copy1to1  created=2026-09-15T19:00:51Z
  filesystem tree /srv/data (3 entries)
  items: 2   estimated: 97.66 KiB   integrity: blake3
    [  1] file       a.txt                                    5 B
    [  2] file       sub/bin.dat                              97.66 KiB
```

| Campo | Significato |
|-------|-------------|
| `module` | modulo che ha prodotto il piano |
| `mode` | modalità di copia (`Copy1to1`: ricostruzione fedele 1:1) |
| `created` | istante UTC dell'analisi |
| riga descrittiva | riepilogo specifico del backend (albero, cluster, bucket…) |
| `items` | numero di elementi che verranno trasferiti |
| `estimated` | dimensione stimata del payload |
| `integrity` | algoritmo dei commitment (BLAKE3) |
| elenco | gli elementi, nell'ordine in cui verranno trasferiti |

È lo stesso testo che la destinazione mostra prima di chiedere conferma: se si
usa `plan` prima, alla conferma non ci sono sorprese.

## A cosa serve davvero

- **Verificare le credenziali e i privilegi** della sorgente senza coinvolgere
  la destinazione.
- **Scoprire in anticipo i rifiuti**: oggetti PostgreSQL non riproducibili,
  viste/time-series MongoDB, bucket S3 con versioning o SSE. `plan` fallisce
  esattamente come fallirebbe il trasferimento, ma in pochi secondi.
- **Stimare la durata**: `items` ed `estimated` dicono quanto dovrà passare
  sulla rete.
- **Controllare la deriva** fra due momenti, salvando l'output e confrontandolo.

## Codici di uscita

Gli stessi degli altri comandi, limitatamente alle fasi che `plan` esegue:
`0` piano prodotto, `2` configurazione/credenziali, `1` o codice specifico per un
errore d'analisi. Vedi [11 — Codici di uscita](11-codici-uscita.md).
