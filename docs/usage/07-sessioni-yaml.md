# 07 — Sessioni YAML (`rust-backup run`)

Un file di sessione descrive **uno o più target** (lato sorgente, lato
destinazione, o entrambi) e li esegue con un solo comando. È la forma adatta a
cron, systemd timer e pipeline di CI.

## Caso minimal

Il file minimo eseguibile contiene un solo target:

```yaml
# sessione.yml
targets:
  - module: filesystem
    role: source
    transport:
      to: 127.0.0.1:7835
      channel: demo
    params:
      root: /srv/data
```

```bash
rust-backup run --config sessione.yml
```

Se nello **stesso** file stanno sorgente e destinazione dello stesso canale,
serve obbligatoriamente `parallel_targets: 2` (o più): i due lati devono girare
insieme per incontrarsi.

```yaml
parallel_targets: 2
targets:
  - module: filesystem
    role: source
    transport: { to: 127.0.0.1:7835, channel: demo }
    params: { root: /srv/data }
  - module: filesystem
    role: destination
    transport: { to: 127.0.0.1:7835, channel: demo }
    params: { root: /srv/restore }
    auto_accept: true
```

## Flag di `rust-backup run`

| Flag | Variabile | Default | A cosa serve |
|------|-----------|---------|--------------|
| `--config <file.yml>` | `RUST_BACKUP_CONFIG` | — (obbligatorio) | percorso del file di sessione |
| `--parallel-targets <n>` | `RUST_BACKUP_PARALLEL_TARGETS` | valore del file, altrimenti `1` | quanti target far girare contemporaneamente. Sovrascrive il valore del file; `1` significa esecuzione sequenziale nell'ordine scritto |
| `--fail-fast` | `RUST_BACKUP_FAIL_FAST` | valore del file, altrimenti falso | al primo target fallito smette di avviarne altri e interrompe quelli in corso |
| `-v`, `-vv` | — | `info` | verbosità |

## Schema completo del file

```yaml
# Numero massimo di target eseguiti contemporaneamente.
# 1 = sequenziale e deterministico. Serve >= 2 se il file contiene
# sorgente e destinazione dello stesso canale.
parallel_targets: 2

# Al primo fallimento non avvia altri target e interrompe quelli attivi.
fail_fast: true

targets:
  - module: postgres          # postgres | mongodb | filesystem | s3
    role: source              # source | destination

    transport:
      to: coordinatore.example:7835   # obbligatorio (https:// per il TLS)
      channel: pg-prod                # obbligatorio, uguale sui due lati
      secret: CAMBIAMI                # opzionale, deve combaciare col server
      carriers: 1                     # default 1, intervallo 1..=32
      udp: true                       # default true
      insecure: false                 # default false, solo per test
      max_rate: 20000000              # opzionale, byte/s; assente o 0 = illimitato

    params:                    # parametri specifici del modulo (vedi le sue pagine)
      host: pg-source.internal
      port: 5432
      user: backup_readonly
      password: CAMBIAMI
      database: app
      sslmode: require
      # solo destinazione: source (default) | default
      extension_version: source
      # solo filesystem, solo sorgente: accetta l'aggiornamento dell'atime
      allow_atime_updates: false

    auto_accept: false         # true = come --yes; ignorato sul lato sorgente

# Una sezione `server:` può esistere nel file, ma `run` la RIFIUTA:
# il coordinatore si avvia sempre con `rust-backup server`.
```

Campi obbligatori: `module`, `role`, `transport.to`, `transport.channel` e i
parametri obbligatori del modulo. Tutto il resto ha un default.

### Ancore YAML per non ripetere il trasporto

```yaml
parallel_targets: 2
targets:
  - module: postgres
    role: source
    transport: &trasporto
      to: coordinatore.example:7835
      channel: pg-prod
      secret: CAMBIAMI
    params: { host: pg-source.internal, user: backup_readonly, password: CAMBIAMI, database: app }
  - module: postgres
    role: destination
    transport: *trasporto
    params: { host: pg-dest.internal, user: postgres, password: CAMBIAMI, admin: true }
    auto_accept: true
```

