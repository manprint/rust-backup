# 09 — Docker e Docker Compose

L'immagine contiene **lo stesso binario** della distribuzione nativa: serve sia
come server di coordinamento sia come client sorgente/destinazione. È
pubblicata per `linux/amd64` e `linux/arm64`.

## Caso minimal

```bash
docker run --rm -p 7835:7835/tcp -p 7835:7835/udp ghcr.io/manprint/rust-backup:latest
```

Il comando predefinito dell'immagine è già il server di coordinamento su
`0.0.0.0:7835`. Per usarla come client basta appendere i soliti argomenti:

```bash
docker run --rm --network host ghcr.io/manprint/rust-backup:latest \
  filesystem source --to 127.0.0.1:7835 --channel demo --root /source
```

## Come è fatta l'immagine

| Caratteristica | Valore |
|----------------|--------|
| Registro | `ghcr.io/manprint/rust-backup` |
| Entrypoint | `rust-backup` (gli argomenti del `docker run` sono quelli della CLI) |
| Comando predefinito | `server --bind-addr 0.0.0.0 --control-port 7835` |
| Utente | non-root, UID/GID `65532` |
| Porte dichiarate | `7835/tcp`, `7835/udp` |
| Healthcheck | verifica che la porta 7835 risponda |
| Base | `debian:bookworm-slim` con sole `ca-certificates` e `netcat-openbsd` |

Tag disponibili:

| Tag | Contenuto |
|-----|-----------|
| `latest` | ultimo commit del ramo `main` |
| `dev` | ultimo commit del ramo `dev` |
| `0.0.1`, `0.0` | rilascio corrispondente al tag `v0.0.1` |
| `v0.0.1` | il tag Git così com'è |
| `sha-<commit>` | build di un commit specifico |

```bash
docker pull ghcr.io/manprint/rust-backup:latest
docker run --rm ghcr.io/manprint/rust-backup:latest --version
```

## Server con Docker Compose

Il file [compose.yml](../../compose.yml) della repository avvia il coordinatore
con filesystem in sola lettura, `cap_drop: ALL`, `no-new-privileges` e il
segreto montato come Docker secret.

```bash
openssl rand -base64 48 > deploy/coordination/coordination.secret
chmod 600 deploy/coordination/coordination.secret

docker compose pull
docker compose up -d coordinator
docker compose ps
docker compose logs -f coordinator
```

Variabili lette da `compose.yml`:

| Variabile | Default | Effetto |
|-----------|---------|---------|
| `RUST_BACKUP_IMAGE` | `ghcr.io/manprint/rust-backup:latest` | immagine da eseguire |
| `RUST_BACKUP_CONTROL_PORT` | `7835` | **solo la porta TCP pubblicata sull'host.** Dentro il container il server ascolta sempre su 7835, perché il `command` passa `--control-port 7835` letterale. I client si collegano alla porta pubblicata |
| `RUST_BACKUP_UDP_PORT` | `7835` | porta UDP pubblicata sull'host, stesso significato di sola mappatura |
| `RUST_BACKUP_MAX_CONNS` | `256` | passata come `--max-conns`: questa cambia davvero il comportamento del server |
| `RUST_BACKUP_UDP` | `true` | passata come `--udp` |
| `RUST_BACKUP_SECRET_FILE` | `./deploy/coordination/coordination.secret` | percorso host del file montato come Docker secret |
| `RUST_BACKUP_VERSION` | `dev` | build arg inciso nell'immagine (solo con `docker compose build`) |
| `RUST_BACKUP_VCS_REF` | `local` | build arg inciso nell'immagine (solo con `docker compose build`) |

Esempio di override:

```bash
RUST_BACKUP_IMAGE=ghcr.io/manprint/rust-backup:dev \
RUST_BACKUP_CONTROL_PORT=17835 \
RUST_BACKUP_UDP_PORT=17835 \
RUST_BACKUP_MAX_CONNS=1024 \
docker compose up -d
```

### TLS

Mettere `tls.crt` e `tls.key` in `deploy/coordination/tls/` e applicare
l'override:

