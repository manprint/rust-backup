# Phase 6 — Documentation audit and parity gate

> **Intent:** every documentation table that describes a flag, environment variable, default,
> side or check is verified row by row against a generated inventory; misleading descriptions
> are corrected; every table carries the matching environment variable; the module docs carry
> the Limits and guardrail sections produced by phases 1-5; a parity script in the gates
> prevents drift.
> **Shippable alone?** yes — documentation and a gate script.
> **Preconditions:** phase_06 DONE (all behaviour changes of this plan have landed; the audit documents the final state).

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`,
   finish or revert that unit first (§6 says how far it got). Run the gate
   commands in STATE.md **§3** and check the result against what §1, §7, and §11
   claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type:
   sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to
   `claimed — nothing written yet`.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to
   `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1
   at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has
   WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A
   sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is
   half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP
   commits are on.

Shared facts for this phase (recon 2026-09-16; line numbers may have shifted by phases 1-5, re-locate with `grep -n '^|' <file>`):
- Inventory source: `bash scripts/env_inventory.sh` (§ 0.3) — TSV `command / flag / env / default / help` plus direct `env::var` reads. Precedence: CLI > env > YAML > default (`crates/rb-core/src/config.rs:1-6`, `crates/rust-backup/src/main.rs:13-16`).
- Tables to audit (path:line — header — rows — env column): `docs/usage/02-trasporto.md:21` Flag/Variabile/Default/A cosa serve (14, yes); `docs/usage/03-postgres.md:49` Flag/Variabile/Default/Lato/A cosa serve (10, yes); `docs/usage/04-mongodb.md:38` (9, yes); `docs/usage/05-filesystem.md:40` (5, yes), `:60` Attributo/Sorgente/Destinazione/Privilegi (6, no), `:109` Controllo/Significato (4, no), `:189` Sintomo/Causa e rimedio (5, no); `docs/usage/06-s3.md:51` (9, yes); `docs/usage/07-sessioni-yaml.md:47` (4, yes); `docs/usage/08-plan.md:30` Flag/Variabile/A cosa serve (3, yes); `docs/usage/09-docker.md:23` Caratteristica/Valore (7, no); `docs/usage/10-variabili-ambiente.md:27` (10), `:42` (8), `:55` (3), `:64` Variabile/Flag equivalente/Moduli che la usano (20), `:105` Variabile/Default/Significato (5) — all env; `docs/usage/11-codici-uscita.md:12` Codice/Nome/Significato/Cosa fare (8, no); `docs/modules/FILESYSTEM.md:9` Attribute/Source/Destination/Privilege (6, no); `docs/modules/README.md:7` Module/Crate/Versions/Driver/Status (4, no); `README.md:136` Variable/Default/Effect (8, yes). `docs/modules/POSTGRES.md`, `MONGODB.md`, `S3.md`, `docs/TRANSPORT.md`, `docs/QA_GUIDE.md` contain no tables.
- Known counts: 40 clap `env=` variables in `crates/rust-backup/src/main.rs` (recon list in `STATE.md` §2) plus `RUST_BACKUP_EXTENSION_VERSION` and `RUST_BACKUP_ALLOW_ATIME_UPDATES` from this plan; direct reads `RUST_BACKUP_PLAN_TIMEOUT` (600 s), `RUST_BACKUP_VERIFY_TIMEOUT` (86400 s), `RUST_BACKUP_STUN_SERVERS`/`RUST_BACKUP_STUN_SERVER`, `BORE_PROXY_BUFFER_SIZE` (256 KiB), `RUST_LOG`, test-only `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART`, `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA`. The `:64` table with 20 rows is therefore incomplete against 42 clap variables — expected finding.
- Existing gate scripts: `scripts/help_parity.sh` (help text parity), `scripts/gates.sh`.
- Languages: Italian in `docs/usage/`, English elsewhere (D8). No emojis, no informal language.

---

## Sub-phases

