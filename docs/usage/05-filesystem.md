# 05 — Filesystem (alberi di directory POSIX)

Copia 1:1 di un albero di directory dalla sorgente a una root di destinazione
**vuota**. Permessi, link e date sono preservati; l'ownership (uid/gid) è
preservata quando la destinazione ha i privilegi per farlo. È l'unico modulo che
sfrutta più carrier paralleli (fino a 32).

## Caso minimal

```bash
# sorgente: sola lettura, non serve alcun privilegio
rust-backup filesystem source --to 127.0.0.1:7835 --channel fs --root /srv/data

# destinazione: la root deve essere assente o vuota
rust-backup filesystem destination --to 127.0.0.1:7835 --channel fs --root /srv/restore --yes
```

Se l'albero contiene file di utenti diversi da quello che esegue la
destinazione, questo caso minimal fallisce in preflight: servono i privilegi
(`sudo`) oppure la rinuncia esplicita all'ownership (vedi sotto).

## Caso tipico in produzione

```bash
# sorgente, 4 carrier paralleli
rust-backup filesystem source \
  --to https://coordinatore.example:7835 --channel fs-prod \
  --secret-file /etc/rust-backup/coordination.secret \
  --carriers 4 --root /srv/data

# destinazione: root/CAP_CHOWN per ripristinare uid/gid esatti
sudo rust-backup filesystem destination \
  --to https://coordinatore.example:7835 --channel fs-prod \
  --secret-file /etc/rust-backup/coordination.secret \
  --carriers 4 --root /srv/restore --yes
```

## Parametri del modulo

| Flag | Variabile | Default | Lato | A cosa serve |
|------|-----------|---------|------|--------------|
| `--root <percorso>` | `RUST_BACKUP_ROOT` | — (obbligatorio) | entrambi | sorgente: directory da copiare. Destinazione: directory in cui ripristinare, che deve essere **assente o vuota** |
| `--no-preserve-ownership[=bool]` | `RUST_BACKUP_NO_PRESERVE_OWNERSHIP` | falso (cioè: ownership preservata) | destinazione | rinuncia esplicita al ripristino di uid/gid. I file diventano dell'utente che esegue il processo; contenuti, permessi, date e link restano verificati |
| `--allow-atime-updates[=bool]` | `RUST_BACKUP_ALLOW_ATIME_UPDATES` | falso | sorgente | accetta che la lettura aggiorni l'**access time** dei file. Serve solo quando il processo non è proprietario dei file e non ha `CAP_FOWNER`: in quel caso `O_NOATIME` viene rifiutato dal kernel e senza questo flag la corsa termina prima di trasferire qualsiasi byte |
| `--follow-symlinks[=bool]` | `RUST_BACKUP_FOLLOW_SYMLINKS` | falso | entrambi | **rifiutato da questa build**: seguire un link potrebbe uscire dalla root dichiarata e non preserverebbe il link come tale. I symlink vengono comunque copiati *come symlink* |
| `--preserve-xattr[=bool]` | `RUST_BACKUP_PRESERVE_XATTR` | falso | entrambi | **rifiutato da questa build**: gli attributi estesi saranno supportati quando esisterà un'implementazione sicura compatibile con `forbid(unsafe_code)` |
| `-P preserve_ownership=true\|false` | — | `true` | destinazione | stesso parametro di `--no-preserve-ownership`, ma con il nome positivo: è la forma usata nello YAML |

Valgono inoltre tutti i flag di trasporto di [02 — Trasporto](02-trasporto.md).
Qui `--carriers` è utile davvero: fino a **32**.

### Access time e `O_NOATIME`

La sorgente apre ogni file con `O_NOATIME`, così una copia non lascia traccia
nemmeno nell'access time. Il kernel concede `O_NOATIME` solo al proprietario del
file (o a chi ha `CAP_FOWNER`): su un albero di file altrui la `open` risponde
`EPERM` e leggerli **cambierebbe** il loro atime, cioè modificherebbe la
sorgente. Senza il flag la corsa si ferma subito, durante la prima impronta:

```text
[Analyze] cannot open /srv/data/file.bin without updating its access time
(O_NOATIME needs file ownership or CAP_FOWNER); run as the file owner or root,
or pass --allow-atime-updates to accept atime changes on the source
```

Rimedi, in ordine di preferenza: eseguire come proprietario dei file o come
root, oppure passare `--allow-atime-updates` per accettare la modifica. Con il
flag la corsa prosegue e stampa una volta:

```text
WARN atime updates on the source accepted by --allow-atime-updates
```

