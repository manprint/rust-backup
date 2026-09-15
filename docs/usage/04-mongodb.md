# 04 — MongoDB

Copia **logica** di database MongoDB con driver Rust puro: `mongodump` e
`mongorestore` non vengono usati. Versioni supportate: **MongoDB 4–8**, comprese
le coppie cross-major (per esempio 4→8) verificate in CI.

## Caso minimal

MongoDB in ascolto su `localhost:27017` senza autenticazione (i default del
modulo sono già questi):

```bash
rust-backup mongodb source      --to 127.0.0.1:7835 --channel mg
rust-backup mongodb destination --to 127.0.0.1:7835 --channel mg --yes
```

Senza `--database` vengono copiati **tutti i database non di sistema**.

## Caso tipico in produzione

L'URI contiene le credenziali: si passa come variabile d'ambiente, mai come
flag.

```bash
RUST_BACKUP_URI="mongodb://backup_readonly:${MONGO_SOURCE_PASSWORD}@mongo-source.internal:27017/?authSource=admin&tls=true" \
rust-backup mongodb source \
  --to https://coordinatore.example:7835 --channel mongo-prod --secret-file /run/secrets/rb \
  --database app --auth-db admin

RUST_BACKUP_URI="mongodb://root:${MONGO_DEST_PASSWORD}@mongo-destination.internal:27017/?authSource=admin&tls=true" \
rust-backup mongodb destination \
  --to https://coordinatore.example:7835 --channel mongo-prod --secret-file /run/secrets/rb \
  --database app --auth-db admin --yes
```

## Parametri del modulo

| Flag | Variabile | Default | Lato | A cosa serve |
|------|-----------|---------|------|--------------|
| `--uri <uri>` | `RUST_BACKUP_URI` | nessuno | entrambi | URI di connessione completo. **Se presente, host/porta/utente/password discreti vengono ignorati.** È l'unica via per opzioni avanzate (`tls=true`, replica set, `readPreference`, …) |
| `--host <host>` | `RUST_BACKUP_HOST` | `localhost` | entrambi | host, usato solo se `--uri` non è impostato |
| `--port <n>` | `RUST_BACKUP_PORT` | `27017` | entrambi | porta, usata solo se `--uri` non è impostato |
| `--user <nome>` | `RUST_BACKUP_USER` | nessuno | entrambi | utente, usato solo senza `--uri` |
| `--password <pw>` | `RUST_BACKUP_PASSWORD` | nessuna | entrambi | password, usata solo senza `--uri`. Passarla come variabile d'ambiente |
| `--database <nome>` | `RUST_BACKUP_DATABASE` | nessuno | entrambi | limita la copia a un solo database. Se assente: tutti i database non di sistema |
| `--auth-db <nome>` | `RUST_BACKUP_AUTH_DB` | nessuno | entrambi | database di autenticazione (di norma `admin`). È anche il database su cui vengono eseguiti i comandi di cluster (`buildInfo`, `usersInfo`); in mancanza si usa il database di destinazione della copia, altrimenti `admin` |
| `--overwrite[=bool]` | `RUST_BACKUP_OVERWRITE` | falso | destinazione | autorizza il ripristino su collection già esistenti, eliminandole prima |
| `-P allow_skipped_namespaces=true` | — | falso | sorgente | accetta una copia parziale che omette i namespace non riproducibili (viste, collection time-series) |

Valgono inoltre tutti i flag di trasporto di [02 — Trasporto](02-trasporto.md).
`--carriers` viene sempre ridotto a **1**: il restore usa un accumulatore di
batch ordinato.

## Cosa viene copiato

Database, opzioni delle collection, indici e documenti BSON. Il dettaglio
dell'ordine di ripristino e delle garanzie è in
[docs/modules/MONGODB.md](../modules/MONGODB.md).

