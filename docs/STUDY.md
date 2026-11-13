# Étude : monter un `.pkg` PS5 en FUSE userland

Étude faite sur `quer3q/ps5-dump-forge` (GPL-3.0) au commit `8a3c7d3`, puis mise en œuvre dans
ce dépôt. Elle répond à une question précise : peut-on exposer un `.pkg` comme un répertoire,
sans le décompresser ni le copier ?

## Conclusion

Oui pour un **FPKG debug** — le lecteur du dépôt est déjà une API à accès aléatoire, et
l'adaptateur FUSE fait ~450 lignes. Non pour un **`.pkg` retail** : le lecteur refuse tout
paquet dont l'octet « signed » du header n'est pas `0x00` (`crates/ps5-dump-forge-fpkg/src/pfs.rs`).

## Pourquoi le lecteur s'y prête

- `FpkgSource` (`lib.rs`) expose `files()` (chemins triés + tailles), `empty_dirs()` et
  `read_range(path, offset, len)` : de quoi servir `readdir`, `lookup`, `getattr` et `read`.
- Les données ne sont jamais matérialisées : chaque plage demandée traverse la carte de blocs
  de l'image interne (`mount.rs`), ne décode que les blocs Kraken qu'elle touche (≤ 256 Kio),
  déchiffre les blocs externes AES-XTS et vérifie leur SHA3.
- L'arbre est parcouru une fois à l'ouverture (`tree.rs`) : exactement ce qu'un montage exige.

## Mesures (machine locale, paquets synthétiques construits par le dépôt)

| Charge | Résultat |
|---|---|
| `diff -r` montage vs extraction CLI | identique, 3 004 fichiers |
| Séquentiel `dd` `O_DIRECT` | 142 Mo/s |
| Lecture 4 Kio aléatoire, cache 32 Mo | p50 1,9 ms |
| Lecture 4 Kio aléatoire, cache ≥ fichier | p50 0,13 ms |
| Montage (40 208 entrées indexées) | 476 ms, 3,6 Mio RSS |

Sans cache applicatif, une lecture 4 Kio aléatoire froide coûte le décodage d'un bloc entier
(~2,5 ms, soit ~1,6 Mo/s en aléatoire pur). C'est le principal point faible : dimensionner
`--cache-mb` sur l'ensemble de travail d'un jeu (ou un LRU plus grand) règle le problème.

## Manques à combler pour un usage courant

1. Retail : header signé refusé ; il faudrait les clés (passcode retail, `keys.h`/LibOrbisPkg).
2. `&mut self` sur `read_range` : un `Mutex` sérialise les lectures ; un client FUSE
   multi-thread exigerait des caches à mutabilité intérieure.
3. Parcours complet au montage : coût en secondes et empreinte mémoire sur les gros titres
   (descripteurs jusqu'à 384 Mio, plafond `MAX_OPEN` de `mount.rs`).
4. Pas d'attributs dans le format : pas de mode, propriétaire, mtime, symlink ; FS forcément
   en lecture seule et attributs synthétisés.
5. Paquets de patch (`.pkg` à header `\x7FLIH`) : seuls les fichiers portés en entier sont
   listés, les blocs du jeu de base ne sont pas servis.
6. Plateformes : Linux trivial (fusermount), macOS exige macFUSE, Windows WinFsp — à garder
   optionnel, le README de Forge revendiquant « no FUSE, no install » pour l'AppImage.

## Comparaison

kraken-mount (`quer3q/kraken-mount`) lit aussi les `.pkg` en place, mais ne fournit pas de vue
FS générique : il extrait les métadonnées et libellés dans un cache, monte le dossier du `.pkg`
en RO sur `/app0/kraken` et sert les gros fichiers via un index AMPR que seul ampr-emu-ng
comprend. ShadowMountPlus monte les images (`.ffpkg`, `.exfat`, `.ffpfs`, `.ffpfsc`) sur la
console, pas les `.pkg`. Un FUSE côté PC est donc un complément, pas un doublon.

## Optimisation (après la première version synchrone)

Première version : un seul lecteur, cache LRU, readahead synchrone. Mesures : 91 Mo/s en
séquentiel, 2,2 ms par lecture 4 Kio aléatoire, 17 Mo/s agrégés sur 4 flux concurrents. Le
point dur n'était pas le décodage mais la sérialisation : chaque lecture bloquait le fil FUSE
unique (fuser) sur un décodage, et un seul cache servait tous les fichiers.

Version retenue :

- décodage dans des fils ouvriers, les rappels FUSE ne font que copier depuis un cache et
  n'attendent qu'un bloc déjà en cours de lecture ;
- un lecteur et un cache par ouvrier, fichiers répartis par hachage du chemin : les fichiers
  distincts décodent en parallèle, chaque ouvrier garde son propre fenêtrage ;
- fenêtre de readahead (32 Mio) planifiée avant la lecture séquentielle, et lectures fusionnées
  jusqu'à 8 Mio pour décoder large sur tous les cœurs ;
- blocs partagés en `Arc<[u8]>`, une requête ne copie que ses propres octets ;
- `max_read=1 Mio` pour que le noyau demande gros.

Résultat sur 4 flux concurrents : 385 Mo/s agrégés (500 en pointe), 175 Mo/s en séquentiel
simple, p50 0,18 ms en 4 Kio aléatoire, soit ~20× le débit concurrent et ~12× la latence de la
première version. Le coût est la mémoire (`--cache-mb`, 256 Mio par défaut, répartie) et le
temps de montage (un parcours d'arbre par ouvrier).

## Mise en œuvre ici

`src/lib.rs` (index chemin → inode, caches LRU par ouvrier, fenêtres de readahead, fusion des
lectures), `src/main.rs` (mount/list/unmount), `examples/mkpkg` (paquet de test),
`scripts/smoke.sh` (montage, `diff -r` et hachage concurrent contre l'extraction CLI). Le FS est
monté en `ro`, `NoAtime`, `NoExec`, `NoSuid`, `max_read=1 Mio`.
