# 11 — Codici di uscita e diagnostica

## Caso minimal

```bash
rust-backup filesystem destination --to 127.0.0.1:7835 --channel demo --root /srv/restore --yes
echo "exit=$?"     # 0 = copia verificata; qualunque altro valore = nessuna certificazione
```

## Tabella dei codici

| Codice | Nome | Significato | Cosa fare |
|--------|------|-------------|-----------|
| `0` | successo | trasferimento **verificato**: la destinazione ha riletto ciò che ha scritto e la sorgente risulta immutata | niente |
| `1` | errore generico | qualunque fallimento non classificato (I/O, errore interno, interruzione da segnale) | leggere il messaggio su stderr |
| `2` | configurazione | parametro mancante, incoerente o di tipo sbagliato; YAML non valido; modulo sconosciuto | correggere il comando o il file |
| `3` | preflight | la destinazione non è in condizione di ricevere: database già presente senza `--overwrite`, root non vuota, privilegi insufficienti | sistemare la destinazione e ripetere |
| `4` | piano rifiutato | l'operatore ha risposto qualcosa di diverso da `yes` al prompt (o stdin era chiuso) | usare `--yes` / `auto_accept: true` nelle esecuzioni non interattive |
| `5` | integrità / apply / verify | un digest o la rilettura non coincidono, il numero di righe ripristinate non coincide con quello contato sulla sorgente, oppure l'applicazione è fallita | **il ripristino non è utilizzabile**: indagare e rieseguire da zero |
| `6` | sorgente modificata | l'impronta della sorgente è cambiata fra l'inizio e la fine | fermare le scritture sulla sorgente (o usare uno snapshot) e ripetere |
| `7` | trasporto / connessione | coordinatore irraggiungibile, segreto errato, TLS fallito, peer assente | verificare rete, `--to`, `--channel`, segreto e certificati |

In una sessione con più target falliti vale il codice **più grave**, in
quest'ordine: `6` → `5` → `3` → `4` → `7` → `2` → `1`.

## Come si riconosce un'esecuzione riuscita

Non basta l'assenza di errori: devono comparire **entrambe** queste righe, con
lo stesso BLAKE3, una per peer, e l'ultima riga di avanzamento deve dire
`status="verified"` al `100.0%`.

```text
BACKUP VERIFIED: source unchanged; destination read-back matches
RESTORE VERIFIED: persisted destination matches source
items 2/2  97.66 KiB/97.66 KiB  (100.0%)  8.86 KiB/s  status="verified"
```

Il programma non stampa mai quelle righe per una copia parziale. Una conferma di
completamento priva della prova di rilettura viene rifiutata.

Per PostgreSQL la destinazione aggiunge sotto quella riga ciò che ha verificato
— `rows verified: …`, `constraints: …` e un `deviation: …` per ogni scostamento
dichiarato. Sono diagnostica: non cambiano il codice di uscita, ma un conteggio
di righe che non torna lo fa diventare `5`. Il dettaglio delle righe è in
[03-postgres.md](03-postgres.md).

## Messaggi frequenti e loro causa

| Messaggio | Codice | Causa |
|-----------|--------|-------|
| `--to is required (or set it in --config)` | `2` | manca l'indirizzo del coordinatore |
| `--channel is required (or set it in --config)` | `2` | manca il canale di rendezvous |
| `--secret and --secret-file are mutually exclusive` | `2` | passati entrambi |
| `secret file … is empty` | `2` | il file del segreto esiste ma è vuoto |
| `carriers=N is outside the supported range 1..=32` | `2` | valore di `--carriers` fuori intervallo |
| `unknown module '…'` | `2` | nome di modulo errato nel file di sessione |
| `YAML parse: …` | `2` | errore di sintassi o tipo nel file `--config` |
| `run --config does not start server:` | `2` | sezione `server:` in un file passato a `run` |
| `destination root must be absent or empty` | `3` | la root del filesystem contiene già qualcosa |
| `exact uid/gid restore requires root or CAP_CHOWN` | `3` | destinazione filesystem non privilegiata: `sudo` o `--no-preserve-ownership` |
| `destination prefix contains stale objects not in the source` | `3` | prefisso S3 sporco: `--overwrite` o prefisso nuovo |
| `filesystem follow_symlinks and preserve_xattr are not supported` | `7` | opzioni non supportate da questa build |
| `interrupted by SIGINT/SIGTERM` | `1` | esecuzione interrotta: l'elemento attivo è stato rimosso, nulla è certificato |

## Diagnostica passo per passo

1. **Il coordinatore risponde?**
   ```bash
   nc -z coordinatore.example 7835 && echo ok
   ```
2. **I due lati usano lo stesso canale e lo stesso segreto?** Canali diversi non
   si incontrano mai e il comando resta in attesa fino al timeout del piano
   (`RUST_BACKUP_PLAN_TIMEOUT`, default 600 s).
3. **Il canale è occupato da un'esecuzione precedente?** Dopo un crash la voce
   viene liberata entro circa 60 secondi (heartbeat 20 s, reaper 60 s).
4. **La sorgente da sola funziona?**
   ```bash
   rust-backup plan <modulo> [PARAMETRI]
   ```
   Se fallisce qui, il problema è di credenziali, privilegi o fedeltà — non di
   rete.
5. **Alzare la verbosità.**
   ```bash
   RUST_LOG=rust_backup=debug,rb_core=debug,rb_transport=debug rust-backup …
   ```
6. **Isolare il trasporto.** Con `--no-udp` su entrambi i lati si esclude il
   percorso diretto QUIC; se il problema sparisce, è la rete UDP.
7. **Trasferimento lento.** Controllare i carrier concordati nel log
   (`negotiated data plane carriers=N`): solo `filesystem` supera 1. Verificare
   che non ci sia un `--max-rate` attivo e ricordare che, se il percorso diretto
   non si stabilisce, tutto il traffico passa dal relay.
8. **Attese lunghissime alla fine.** La destinazione sta rileggendo l'intero
   backend (`destination read-back verification started`). Su dataset grandi può
   servire alzare `RUST_BACKUP_VERIFY_TIMEOUT`.

## Uso negli script

```bash
#!/usr/bin/env bash
set -euo pipefail

if rust-backup run --config /etc/rust-backup/sessione.yml; then
  logger -t backup "backup verificato"
else
  codice=$?
  case "$codice" in
    6) logger -t backup "SORGENTE MODIFICATA durante il backup" ;;
    5) logger -t backup "RIPRISTINO NON VALIDO: rilettura non coincidente" ;;
    3) logger -t backup "preflight fallito: destinazione non pronta" ;;
    *) logger -t backup "backup fallito (exit $codice)" ;;
  esac
  exit "$codice"
fi
```

Regola d'oro: **considerare valido solo l'exit code `0`**. Ogni altro valore
significa che non esiste una copia certificata, indipendentemente da quanti dati
sono arrivati a destinazione.