### File speciali

L'albero non è fatto solo di file, directory e link. Cosa succede agli altri:

| Voce | Sorgente | Destinazione | Privilegio |
|------|----------|--------------|------------|
| FIFO (pipe con nome) | copiata come nodo, mai il suo contenuto | ricreata con modo e mtime | nessuno |
| device a caratteri / a blocchi | copiati con il loro numero di device | ricreati con `mknod` | **root o `CAP_MKNOD`** |
| socket unix | **rifiutata durante l'analisi** | — | — |

Un socket unix esiste solo finché un processo lo tiene aperto: ricrearne l'inode
darebbe un nodo morto senza nessuno in ascolto, cioè certificherebbe qualcosa
che la sorgente non ha. La corsa si ferma prima di trasferire qualsiasi byte:

```text
[Analyze] unsupported filesystem entry (a unix socket cannot be reproduced): /srv/data/app.sock
```

I device sono l'unica voce il cui ripristino richiede una capability propria. Se
manca, il **preflight** fallisce sul controllo `special_files`, prima di
scrivere qualsiasi cosa:

```text
restoring device nodes needs root or CAP_MKNOD (2 device entries in the plan)
```

Rimedio: eseguire la destinazione con `sudo` (o concederle `CAP_MKNOD`), oppure
escludere i device dall'albero copiato.

Passare `--follow-symlinks` o `--preserve-xattr` con valore vero fa terminare il
comando con:

```text
filesystem follow_symlinks and preserve_xattr are not supported by this safe backend
```

## Cosa viene preservato

| Attributo | Sorgente | Destinazione | Privilegi |
|-----------|----------|--------------|-----------|
| contenuto dei file | sempre | sempre | nessuno |
| permessi (rwx, setuid/setgid, sticky) | sempre | sempre | nessuno |
| mtime (anche nanosecondi e date precedenti al 1970) | sempre | sempre | nessuno |
| symlink e hardlink | sempre | ricreati | nessuno |
| **uid / gid** | sempre (registrati nel piano) | **solo se privilegiata** | root o `CAP_CHOWN` |
| xattr | non supportati | non supportati | — |

Dettagli e garanzie formali: [docs/modules/FILESYSTEM.md](../modules/FILESYSTEM.md).

Due comportamenti da conoscere:

- **La root di destinazione non viene ripristinata**: modo, proprietario e mtime
  della directory radice restano quelli con cui l'operatore l'ha creata. Vengono
  riprodotte solo le voci *interne*. Creare la root con i permessi desiderati
  prima dell'esecuzione.
- I file vengono creati privati (`0600`/`0700`) e allargati al loro modo finale
  solo dopo contenuto e ownership: non esiste una finestra in cui un file
  riservato è leggibile da tutti.

## Il caso `sudo` / ownership

Assegnare un `uid`/`gid` arbitrario è un'operazione privilegiata su Linux.

- **Con privilegi** (consigliato per backup di sistema): eseguire la
  destinazione con `sudo`. L'ownership viene ripristinata esattamente.
- **Senza privilegi**: se il piano contiene voci con uid/gid diversi da quelli
  del processo, il **preflight fallisce** con

  ```text
  exact uid/gid restore requires root or CAP_CHOWN; use --no-preserve-ownership
  to explicitly exclude ownership from the restore contract
  ```

  Si sceglie allora consapevolmente il contratto ridotto:

  ```bash
  rust-backup filesystem destination --to coord:7835 --channel fs \
    --root /srv/restore --no-preserve-ownership --yes
  ```

La **sorgente non ha mai bisogno di privilegi**: apre in sola lettura e usa
`O_NOATIME` dove il kernel lo consente, così non tocca nemmeno gli access time.

## Controlli di preflight sulla destinazione

Prima di scrivere qualunque cosa, la destinazione verifica e stampa:

| Controllo | Significato |
|-----------|-------------|
| `destination-parent` | la directory che contiene la root esiste |
| `destination-empty` | la root è assente o vuota; **voci preesistenti non vengono mai cancellate** |
| `ownership` | se il ripristino di uid/gid è richiesto, il processo può farlo |
| `special_files` | se il piano contiene device node, il processo ha root o `CAP_MKNOD` |
| `estimated-bytes` | quantità di dati da ripristinare |

Non esiste un `--overwrite` per il filesystem: per rifare un ripristino si
svuota (o si cambia) la root a mano. È deliberato — un `rm -rf` implicito su una
directory di sistema non è un comportamento che il programma vuole avere.

## Interruzioni

