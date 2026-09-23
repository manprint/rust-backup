# 03 — PostgreSQL

Copia **logica** di un cluster PostgreSQL da una sorgente in sola lettura a una
destinazione, con driver Rust puro: `pg_dump` e `pg_restore` non vengono usati e
non devono essere installati. Versioni supportate: **PostgreSQL 10 e superiori**
(il minimo è imposto dal codice; la matrice di CI copre 10–18, comprese le
coppie cross-major come 10→18 o 14→17).

## Caso minimal

Server di coordinamento già avviato, niente TLS, niente segreto:

```bash
# sorgente (utente in SOLA LETTURA)
RUST_BACKUP_PASSWORD='…' rust-backup postgres source \
  --to 127.0.0.1:7835 --channel pg \
  --host pg-sorgente --user backup_readonly

# destinazione (utente amministrativo)
RUST_BACKUP_PASSWORD='…' rust-backup postgres destination \
  --to 127.0.0.1:7835 --channel pg \
  --host pg-destinazione --user postgres --admin --yes
```

Senza `--database` viene copiato **l'intero cluster**: tutti i database
connettibili e non-template, più i ruoli, i privilegi e le proprietà. Senza
`--port` si usa 5432, senza `--sslmode` si usa `prefer`.

## Caso tipico in produzione

```bash
# sorgente
RUST_BACKUP_PASSWORD="$SOURCE_PG_PASSWORD" rust-backup postgres source \
  --to https://coordinatore.example:7835 --channel pg-prod \
  --secret-file /run/secrets/rb \
  --host pg-source.internal --port 5432 --user backup_readonly \
  --database app --sslmode require

# destinazione
RUST_BACKUP_PASSWORD="$DEST_PG_PASSWORD" rust-backup postgres destination \
  --to https://coordinatore.example:7835 --channel pg-prod \
  --secret-file /run/secrets/rb \
  --host pg-destination.internal --port 5432 --user postgres \
  --database postgres --sslmode require --admin --yes
```

## Parametri del modulo