## Esecuzione sequenziale o parallela

- `parallel_targets: 1` → i target partono **uno alla volta, nell'ordine
  scritto**. Utile quando ogni target è un lato solo (l'altro gira su un altro
  host) e si vuole un ordine deterministico.
- `parallel_targets: N > 1` → fino a N target contemporanei; appena uno finisce
  parte il successivo.
- Con `fail_fast: true` il primo errore blocca gli avvii successivi e annulla i
  target in corso. Senza, la sessione prova comunque tutti i target e riepiloga
  alla fine.

Quando più target falliscono, la sessione esce con il codice del fallimento
**più grave**, nell'ordine: sorgente modificata (`6`) → integrità/apply/verify
(`5`) → preflight (`3`) → piano rifiutato (`4`) → connessione (`7`) →
configurazione (`2`) → altro (`1`). Il messaggio riporta quanti target sono
falliti e qual è stato il più grave.

## Sorgente e destinazione su host diversi

È il caso normale in produzione: un file per host, ciascuno con il solo target
locale.

```yaml
# host-sorgente.yml
targets:
  - module: postgres
    role: source
    transport: { to: coordinatore.example:7835, channel: pg-prod, secret: CAMBIAMI }
    params: { host: localhost, user: backup_readonly, password: CAMBIAMI, database: app }
```

```yaml
# host-destinazione.yml
targets:
  - module: postgres
    role: destination
    transport: { to: coordinatore.example:7835, channel: pg-prod, secret: CAMBIAMI }
    params: { host: localhost, user: postgres, password: CAMBIAMI, admin: true }
    auto_accept: true
```

## Il file YAML come strato sottostante di un comando singolo

`--config` esiste anche sui comandi di modulo: il target che ha lo stesso
`module` e lo stesso `role` viene usato come base, e ciò che si passa da CLI o
da ambiente lo sovrascrive campo per campo.

```bash
# Prende tutto da sessione.yml ma cambia il canale e accetta il piano
rust-backup postgres destination --config sessione.yml --channel pg-collaudo --yes
```

Questo è il modo pulito per tenere le costanti in un file e variare solo il
poco che cambia fra un'esecuzione e l'altra.

## Sicurezza del file

Il file contiene segreti in chiaro (password dei backend, chiavi S3, segreto del
trasporto). Nei log e negli errori questi valori sono oscurati (`[REDACTED]`),
ma il file no:

```bash
cp examples/postgres-session.yml /secure/path/postgres-session.yml
chmod 600 /secure/path/postgres-session.yml
```

Tenerlo fuori dal controllo di versione, oppure generarlo da un secret manager
immediatamente prima dell'uso. In alternativa si lasciano i campi sensibili
fuori dal file e si passano come variabili d'ambiente, che hanno la precedenza.

## Modelli pronti

- [examples/filesystem-session.yml](../../examples/filesystem-session.yml)
- [examples/postgres-session.yml](../../examples/postgres-session.yml)
- [examples/mongodb-session.yml](../../examples/mongodb-session.yml)
- [examples/s3-session.yml](../../examples/s3-session.yml)
- [examples/session.yml](../../examples/session.yml) — sessione mista

## Errori frequenti

| Sintomo | Causa e rimedio |
|---------|-----------------|
| `run --config does not start server:` | rimuovere la sezione `server:` e avviare il coordinatore a parte |
| la sessione resta appesa | file con i due lati dello stesso canale e `parallel_targets: 1`: portarlo ad almeno 2 |
| `YAML parse: …` (exit `2`) | errore di sintassi o campo con tipo sbagliato (per esempio `port` fra virgolette) |
| `unknown module 'xyz'` | `module` non è uno fra `postgres`, `mongodb`, `filesystem`, `s3` |
| la destinazione resta in attesa di conferma | manca `auto_accept: true` (o `--yes`) |