**Utenti e ruoli non vengono ricreati**: `usersInfo` non espone le credenziali,
quindi ricrearli produrrebbe utenti senza password. Vanno gestiti a parte.

## Viste e time-series

Viste e collection time-series non vengono copiate. Di default l'analisi
**fallisce** invece di certificare come "1:1" una copia che le ha ignorate. Per
procedere consapevolmente:

```bash
rust-backup mongodb source … -P allow_skipped_namespaces=true
```

I namespace omessi vengono elencati nei log.

## `--overwrite`: cosa distrugge e quando

Le collection di destinazione vengono eliminate **prima** che arrivi il primo
documento. Un'esecuzione fallita a metà quindi non lascia il contenuto
precedente: il modulo rimuove anche ciò che aveva creato. Se i dati precedenti
devono sopravvivere a un tentativo fallito, ripristinare su un nome nuovo e
commutare dopo.

## Privilegi

- **Sorgente**: lettura dei database/collection interessati e accesso ai comandi
  di catalogo (`listDatabases`, `listCollections`, `listIndexes`); la lettura
  degli utenti richiede il privilegio `usersInfo` (best-effort).
- **Destinazione**: creazione ed eliminazione di collection e indici sui database
  di destinazione.

## TLS

Il modulo non ha un flag TLS: si attiva nell'URI.

```bash
RUST_BACKUP_URI="mongodb://utente:pw@host:27017/?authSource=admin&tls=true" rust-backup mongodb source …
```

Con host/porta discreti la connessione è in chiaro, a meno che non sia la policy
del server a imporre altro.

## Anteprima senza trasferire nulla

```bash
RUST_BACKUP_URI="mongodb://backup_readonly:pw@mongo-source.internal:27017/?authSource=admin" \
rust-backup plan mongodb --database app --auth-db admin
```

## In container

```bash
docker run --rm --network host \
  -e RUST_BACKUP_URI="mongodb://backup_readonly:${MONGO_SOURCE_PASSWORD}@127.0.0.1:27017/?authSource=admin" \
  ghcr.io/manprint/rust-backup:latest \
  mongodb source --to 127.0.0.1:7835 --channel mongo-prod --database app

docker run --rm --network host \
  -e RUST_BACKUP_URI="mongodb://root:${MONGO_DEST_PASSWORD}@127.0.0.1:37017/?authSource=admin" \
  ghcr.io/manprint/rust-backup:latest \
  mongodb destination --to 127.0.0.1:7835 --channel mongo-prod --database app --yes
```

## In sessione YAML

```yaml
parallel_targets: 2
targets:
  - module: mongodb
    role: source
    transport: { to: coordinatore.example:7835, channel: mongo-prod, secret: CAMBIAMI }
    params:
      uri: "mongodb://backup_readonly:CAMBIAMI@mongo-source.internal:27017/?authSource=admin"
      database: app
      auth_db: admin

  - module: mongodb
    role: destination
    transport: { to: coordinatore.example:7835, channel: mongo-prod, secret: CAMBIAMI }
    params:
      uri: "mongodb://root:CAMBIAMI@mongo-destination.internal:27017/?authSource=admin"
      database: app
      auth_db: admin
      overwrite: false
    auto_accept: true
```

Modello pronto: [examples/mongodb-session.yml](../../examples/mongodb-session.yml).

## Errori frequenti

| Sintomo | Causa e rimedio |
|---------|-----------------|
| exit `2` | URI malformato o parametri incoerenti; ricordare che con `--uri` i campi discreti vengono ignorati |
| exit `3`, collection già presente | serve `--overwrite` o una destinazione pulita |
| analisi rifiutata per viste/time-series | `-P allow_skipped_namespaces=true` se la copia parziale è accettabile |
| autenticazione fallita | `--auth-db admin` mancante, oppure `authSource` assente nell'URI |
| exit `5` | la rilettura non coincide: il ripristino non è valido |
| exit `6` | la sorgente è stata modificata durante la copia |
