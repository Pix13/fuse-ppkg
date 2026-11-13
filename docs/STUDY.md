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

## Mise en œuvre ici

`src/lib.rs` (index chemin → inode, cache de blocs avec LRU, readahead séquentiel), `src/main.rs`
(mount/list/unmount), `examples/mkpkg` (paquet de test), `scripts/smoke.sh` (montage + `diff -r`
contre l'extraction CLI). Le FS est monté en `ro`, `NoAtime`, `NoExec`, `NoSuid`.