`SIGINT`/`SIGTERM` interrompono: il file attivo viene rimosso, nessuna riga
`VERIFIED` viene stampata, l'uscita è diversa da zero. I file già completati
restano su disco: un ripristino interrotto lascia un *albero* parziale, mai un
*file* parziale — e nulla di esso è certificato. Per ripetere, svuotare la root.

## Anteprima senza trasferire nulla

```bash
rust-backup plan filesystem --root /srv/data
```

```text
Backup plan  module=filesystem  mode=Copy1to1  created=2026-09-15T19:00:51Z
  filesystem tree /srv/data (3 entries)
  items: 2   estimated: 97.66 KiB   integrity: blake3
    [  1] file       a.txt                                    5 B
    [  2] file       sub/bin.dat                              97.66 KiB
```

## In container

`--network host` permette al client di raggiungere il coordinatore sull'host; i
dati vanno montati (la sorgente in sola lettura):

```bash
IMAGE=ghcr.io/manprint/rust-backup:latest
SECRET="/etc/rust-backup/coordination.secret"

docker run --rm --network host \
  -v "$SECRET:/run/secrets/coordinator:ro" \
  -v /srv/data:/source:ro \
  "$IMAGE" filesystem source --to 127.0.0.1:7835 --channel fs-prod \
  --secret-file /run/secrets/coordinator --carriers 4 --root /source

# --user 0:0 serve per ripristinare uid/gid esatti (l'immagine gira come 65532)
docker run --rm --network host --user 0:0 \
  -v "$SECRET:/run/secrets/coordinator:ro" \
  -v /srv/restore:/restore \
  "$IMAGE" filesystem destination --to 127.0.0.1:7835 --channel fs-prod \
  --secret-file /run/secrets/coordinator --carriers 4 --root /restore --yes
```

## In sessione YAML

```yaml
parallel_targets: 2
targets:
  - module: filesystem
    role: source
    transport: { to: coordinatore.example:7835, channel: fs-home, secret: CAMBIAMI, carriers: 4 }
    params:
      root: /srv/home
      preserve_ownership: true

  - module: filesystem
    role: destination
    transport: { to: coordinatore.example:7835, channel: fs-home, secret: CAMBIAMI, carriers: 4 }
    params:
      root: /restore/home
      preserve_ownership: true
    auto_accept: true
```

Modello pronto: [examples/filesystem-session.yml](../../examples/filesystem-session.yml).

## Costo dell'impronta e limiti di perimetro

L'impronta che dimostra che la sorgente non è cambiata contiene il **contenuto
completo** di ogni file, e viene calcolata due volte: prima del trasferimento e
dopo. Una copia legge quindi l'albero sorgente **tre volte** (impronta, stream,
impronta). Su alberi grandi è il costo dominante della corsa; è il prezzo della
riga `BACKUP VERIFIED: source unchanged`, perché un'impronta sui soli metadati
certificherebbe anche un albero riscritto sul posto.

Cosa resta fuori dal perimetro, per scelta esplicita:

| Caso | Comportamento |
|------|---------------|
| file sparsi (con buchi) | ripristinati **densi**: stessa dimensione e stesso contenuto byte per byte, ma i buchi diventano zeri scritti e l'occupazione su disco può crescere |
| attributi estesi (xattr) e ACL POSIX | **non letti**: `--preserve-xattr` viene rifiutato alla connessione invece di essere ignorato in silenzio |
| socket unix | rifiutati durante l'analisi |
| FIFO e device | copiati; i device richiedono root o `CAP_MKNOD` sulla destinazione |

## Errori frequenti

| Sintomo | Causa e rimedio |
|---------|-----------------|
| preflight `destination-empty` fallito | la root contiene già qualcosa: svuotarla o usarne un'altra |
| preflight `ownership` fallito | eseguire la destinazione con `sudo`, oppure accettare `--no-preserve-ownership` |
| preflight `special_files` fallito | l'albero contiene device node: eseguire la destinazione con `sudo`/`CAP_MKNOD`, oppure copiare un albero che non li contiene |
| `a unix socket cannot be reproduced` | rimuovere il socket dall'albero copiato (o copiarne una sottodirectory che non lo contiene): un socket non è riproducibile |
| `follow_symlinks and preserve_xattr are not supported` | rimuovere quei flag: non sono supportati da questa build |
| trasferimento lento su molti file piccoli | alzare `--carriers` (fino a 32) su entrambi i lati |
| exit `6` | l'albero sorgente è cambiato durante la copia: fermare le scritture e ripetere |