```bash
docker compose -f compose.yml -f deploy/coordination/compose.tls.yml up -d
```

I client useranno `--to https://coordinatore.example:7835`. Un reverse proxy può
terminare il TLS del TCP, ma la porta UDP 7835 deve arrivare direttamente al
coordinatore se il percorso QUIC è abilitato.

## Client in container: le tre cose da ricordare

1. **Rete.** `--network host` è il modo più semplice perché il client raggiunga
   sia il coordinatore sia i backend locali. In alternativa vanno pubblicate o
   instradate esplicitamente le porte.
2. **Dati.** Il container vede solo ciò che gli si monta: la sorgente in sola
   lettura (`-v /srv/data:/source:ro`), la destinazione in scrittura. Il valore
   di `--root` è il percorso **dentro** il container.
3. **Segreti.** Password e chiavi si passano con `-e VARIABILE=...` (o
   `--env-file`), il segreto del trasporto come file montato in sola lettura.

```bash
IMAGE=ghcr.io/manprint/rust-backup:latest
SECRET=/etc/rust-backup/coordination.secret

docker run --rm --network host \
  -v "$SECRET:/run/secrets/coordinator:ro" \
  -v /srv/data:/source:ro \
  "$IMAGE" filesystem source --to 127.0.0.1:7835 --channel fs-prod \
  --secret-file /run/secrets/coordinator --carriers 4 --root /source
```

### Ripristino di filesystem con ownership

L'immagine gira come UID 65532: senza privilegi non può assegnare uid/gid
arbitrari. Per un ripristino fedele serve `--user 0:0` (ed eventualmente le
capability del demone Docker):

```bash
docker run --rm --network host --user 0:0 \
  -v "$SECRET:/run/secrets/coordinator:ro" \
  -v /srv/restore:/restore \
  "$IMAGE" filesystem destination --to 127.0.0.1:7835 --channel fs-prod \
  --secret-file /run/secrets/coordinator --carriers 4 --root /restore --yes
```

In alternativa, restare non-root e accettare il contratto ridotto con
`--no-preserve-ownership`.

### Database e oggetti

```bash
# PostgreSQL
docker run --rm --network host \
  -e RUST_BACKUP_PASSWORD="$SOURCE_PG_PASSWORD" \
  "$IMAGE" postgres source --to 127.0.0.1:7835 --channel pg \
  --host 127.0.0.1 --user backup_readonly --database app

# MongoDB
docker run --rm --network host \
  -e RUST_BACKUP_URI="mongodb://backup_readonly:pw@127.0.0.1:27017/?authSource=admin" \
  "$IMAGE" mongodb source --to 127.0.0.1:7835 --channel mg --database app

# S3 / MinIO
docker run --rm --network host \
  -e RUST_BACKUP_ACCESS_KEY=minioadmin -e RUST_BACKUP_SECRET_KEY="$MINIO_PASSWORD" \
  "$IMAGE" s3 source --to 127.0.0.1:7835 --channel s3 \
  --endpoint http://127.0.0.1:9000 --path-style --bucket sorgente
```

### Sessioni YAML in container

```bash
docker run --rm --network host \
  -v /secure/path/sessione.yml:/etc/rust-backup/sessione.yml:ro \
  -v /srv/data:/source:ro \
  "$IMAGE" run --config /etc/rust-backup/sessione.yml
```

I percorsi dentro il file devono essere quelli visti dal container.

## Errori frequenti

| Sintomo | Causa e rimedio |
|---------|-----------------|
| il client non raggiunge il coordinatore | manca `--network host` o la porta non è pubblicata; dentro un container `127.0.0.1` è il container stesso |
| `--root` punta a una directory vuota | il volume non è montato, o è montato in un percorso diverso da quello passato a `--root` |
| ripristino con proprietario sbagliato | container non-root: usare `--user 0:0` oppure `--no-preserve-ownership` |
| permission denied sul file del segreto | il file montato deve essere leggibile dall'UID 65532 (o dall'utente scelto con `--user`) |
| il percorso diretto QUIC non si stabilisce | la porta UDP non è pubblicata; il trasferimento prosegue comunque sul relay |