| Flag | Variabile | Default | Lato | A cosa serve |
|------|-----------|---------|------|--------------|
| `--host <host>` | `RUST_BACKUP_HOST` | — (obbligatorio) | entrambi | host PostgreSQL |
| `--port <n>` | `RUST_BACKUP_PORT` | `5432` | entrambi | porta |
| `--user <nome>` | `RUST_BACKUP_USER` | — (obbligatorio) | entrambi | utente di connessione |
| `--password <pw>` | `RUST_BACKUP_PASSWORD` | nessuna | entrambi | password. **Usare la variabile d'ambiente**, non il flag |
| `--database <nome>` | `RUST_BACKUP_DATABASE` | nessuno | entrambi | sorgente: limita la copia a quel database (se assente: tutto il cluster). È anche il database di aggancio per l'introspezione globale; in mancanza si usa `postgres` |
| `--sslmode <modo>` | `RUST_BACKUP_SSLMODE` | `prefer` | entrambi | `disable`, `allow`, `prefer`, `require`, `verify-ca`, `verify-full` |
| `--admin[=bool]` | `RUST_BACKUP_ADMIN` | falso | destinazione | dichiara che la connessione è amministrativa: serve per creare ruoli, database e applicare le proprietà |
| `--overwrite[=bool]` | `RUST_BACKUP_OVERWRITE` | falso | destinazione | autorizza il ripristino sopra database già esistenti. Senza, il preflight fallisce |
| `--hot-backup[=bool]` | `RUST_BACKUP_HOT_BACKUP` | falso | entrambi | backup **a caldo** di una sorgente che resta in linea: la sorgente legge ogni database da un solo snapshot e non viene sottoposta all'audit di immutabilità; la destinazione deve passarlo anch'essa per accettare il piano. Vedi [Backup a caldo](#backup-a-caldo---hot-backup) |
| `--extension-version <source\|default>` | `RUST_BACKUP_EXTENSION_VERSION` | `source` | destinazione | quale versione installare per ogni estensione della sorgente. `source`: esattamente quella della sorgente, e il preflight rifiuta il piano se la destinazione non ce l'ha. `default`: la versione predefinita della destinazione, registrata come `deviation:` nella riga `RESTORE VERIFIED` |
| `-P sslrootcert=<file.pem>` | — | nessuno | entrambi | CA privata per `require`/`verify-ca`/`verify-full` |
| `-P allow_unsupported_objects=true` | — | falso | sorgente | accetta una copia *consapevolmente parziale* di un cluster che contiene oggetti non riproducibili (vedi sotto) |

Valgono inoltre tutti i flag di trasporto di [02 — Trasporto](02-trasporto.md).
`--carriers` viene sempre ridotto a **1**: un `COPY` è legato a una connessione.

## Cosa viene copiato

Ruoli e appartenenze, database (con encoding/collation/locale provider),
schemi, tabelle (dati inclusi, via `COPY` binario), vincoli, indici, sequenze
con il loro valore corrente, estensioni, proprietà e privilegi. L'ordine di
ripristino è derivato dalle dipendenze, non da una lista fissa.

Le tabelle di configurazione delle estensioni (quelle registrate con
`pg_extension_config_dump`, per esempio `spatial_ref_sys` di PostGIS) sono
copiate come i dati che sono: l'estensione viene ricreata con `CREATE
EXTENSION`, poi la destinazione svuota esattamente l'ambito registrato
dall'estensione (`DELETE` con la sua condizione, `TRUNCATE` se non ne ha
registrata una) e vi carica le righe della sorgente. Le righe inserite da
`CREATE EXTENSION` vengono quindi sostituite, non sommate a quelle di origine.

Durante l'analisi la sorgente conta le righe di ogni elemento del piano e di
ogni vista materializzata popolata. La destinazione confronta quei numeri con
quelli che ha davvero scritto (`COPY`) o prodotto (`REFRESH`): una differenza è
un errore di integrità e il ripristino fallisce. I digest BLAKE3 dimostrano che
i byte arrivati sono quelli inviati; il conteggio dimostra che non ne sono
rimasti indietro.

Le **password dei ruoli non vengono mai lette né ripristinate**: dopo il restore i
ruoli esistono ma vanno riconfigurati con le credenziali.

Il contratto completo, con il dettaglio di partizionamento, ereditarietà,
`reg*`, ACL e determinismo della rilettura, è in
[docs/modules/POSTGRES.md](../modules/POSTGRES.md).

## Lock, timeout e connessioni

La sorgente apre **una connessione al database bootstrap più una per ogni
database copiato**, riusate da tutte le fasi (impronta, analisi, conteggi,
`COPY`).

Le sessioni di sorgente hanno `lock_timeout=30s`: se qualcun altro tiene una
tabella in `ACCESS EXCLUSIVE`, la copia fallisce entro trenta secondi con il
messaggio del server (`canceling statement due to lock timeout`) e il nome
della relazione, invece di restare in attesa. Non c'è invece alcun
`statement_timeout`: una `COPY` di una tabella grande dura legittimamente a
lungo. Sulla destinazione nessuno dei due è impostato, perché DDL e `REFRESH
MATERIALIZED VIEW` prendono lock e tempo di proposito.

Rimedio: rilasciare il lock (o attendere la fine della manutenzione) e
rieseguire; la copia riparte da zero, nulla è stato scritto sulla destinazione.

## Cosa stampa la verifica

Alla fine la destinazione stampa la riga formale di verifica e, solo per
PostgreSQL, le righe che dicono **che cosa** ha dimostrato:

```text
RESTORE VERIFIED: persisted destination matches source  items=51 bytes=212265935 blake3=0a91dd66…
rows verified: 103590 from tables, 1137 from materialized views, 104727 rows
constraints: 32 (validated 30, not valid 2)
deviation: extension rbtest restored at version 1.1 (source 1.0)
```

- `rows verified:` — righe scritte con `COPY` (tabelle e tabelle di
  configurazione delle estensioni), righe prodotte da `REFRESH MATERIALIZED
  VIEW` e totale. Sono le stesse righe contate sulla sorgente durante l'analisi:
  se non coincidono il ripristino fallisce con codice `5` invece di certificare
  una copia parziale.
- `constraints:` — vincoli riletti dal catalogo della destinazione e confrontati
  con quelli della sorgente; `not valid` conta quelli `NOT VALID`, che restano
  tali (una destinazione che li avesse validati avrebbe dovuto rifiutare righe
  che la sorgente contiene legittimamente).
- `deviation:` — compare solo quando qualcosa è stato ricostruito in modo
  dichiaratamente non identico; oggi solo con `--extension-version default`.

Le stesse informazioni escono anche come campi strutturati dell'evento
`PostgreSQL destination read-back verified`, per chi raccoglie i log.

## Backup a caldo (`--hot-backup`)

Di norma la sorgente deve restare **ferma** per tutta la copia: all'inizio e
alla fine calcola un'impronta del cluster (catalogo, stato delle sequenze e un
hash di ogni riga) e, se le due differiscono, fallisce con codice `6`. Con
un'applicazione in linea, per esempio Odoo, le scritture non si fermano mai
(cron, code, sessioni, sequenze) e la copia fallisce sempre.

`--hot-backup` copia una sorgente che resta in linea:

```bash
# sorgente: l'applicazione continua a lavorare
RUST_BACKUP_PASSWORD='…' rust-backup postgres source \
  --to coordinatore:7835 --channel pg --secret-file ./secret.txt \
  --host pg-sorgente --user backup_readonly --database app --hot-backup

# destinazione: anche qui serve --hot-backup, altrimenti il piano è rifiutato
RUST_BACKUP_PASSWORD='…' rust-backup postgres destination \
  --to coordinatore:7835 --channel pg --secret-file ./secret.txt \
  --host pg-destinazione --user postgres --admin --yes --hot-backup
```

**Cosa fa.** Ogni connessione della sorgente apre una sola transazione
`REPEATABLE READ, READ ONLY` prima della prima lettura e la tiene fino alla fine
dell'esecuzione, come fa `pg_dump`. Catalogo, valori delle sequenze, conteggi
delle righe e ogni `COPY` vedono quindi **lo stesso istante** del database,
anche mentre l'applicazione scrive. Ne segue che:

- le righe ripristinate rispettano le foreign key (una tabella figlia non può
  essere stata letta dopo che la madre ha ricevuto righe nuove);
- nessuna sequenza riparte da un valore già usato da una riga copiata;
- la verifica della destinazione resta **severa come a freddo** (catalogo,
  conteggi, rilettura con BLAKE3), ma rispetto allo snapshot, non alla sorgente
  come sarà alla fine.

**Cosa si accetta passando il flag.**

- La sorgente **non** viene sottoposta all'audit di immutabilità: le scritture
  dell'applicazione durante la copia sono attese. Le protezioni del programma
  restano tutte attive (ruolo e sessione in sola lettura, allowlist delle
  istruzioni, transazione `READ ONLY`).
- La destinazione è la copia dello snapshot preso all'inizio dell'analisi: ciò
  che l'applicazione scrive dopo quell'istante **non** è nel backup.
- Senza `--database` (intero cluster) lo snapshot è **per database**:
  PostgreSQL non permette di condividerne uno fra database diversi. Ogni
  database è coerente al suo interno, non rispetto agli altri (come
  `pg_dumpall`).

**Effetti sulla sorgente durante la copia.**

- La transazione aperta trattiene il vacuum: le versioni morte delle righe
  prodotte nel frattempo non vengono ripulite finché il backup non termina.
  Per un backup di qualche ora è normale; su un database con moltissime
  scritture conviene farlo nelle ore meno cariche.
- La transazione tiene un lock `ACCESS SHARE` su ogni tabella già letta fino
  alla fine: un `ALTER TABLE`, `DROP`, `TRUNCATE`, `VACUUM FULL` o `REINDEX` su
  quelle tabelle **attende** la fine del backup, e le query che arrivano dopo di
  lui su quella tabella attendono a loro volta. Non aggiornare i moduli
  dell'applicazione (per Odoo: `-u`/installazioni) durante un backup a caldo.
- Se la connessione che tiene lo snapshot cade, la copia si ferma con
  `hot backup: the connection holding the snapshot of database '…' was lost;
  restart the backup` invece di proseguire su uno snapshot diverso.

**Cosa stampa.** Al posto delle righe consuete compaiono:

```text
WARN hot backup: the source stays online and is not audited for immutability; the destination is verified against the snapshot the source was read from
BACKUP VERIFIED (hot backup): destination read-back matches the source snapshot; the online source was not audited
RESTORE VERIFIED (hot backup): persisted destination matches the source snapshot
```

Il piano mostra `mode=HotSnapshot`. Una destinazione avviata senza
`--hot-backup` lo rifiuta in preflight (codice `3`) con `the source ran as a hot
backup (online, not audited for immutability); pass --hot-backup to the
destination to accept a copy of its snapshot`, prima di scrivere qualsiasi
cosa. Una build precedente a questa funzione non riconosce il piano e lo
rifiuta.

`--hot-backup` esiste solo per PostgreSQL: sugli altri moduli è un errore di
configurazione (codice `2`), perché nessuno di loro sa leggere uno snapshot
coerente di una sorgente in linea.

## Oggetti che fanno fallire l'analisi (di proposito)

L'analisi si **rifiuta** di procedere se il cluster contiene oggetti che questa
build non sa ricostruire: trigger, policy RLS, tipi definiti dall'utente,
aggregati, foreign table, large object, ACL di colonna o di default, opzioni di
vista, valori di colonne `reg*`, publication e subscription di replica logica,
event trigger, e identificatori che contengono un punto.

Il motivo è che quegli oggetti non entrerebbero nel piano, e la rilettura —
che confronta lo *stesso* modello sui due lati — non potrebbe accorgersi della
loro assenza: il risultato sarebbe una copia certificata "1:1" di un cluster che
ha silenziosamente perso, per esempio, tutte le policy RLS.

Per accettare comunque una copia parziale:

```bash
rust-backup postgres source … -P allow_unsupported_objects=true
```

L'esecuzione elenca allora nei log esattamente ciò che lascia indietro:

```text
WARN rb_postgres::introspect: allow_unsupported_objects is set: this backup is NOT a 1:1 copy unsupported=…
```

### Come leggere il messaggio

```text
[Analyze] this build cannot reproduce the following objects, and the catalog read-back cannot detect their absence either, so the run would report a verified copy of a cluster that had lost them: default privileges (ALTER DEFAULT PRIVILEGES) (2): synclinic.odoo:S, synclinic.odoo:r. Set allow_unsupported_objects=true to accept a knowingly partial copy.
```

- Ogni classe di oggetti compare come `<classe> (<totale>): <nome>, <nome>, …`,
  con al massimo cinque nomi seguiti da `and N more`. Classi diverse sono
  separate da `;`.
- Il controllo gira **un database alla volta** e si ferma al primo che contiene
  oggetti non supportati. Il messaggio non nomina il database: con `--database`
  è quello indicato; senza, dopo averlo risolto l'esecuzione successiva può
  fermarsi sul database seguente.
- Il rifiuto avviene in analisi, prima di qualunque trasferimento: esce con
  codice `1` e la destinazione non riceve nulla.
- Lo stesso rifiuto compare con `rust-backup plan postgres …`: è il modo più
  rapido per controllare un cluster senza avviare la destinazione.

### Caso frequente: `ALTER DEFAULT PRIVILEGES`

Le regole `ALTER DEFAULT PRIVILEGES` non riguardano gli oggetti che esistono
già: stabiliscono quali `GRANT` riceveranno in automatico le tabelle, le
sequenze o le funzioni che un ruolo **creerà in futuro**. Servono tipicamente a
dare a un secondo ruolo (reporting, sola lettura) l'accesso a ciò che
l'applicazione crea da sé. Questa build non le copia.

Ogni regola compare nel messaggio come `schema.ruolo:tipo`, senza `schema.`
quando la regola vale per tutti gli schemi del ruolo (`ALTER DEFAULT
PRIVILEGES FOR ROLE … GRANT …` senza `IN SCHEMA`). Il tipo è la lettera di
`pg_default_acl.defaclobjtype`:

| Lettera | Oggetti futuri |
|---------|----------------|
| `r` | tabelle e viste |
| `S` | sequenze |
| `f` | funzioni e procedure |
| `T` | tipi |
| `n` | schemi |

Nell'esempio sopra, `synclinic.odoo:r` e `synclinic.odoo:S` sono due regole
sulle tabelle e sulle sequenze che il ruolo `odoo` creerà nello schema
`synclinic`.

**Cosa si perde davvero.** I privilegi già concessi su tabelle e sequenze
esistenti vengono copiati come tutti gli altri, insieme ai dati e alle
proprietà. Si perde solo l'automatismo: sulla destinazione, un oggetto creato
dopo il restore (per esempio una tabella aggiunta da un aggiornamento
dell'applicazione) non riceve i `GRANT` che avrebbe ricevuto sulla sorgente.

**Come procedere.**

1. Leggere le regole sulla sorgente, collegati al database indicato
   (`pg_default_acl` è per database):

   ```sql
   \ddp
   -- oppure, senza psql:
   SELECT pg_get_userbyid(d.defaclrole) AS ruolo,
          n.nspname                     AS schema,
          d.defaclobjtype               AS tipo,
          d.defaclacl                   AS privilegi
   FROM pg_default_acl d
   LEFT JOIN pg_namespace n ON n.oid = d.defaclnamespace;
   ```

   `privilegi` è nel formato ACL di PostgreSQL: `{reporting=r/odoo}` significa
   "`reporting` riceve `SELECT`, concesso da `odoo`". Le lettere più comuni:
   `r` = `SELECT`, `a` = `INSERT`, `w` = `UPDATE`, `d` = `DELETE`,
   `D` = `TRUNCATE`, `x` = `REFERENCES`, `t` = `TRIGGER`, `U` = `USAGE`,
   `X` = `EXECUTE`, `C` = `CREATE`.

2. Eseguire il backup accettando la copia parziale:

   ```bash
   rust-backup postgres source … -P allow_unsupported_objects=true
   ```

3. Dopo il restore, ricreare le stesse regole sulla destinazione, collegati allo
   stesso database, con un superutente o un membro del ruolo proprietario della
   regola. Per l'esempio sopra, se `\ddp` avesse mostrato `{reporting=r/odoo}`
   su tabelle e `{reporting=rU/odoo}` su sequenze:

   ```sql
   ALTER DEFAULT PRIVILEGES FOR ROLE odoo IN SCHEMA synclinic
     GRANT SELECT ON TABLES TO reporting;
   ALTER DEFAULT PRIVILEGES FOR ROLE odoo IN SCHEMA synclinic
     GRANT SELECT, USAGE ON SEQUENCES TO reporting;
   ```

   Poi controllare che `\ddp` mostri sulla destinazione le stesse righe della
   sorgente. La verifica finale del programma copre ciò che ha copiato, non
   queste regole: ricrearle spetta all'operatore.

Se le regole sono obsolete, revocarle sulla sorgente (`ALTER DEFAULT
PRIVILEGES … REVOKE …`) fa passare l'analisi senza l'opzione. È però una
modifica al database di produzione, che il programma non fa mai da sé: la
decisione resta all'operatore.

## Privilegi

**Sorgente — utente in sola lettura.** Deve poter leggere i cataloghi e fare
`SELECT` sulle tabelle copiate **e sulle sequenze**: `pg_sequences.last_value` è
`NULL` sia per una sequenza mai usata sia per una che il ruolo non può leggere, e
`GRANT SELECT ON ALL TABLES` non copre le sequenze. Senza quel privilegio
l'analisi fallisce indicando la sequenza e il grant mancante.

```sql
CREATE ROLE backup_readonly LOGIN PASSWORD '…';
GRANT pg_read_all_data TO backup_readonly;          -- PostgreSQL 14+
-- Fino alla 13, per ogni database e schema coinvolti:
--   GRANT CONNECT ON DATABASE app TO backup_readonly;
--   GRANT USAGE ON SCHEMA public TO backup_readonly;
--   GRANT SELECT ON ALL TABLES    IN SCHEMA public TO backup_readonly;
--   GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO backup_readonly;
```

Se il ruolo usato sulla sorgente **può scrivere** (superutente, `CREATEDB`,
`CREATEROLE`, oppure un `GRANT INSERT`/`UPDATE`/`DELETE`/`TRUNCATE` su qualche
tabella), la corsa non viene rifiutata ma stampa una volta:

```text
WARN source role app_owner can write to the source; a read-only role is recommended, see docs/IMMUTABILITY.md
```

La sorgente resta comunque protetta dal lato del programma — il codice sorgente
non compila una scrittura, ogni statement passa da una allowlist e la sessione
è aperta con `default_transaction_read_only=on` — ma un ruolo a soli privilegi
di lettura è la difesa che non dipende da questo programma.

**Destinazione — utente amministrativo.** Crea ruoli e database, applica
proprietà e privilegi: in pratica `CREATEROLE` + `CREATEDB`, e superutente se si
devono ripristinare ACL a livello di database.

## `--overwrite`: cosa distrugge e quando

Con `--overwrite` la destinazione, **prima** che arrivi il primo byte di
payload, blocca le nuove connessioni ai database di destinazione, termina le
sessioni attive e li ricrea da zero.

Conseguenze da mettere in conto:

- non è un "riprova senza rischi": il contenuto precedente è già stato eliminato
  quando il trasferimento comincia;
- se il ripristino fallisce a metà, il modulo rimuove anche ciò che aveva creato,
  quindi il database resta **assente**, non a metà;
- i **ruoli** creati da un'esecuzione fallita restano: vanno eliminati a mano se
  il cluster deve tornare esattamente allo stato precedente.

Se il contenuto precedente deve sopravvivere a un tentativo fallito: fare prima
uno snapshot proprio, oppure ripristinare su un nome di database nuovo e
commutare dopo.

## TLS verso PostgreSQL

`require`, `verify-ca` e `verify-full` usano rustls e verificano **sempre** sia
la catena sia il nome host: si comportano tutti e tre come `verify-full`. È più
severo di libpq (dove `require` cifra senza verificare e `verify-ca` non
controlla il nome): un certificato self-signed o con hostname sbagliato viene
rifiutato in tutte e tre le modalità. Fallisce chiuso, mai aperto.

Con una CA privata:

```bash
rust-backup postgres source … --sslmode verify-full -P sslrootcert=/run/secrets/postgres-ca.pem
```

## Anteprima senza trasferire nulla

```bash
RUST_BACKUP_PASSWORD='…' rust-backup plan postgres \
  --host pg-sorgente --user backup_readonly --database app
```

Apre la sorgente in sola lettura, stampa il piano su stdout e termina: nessun
trasporto, nessuna destinazione. Vedi [08 — Dry-run `plan`](08-plan.md).

## In container

```bash
docker run --rm --network host \
  -e RUST_BACKUP_PASSWORD="$SOURCE_PG_PASSWORD" \
  ghcr.io/manprint/rust-backup:latest postgres source \
  --to 127.0.0.1:7835 --channel pg-prod --secret-file /run/secrets/rb \
  --host 127.0.0.1 --port 5432 --user backup_readonly --database app

docker run --rm --network host \
  -e RUST_BACKUP_PASSWORD="$DEST_PG_PASSWORD" \
  ghcr.io/manprint/rust-backup:latest postgres destination \
  --to 127.0.0.1:7835 --channel pg-prod --secret-file /run/secrets/rb \
  --host 127.0.0.1 --port 55432 --user postgres --admin --yes
```

Il segreto va montato nel container (`-v /etc/rust-backup/coordination.secret:/run/secrets/rb:ro`);
vedi [09 — Docker e Compose](09-docker.md).

## In sessione YAML

```yaml
parallel_targets: 2
fail_fast: true
targets:
  - module: postgres
    role: source
    transport: &trasporto
      to: coordinatore.example:7835
      channel: pg-prod
      secret: CAMBIAMI
    params:
      host: pg-source.internal
      port: 5432
      user: backup_readonly
      password: CAMBIAMI
      database: app
      sslmode: require

  - module: postgres
    role: destination
    transport: *trasporto
    params:
      host: pg-destination.internal
      user: postgres
      password: CAMBIAMI
      sslmode: require
      admin: true
      overwrite: false
    auto_accept: true
```

Modello pronto: [examples/postgres-session.yml](../../examples/postgres-session.yml).
Schema completo in [07 — Sessioni YAML](07-sessioni-yaml.md).

## Errori frequenti

| Sintomo | Causa e rimedio |
|---------|-----------------|
| exit `2`, `configuration error` | manca `--host`/`--user` o un parametro non è del tipo atteso (attenzione ai numeri passati con `-P`: usare `-P 'password="123456"'`) |
| exit `3`, preflight: database già presente | serve `--overwrite`, oppure ripristinare su un cluster/nome pulito |
| exit `3`, privilegi insufficienti | l'utente di destinazione non è amministrativo: aggiungere `--admin` e i diritti `CREATEROLE`/`CREATEDB` |
| analisi rifiutata per una sequenza | manca `SELECT`/`USAGE` sulla sequenza indicata sul lato sorgente |
| analisi rifiutata per oggetti non supportati | vedi la sezione dedicata: valutare `-P allow_unsupported_objects=true` sapendo cosa si perde |
| analisi rifiutata per `default privileges (ALTER DEFAULT PRIVILEGES)` | regole sui privilegi degli oggetti *futuri*, quelli esistenti sono copiati: vedi [Caso frequente: `ALTER DEFAULT PRIVILEGES`](#caso-frequente-alter-default-privileges) |
| exit `6` | la sorgente è cambiata durante l'esecuzione: fermare la scrittura sul cluster di origine e ripetere, oppure — se la sorgente deve restare in linea — usare [`--hot-backup`](#backup-a-caldo---hot-backup) su entrambi i lati |
| exit `3`, `pass --hot-backup to the destination` | la sorgente ha fatto un backup a caldo: aggiungere `--hot-backup` anche alla destinazione |
| exit `5` | la rilettura della destinazione non coincide: **il ripristino non è valido**, non usarlo |
| exit `5`, `views[…].definition differs at character N` | la vista restaurata non si rende come la destinazione rende il testo della sorgente (ogni definizione viene ridisegnata sulla destinazione con una vista temporanea prima del confronto); il messaggio mostra le due parti attorno al primo carattere diverso e una riga `note:` elenca, con il motivo, ogni vista che non è stato possibile ridisegnare. Fino alla 0.0.12 il confronto era alla lettera sulla stessa major e falliva sulle viste di report di Odoo (`IN` su colonne `varchar`) |

Tabella completa dei codici: [11 — Codici di uscita](11-codici-uscita.md).