### 6.1 `scripts/docs_parity.sh`
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — tooling; self-review.
- **Files:** new `scripts/docs_parity.sh`, new `scripts/docs_parity_allow.txt` (allowlist of test-only variables: `RUST_BACKUP_S3_TEST_FAIL_AFTER_PART`, `RUST_BACKUP_PG_TEST_EXPECTED_ROWS_DELTA`, with a comment that they are documented as unsupported in `docs/usage/10-variabili-ambiente.md`).
- **Change:** the script runs `scripts/env_inventory.sh` (or reads a TSV passed as `$1`) and checks: (1) every inventory `RUST_BACKUP_*`/`BORE_*` name appears in a table row of `docs/usage/10-variabili-ambiente.md` (any table) unless allowlisted; (2) for each module M in `postgres mongodb filesystem s3`, every inventory flag whose command is `M source` or `M destination` appears in the flag table of `docs/usage/0N-M.md` (`03`, `04`, `05`, `06`); transport flags (`--to`, `--channel`, `--secret*`, `--carriers`, `--udp`, `--insecure`, `--max-rate`, `--yes`) in `02-trasporto.md`; `server` flags in `01-server.md`; `run` flags in `07-sessioni-yaml.md`; `plan` flags in `08-plan.md`; (3) in every flag table row that carries a `Variabile`/`Variable` cell and a `Default` cell, the env name equals the inventory env for that flag and the default equals the inventory `[default: X]` when the cell is not `—` or empty (normalize backticks and whitespace); (4) every documented `RUST_BACKUP_*` name in any `docs/**/*.md` or `README.md` exists in the inventory (catches renamed or removed variables). Print one `MISMATCH <file>:<line> <what>` per finding, exit 1 on any. Table parsing: lines starting with `|`, header row detection by the column names above (Italian and English variants).
- **Unit tests:** none (shell). Self-check: `--selftest` mode copies `docs/usage/10-variabili-ambiente.md` to a temp file, removes one variable row, runs the check against a temp docs root and asserts a `MISMATCH`.
- **e2e tests:** T-DOCS-PARITY (first run is expected to fail; the failures are the input list for § 6.2 and are recorded in `STATE.md` §9).
- **Done:** script runs, ShellCheck clean, `--selftest` exits 0, the current mismatch list is recorded in `STATE.md` §9; `bash scripts/gates.sh` green (parity not yet wired); closed in `STATE.md` (§1 -> 6.2).

