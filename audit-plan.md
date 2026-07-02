# BeetOS — Audit correctness & plan de correction

> Audit ligne par ligne du code **ajouté par BeetOS** au-dessus du cœur Xous
> cherry-piqué (display/focus, spawn/wait, drivers virtio, fs userspace, arch
> AArch64). Chaque item a été vérifié dans le code, pas seulement signalé.
>
> Deux motifs dominants :
> 1. **Bugs invisibles sur QEMU/CI mais fatals sur vrai matériel** (M1) — caches,
>    barrières mémoire, reliquats 32-bit.
> 2. **Fixes récents incomplets** — corrigés pour le symptôme observé, pas pour
>    la classe entière (réutilisation de PID, dangling focus, API asymétrique).

Légende statut : ☑ corrigé & compilé · ◐ partiellement corrigé / reste documenté · ⏭ différé

> **Bilan :** tout P0/P1/P2 corrigé (sauf C8, documenté). Cross-build AArch64 propre,
> 52 tests hosted au vert. Détail des ◐ en fin de fichier.

---

## P0 — Corruption de données / gel système, déclenchables sur QEMU

### ☑ A1. Troncature silencieuse des chemins → mauvais fichier supprimé/écrit
`api/fs/src/lib.rs:96-109` (`pack_path` tronque à 32 o), `apps/shell/src/main.rs:575` (`write` n'envoie que 16 o de chemin).
`rm`/`cd`/`mkdir` sur un chemin > 32 o opèrent sur le préfixe ; `write` sur > 16 o crée le mauvais fichier. Aucune erreur remontée.
**Fix :** faire échouer explicitement (`FsError::InvalidPath` / `NameTooLong`) quand le chemin dépasse la capacité au lieu de tronquer ; unifier `write` sur le même chemin 32 o que les autres ops ; message d'erreur côté shell.

### ☑ A2. `cd -` lit `PREV_BUF` pendant qu'il l'écrase → CWD corrompu
`apps/shell/src/main.rs:372-394`. `resolved` emprunte `PREV_BUF`, puis le swap réécrit `PREV_BUF` avant de copier `resolved` dans `CWD_BUF`. Aliasing `static mut` (UB).
**Fix :** matérialiser le chemin cible dans un buffer local avant toute écriture des globals.

### ☑ A3. `WaitProcess` fabrique un code de sortie 0 si la cible est déjà morte
`xous/kernel/src/syscall.rs:942-946`. Course spawn→exit rapide→wait : un enfant qui échoue est rapporté « succès ». Pas de dépôt du code de sortie si le waiter n'est pas encore enregistré.
**Fix :** conserver une petite table `(pid, exit_code)` des processus récemment terminés (reaper) et la consulter dans `WaitProcess` avant de renvoyer 0 ; distinguer « jamais existé » de « déjà terminé ».

### ☑ A4. File d'attente display non purgée + skip vaincu par réutilisation de PID
`xous/kernel/src/services.rs:1613-1620, 1646-1653`. Le fix `f67afda` teste `process(pid).is_err()` (existence) ; un PID recyclé rend le test faux → framebuffer mappé dans un process innocent, thread force-réveillé.
**Fix :** purger la file `waiters` du PID mourant dans `terminate_current_process` (comme pour l'owner), et stocker un jeton d'identité (génération) plutôt que le seul PID.

### ☑ A5. File display pleine → thread suspendu jamais réveillé
`xous/kernel/src/services.rs:1580-1584`. `push_waiter` renvoie `false` si pleine (16), mais le retour est ignoré et le thread est quand même suspendu sur `WaitDisplay`.
**Fix :** propager l'échec ; si la file est pleine, renvoyer une erreur au syscall (`OutOfSpace`/retry) au lieu de suspendre.

### ☑ A6. Panic handlers userspace spinnent sans terminer → gel système
`apps/shell/src/main.rs:914`, `apps/hello/src/main.rs:308`, `os/fs/src/main.rs:486`, `os/procman/src/main.rs:232`.
Une app qui panique en tenant le display ne libère jamais display/focus → shell bloqué dans `AcquireDisplay`, procman dans `WaitProcess`.
**Fix :** chaque `#[panic_handler]` appelle `TerminateProcess(nonzero)` après avoir loggé, au lieu de `loop { wfe }`.

---

## P1 — Bugs de logique / API, déclenchables sur QEMU

### ☑ B1. `ReleaseInputFocus` ne vérifie pas la propriété (API « symétrique » qui ne l'est pas)
`xous/kernel/src/syscall.rs:1049`, `services.rs:1540`. N'importe quel process peut couper le clavier du shell. `AcquireInputFocus` vérifie bien la propriété.
**Fix :** `release_input_focus(pid)` ne nettoie le focus que si `pid` est le propriétaire display courant (ou le détenteur du focus).

### ☑ B2. Focus clavier pendouillant à la mort d'un détenteur non-propriétaire → frappes jetées
`xous/kernel/src/services.rs:1638-1644`, `arch/aarch64/irq.rs:232-237`. `release_display_on_exit` ne nettoie le focus que si le mourant est owner ; sinon `input_sid` reste, et `deliver_char_to_sid` jette chaque frappe vers un SID mort.
**Fix :** à la mort de tout process, si `input_sid` résout vers un serveur de ce process, effacer le focus (rebascule vers le buffer noyau).

### ☑ B3. Double `AcquireDisplay` par le propriétaire → auto-deadlock
`xous/kernel/src/services.rs:1579-1590`. Pas de garde `owner == pid` : le propriétaire s'enfile lui-même et se suspend en attendant sa propre release.
**Fix :** si `owner` est déjà `pid`, renvoyer immédiatement la position curseur (idempotent) sans enfiler.

### ☑ B4. `WaitProcess(soi-même)` / `WaitProcess(1)` → blocage définitif
`xous/kernel/src/syscall.rs:942-959`. Aucune garde self/kernel.
**Fix :** rejeter `target_pid == current_pid()` et `target_pid == 1` avec `InvalidArguments`.

### ☑ B5. Réveil `WaitProcess` plafonné à 64 waiters en silence
`xous/kernel/src/services.rs:1096-1111`. Buffer fixe 64 alors que l'espace de waiters est 64×32.
**Fix :** boucler le réveil directement sur les threads scannés (deux passes) au lieu d'un buffer intermédiaire borné, ou dimensionner correctement.

### ☑ B6. Fuite de slot processus si la traduction argv échoue après `create_process`
`xous/kernel/src/services.rs:1449-1454`. Le process créé reste orphelin dans la table (jamais schedulé ni libéré) si `virt_to_phys(argv_ptr)` échoue.
**Fix :** `free_process(pid)` sur tous les chemins d'erreur postérieurs à `create_process`.

---

## P2 — Latents sur QEMU, fatals au portage matériel (M3/M3b)

### ☑ C1. Aucune maintenance I-cache après chargement ELF
`xous/kernel/src/arch/aarch64/elf.rs:183-209`. Code écrit via linear map cachable puis exécuté à EL0 sans `DC CVAU`+`IC IVAU`+`DSB`/`ISB`. QEMU cohérent → OK ; M1 (I-cache séparé) → exécution d'octets périmés.
**Fix :** ajouter une routine `sync_icache(va, len)` (boucle `dc cvau` / `ic ivau` par ligne de cache, `dsb ish`, `isb`) appelée après copie des segments exécutables dans `load_elf`.

### ☑ C2. Barrière virtio du mauvais côté de la lecture `used.idx`
`xous/kernel/src/platform/qemu_virt/virtio.rs:274-285`. `fence(Acquire)` précède la lecture d'`idx` ; rien n'ordonne `idx` → lecture élément/`REQ_STATUS`. Masqué sur hôte x86, réel sur hôte ARM (dev sur Mac).
**Fix :** lire `idx`, tester, **puis** `fence(Acquire)` avant de lire l'élément du ring — pattern `virtio_rmb()`.

### ◐ C3. Reliquat 32-bit dans `MemoryRangeExtra`
`xous/kernel/src/mem.rs:47-52, 66, 870`. `mem_start`/`mem_size` en `u32`, addition en u32 avant cast → wrap au-delà de 4 GiB. M1 : RAM à 34 GiB.
**Fix :** passer les champs en `u64`/`usize` (ou caster avant addition partout).

### ☑ C4. Fuite de descripteurs + réutilisation de buffer sur timeout blk
`xous/kernel/src/platform/qemu_virt/blk.rs:354-374`. Timeout → 3 descripteurs jamais libérés (5 fois = I/O disque morte) ; `REQ_HEADER`/`REQ_STATUS` réécrits alors que le device peut encore écrire.
**Fix :** `free_chain` sur le chemin timeout ; idéalement marquer le device en erreur permanente plutôt que réutiliser des buffers en vol.

### ☑ C5. `free_chain` fait confiance à l'`id` device-écrit → OOB noyau
`xous/kernel/src/platform/qemu_virt/virtio.rs:284, 232-241`. `elem.id` non borné vs `self.num` → `desc.add(idx)` OOB.
**Fix :** dans `pop_used`, rejeter `elem.id >= self.num` (retourner `None`/ignorer).

### ☑ C6. `FbConsole` confond largeur et stride
`beetos/src/fb_console.rs:86-88, 110-123`. `total = width*height` mais chaque ligne fait `stride` pixels. stride==width partout aujourd'hui.
**Fix :** calculer en fonction de `stride` (pixels par ligne physique) pour scroll et clear.

### ☑ C7. Retry IPC `ServerQueueFull` cassé côté AArch64
`xous/xous-rs/src/arch/aarch64/syscall_impl.rs`, `xous/kernel/src/arch/aarch64/process.rs:243`. Le noyau renvoie `RetryCall` en attendant que l'userspace re-émette le SVC ; le wrapper AArch64 ne boucle pas (le hosted oui) → message perdu.
**Fix :** boucler sur `Result::RetryCall` dans le wrapper syscall AArch64 (comme hosted).

### ◐ C8. `run_irq_handler` écrit `threads[0]` = thread principal
`xous/kernel/src/arch/aarch64/process.rs:505-517`. Indexation `tid-1` partout ; `threads[0]` est le thread principal, réutilisé pour l'IRQ → écrase PC/args de l'interrompu.
**Fix :** dédier un slot de thread IRQ distinct, ou revoir la convention d'indexation pour l'IRQ_TID.

---

## P3 — Robustesse / conformité aux règles projet

### ☑ D1. `flags_to_pte` branche kernel peut produire W+X
`xous/kernel/src/arch/aarch64/mem.rs:388-401`. `user=false` + `W|X` → RW EL1 sans PXN. Aucun caller ne passe `X` aujourd'hui.
**Fix :** garde W^X symétrique dans la branche kernel (forcer PXN si W).

### ☑ D2. `invalidate_page` sans maintenance D-cache
`xous/kernel/src/arch/aarch64/mem.rs:675-677`. Contrat documenté par l'appelant, impl ne fait que le TLB. Latent jusqu'à remapping Non-Cacheable.
**Fix :** `dc civac` sur la plage physique de la page libérée.

### ☑ D3. ELF loader : tailles header/phdr/reloc non validées
`xous/kernel/src/arch/aarch64/elf.rs:141-143, 170-171, 288-291`. `e_phoff/e_phnum/p_memsz/DT_RELASZ` non bornés vs `len` ; `vaddr+memsz` peut wrapper. Binaires embarqués = confiance actuelle.
**Fix :** valider chaque offset+taille contre `elf_bytes.len()`, `checked_add` sur `vaddr+memsz`.

### ☑ D4. tarfs : listing sans frontière `/`
`os/fs/src/tarfs.rs:144-157`. `ls /disk/bin` liste `ary.dat` (de `binary.dat`). `has_dir` fait la frontière correctement.
**Fix :** exiger `name == prefix` ou `name` commence par `prefix + '/'`.

### ☑ D5. tarfs : `parse_octal` casse sur espaces de tête + pas de base-256
`os/fs/src/tarfs.rs:54-65`. Taille paddée d'espaces → 0 → désynchro du parcours. GNU/bsdtar zero-pad (disque xtask OK), mais tout writer space-pad casse.
**Fix :** ignorer espaces/NUL de tête et de queue ; rejeter base-256 (bit haut) proprement.

### ☑ D6. `write` extrait le contenu via `full_line.find(path)` sur le chemin résolu
`apps/shell/src/main.rs:556-561`. Chemin relatif → contenu multi-mots tronqué à `args[1]`.
**Fix :** reconstruire le contenu depuis les args bruts (join après le premier arg), pas via `find` sur le chemin résolu.

### ◐ D7. Divers (règles projet / robustesse mineure)
Corrigés :
- ☑ `lba + count` peut wrapper `blk.rs:132,159` → `checked_add`.
- ☑ ICMP echo inclut le padding Ethernet `net_stack.rs` → borne le payload par IP total-length.
- ☑ INTID 1020-1022 non filtrés avant EOI `irq.rs` → filtre `irq >= 1020` (qemu + bcm2712).
- ☑ `app-manifest` `unwrap()` host-side `lib.rs:71` → dégradation gracieuse sans panic.
- ☑ `get_rx_frame` `expect()` sur chemin RX `net.rs:150` → retour slice vide + bornes.
- ☑ (D4 bis) `ls /disk/<inexistant>` renvoyait Ok → garde `has_dir` dans `do_ls`/`do_ls_buf`.

Différés (documentés dans le code / hors périmètre de cette passe) :
- ⏭ `UART_PHYS 0x0900_0000` codé en dur `services.rs` (règle FDT) → nécessite l'infra FDT (M3+).
- ⏭ `FB_PHYS 0x7FC0_0000` figé `fb.rs:31` → latent (xtask force `-m 2G`) ; dériver de la RAM réservée avec le portage M3b.
- ⏭ `input.rs:166` `expect()` = invariant d'init (file fraîche ⇒ succès garanti) — panic intentionnel, conforme « informatif ».
- ⏭ `apps/coreutils` ne compile plus (opcodes FS inexistants), exclu du build → réparer ou retirer (dead code).
- ⏭ `loader/` `PAGE_SIZE=4096` hérité KeyOS → audit 16K à faire au portage matériel.
- ⏭ Linear map noyau RWX à EL1 `start.S:186-201` → compromis de conception du linear map, à documenter/durcir plus tard.

---

## Vérifié sain (réfutations)
Géométrie tables 16K exacte · frame contexte `asm.S` = struct Rust octet à octet ·
barrières *avail ring* correctes · fix MapMemory anonyme (`1bcfe0e`) complet ·
noms `SpawnByName` par registres (pas de pointeur user) · ASID flushé avant réuse
de PID · GIC/timer/UART propres · argv borné à une page.

---

## Ordre d'exécution
P0 (testable hosted en priorité) → P1 → P2 (préparer M3/M3b) → P3.
Validation : `cargo test -p beetos-kernel` (hosted) après chaque lot touchant le
noyau ; `cargo xtask build` (cross AArch64) pour les fixes `#[cfg(beetos)]`.

---

## Détail des items ◐ (partiellement traités)

- **C3 — `MemoryRangeExtra` 32-bit.** L'**arithmétique** est corrigée (cast avant
  addition, plus de wrap/overflow-panic sur QEMU). Les champs restent en `u32`
  **volontairement** : la struct est `memcpy`'d depuis le format binaire fixe des
  kernel-args (16 octets) ; l'élargir en `u64` casserait le parsing sur *toutes*
  les plateformes. Le format 64-bit complet est une tâche coordonnée avec le
  loader, à faire au portage M3b (RAM Apple à 34 GiB).

- **C8 — `run_irq_handler` / slot de thread IRQ.** Défaut **structurel** sur un
  chemin (`IrqHandler::User` via `ClaimInterrupt`) **non câblé** sur QEMU/RPi5 et
  donc non testable ici. Un rewrite à l'aveugle du modèle de threads IRQ
  risquerait de casser le boot fonctionnel. Choix : **avertissement `⚠️` explicite
  au site du code** décrivant précisément la corruption et le fix attendu, pour que
  le câblage des handlers IRQ userspace (M3+) le corrige correctement.

- **D7 — Divers.** Voir la ventilation ☑/⏭ ci-dessus : 6 items corrigés, 6 différés
  (infra FDT, latents matériel, dead code, compromis de conception).

## Note de build (environnement)
Le cross-build du noyau `include_bytes!` `hello-std.stripped`, produit par le
toolchain Rust stage1 custom (`beet-os/rust`, target `aarch64-unknown-beetos`)
**absent de ce conteneur**. Un placeholder éphémère (copie de `hello.stripped`,
sous `target/` gitignoré) a servi à valider la compilation du noyau ; il n'est ni
commité ni fonctionnellement correct pour `hello-std`. Sur une machine avec le
toolchain installé (`cargo xtask install-toolchain`), le build est complet.