### 6.2 Row-by-row audit and correction of every table
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation review; self-review.
- **Files:** every table listed in the shared facts; `README.md:136` table; `docs/usage/10-variabili-ambiente.md` (complete the `:64` table to every clap variable, grouped as today).
- **Change:** for each table row: (a) fix every `MISMATCH` from § 6.1; (b) compare the description cell (`A cosa serve` / `Effect` / `Significato`) with the clap `help` text in the inventory and with the behaviour implemented (open the anchor for the flag in `crates/rust-backup/src/main.rs` and the consuming code when the help is terse); rewrite descriptions that state a wrong unit, a wrong default, a wrong side, a wrong precedence, or a behaviour that does not exist; (c) `Lato` must be `sorgente`, `destinazione` or `entrambi` exactly as clap accepts the flag (destination-only flags: `--admin`, `--overwrite`, `--extension-version`; source-only: `--allow-atime-updates`, `--follow-symlinks`, `--preserve-xattr`); (d) tables without an env column but describing flags or variables gain one (recon found none missing, verify again after phases 1-5); (e) `docs/usage/11-codici-uscita.md` rows checked against the exit-code mapping in `main.rs` (recon: codes 0-7, severity order 6 > 5 > 3 > 4 > 7 > 2 > 1); (f) `docs/usage/09-docker.md:23` values checked against `compose.yml` and the Dockerfile; (g) `docs/modules/README.md:7` versions column checked against `version_support()` of each module. Record each corrected cell as one line `file:line — was — now` in `STATE.md` §4 `What changed` (compress to counts per file if more than 20). Language per D8.
- **Unit tests:** none.
- **e2e tests:** T-DOCS-PARITY — `bash scripts/docs_parity.sh` exits 0 with zero `MISMATCH` lines.
- **Done:** T-DOCS-PARITY passes; every table reviewed (list the 20 tables with a check mark in `STATE.md` §7 notes); `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 6.3).

### 6.3 Module docs, QA guide and harness docs reflect phases 1-5
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; self-review.
- **Files:** `docs/modules/POSTGRES.md`, `docs/modules/FILESYSTEM.md`, `docs/modules/MONGODB.md`, `docs/modules/S3.md`, `docs/IMMUTABILITY.md`, `docs/testing/POSTGRES_MATRIX.md`, `docs/testing/FILESYSTEM_MATRIX.md`, `docs/QA_GUIDE.md`, `e2e/README.md`, `docs/plans/RESUME.md` (status pointer only if it lists module capabilities).
- **Change:** read each file end to end and reconcile with the shipped behaviour: POSTGRES.md Limits (removed `pgsql_tmp` bullet, added extension-version rule, constraint comments supported, config tables supported, refusal list matches `find_unsupported_objects`), "Runtime model and guardrails", "Verification" (row counts, constraint state, deviations); FILESYSTEM.md (special files, capability, TOCTOU guarantee, fingerprint cost, atime rule); MONGODB.md and S3.md (Immutability paragraphs, least-privilege pointers); IMMUTABILITY.md (no "evidence lands in" markers left; every test ID exists in a script or a test module — verify with `grep -rn "<ID>" e2e/ crates/`); matrix docs (`Fixture` and `Expected` columns final; no execution status); QA_GUIDE.md and e2e/README.md list every script and CI job added by this plan (`postgres-postgis`, `postgres-postgis-cross-version`, `postgres-large`, `filesystem_matrix.sh`, `postgres_large_table.sh`) and the gate scripts (`source_readonly_lint.sh`, `docs_parity.sh`, `env_inventory.sh`). Every `T-*` ID mentioned in docs must resolve to a real test.
- **Unit tests:** none.
- **e2e tests:** none. Self-check: `grep -rno "T-[A-Z0-9-]*" docs/ | sort -u` and each ID found in `e2e/` or `crates/`.
- **Done:** all files reconciled; the grep self-check finds no dangling ID; `bash scripts/gates.sh` green; closed in `STATE.md` (§1 -> 6.4).

### 6.4 Wire the docs parity gate
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — mechanical; self-review.
- **Files:** `scripts/gates.sh` (append `bash scripts/docs_parity.sh` after `scripts/help_parity.sh`), `CLAUDE.md` (gate line), `docs/QA_GUIDE.md` (gate list), `.github/workflows/ci.yml` only if `rust-quality` does not already run `gates.sh` end to end (recon: it does).
- **Change:** as listed. `docs_parity.sh` must not rebuild the binary when `gates.sh` already built it: accept `RUST_BACKUP_BIN=target/debug/rust-backup` from `gates.sh` (adjust `env_inventory.sh` to honour it, § 0.3 already specifies the variable).
- **Unit tests:** none.
- **e2e tests:** T-DOCS-PARITY inside `bash scripts/gates.sh`.
- **Done:** `bash scripts/gates.sh` green including the parity step; CI `rust-quality` green on the next run; closed in `STATE.md` (§1 -> 6.5).

### 6.5 Update README.md (final read)
- **Model:** `agent:opus`
- **Assignment:** `agent:opus` — documentation; final self-review of the whole README.
- **Files:** `README.md`.
- **Change:** final pass over the entire README against the shipped state of phases 1-5: install/requirements (Docker and root only for e2e; CAP_CHOWN/CAP_MKNOD for filesystem restores), the full flag surface for every module with realistic examples (postgres `--extension-version`, filesystem `--allow-atime-updates`), the configuration table at `README.md:136` complete against the inventory (every `RUST_BACKUP_*` a user may set, defaults; test-only hooks excluded), the verification block example, known limits (sparse dense, xattr/ACL, sockets, refused PostgreSQL kinds, extension versions), troubleshooting entries added by phases 1-5, and where to find more (`docs/usage/README.md`, `docs/IMMUTABILITY.md`, `docs/testing/`). Remove nothing that still holds; add nothing unshipped; no module, type, function or file names; preserve structure, tone, language.
- **Unit tests:** none (documentation).
- **e2e tests:** none — every README example was executed and produced the documented output; `bash scripts/docs_parity.sh` covers the variable table.
- **Done:** a new user can install, configure with least privilege, run every module and interpret verification output and failures from the README alone; `bash scripts/gates.sh` green; closed in `STATE.md` with the §11 docs row for phase 6 set and the plan status set to complete; §1 `Next action:` reads `plan complete — run /plan-execute-verify verify`.

---

## Phase gates

- **Fmt:** `cargo fmt --all --check`
- **Lint:** `cargo clippy --locked --all-targets --all-features -- -D warnings`, `bash scripts/source_readonly_lint.sh`, `bash scripts/docs_parity.sh`
- **Test subset:** `bash scripts/gates.sh`
- **Regression guard:** T-HARN-INV, T-DOCS-PARITY, T-E2E0
- **README:** final full read completed

## Phase done criterion
`bash scripts/docs_parity.sh` exits 0 inside `bash scripts/gates.sh`; every table in the
shared-facts list was reviewed and corrected; every `T-*` ID in `docs/` resolves to a test;
README.md reflects the complete shipped behavior of this plan, and `STATE.md` §11 shows this
phase `DONE` with every sub-phase closed.
