# stremhu-rs

Stremio addon, ami az [nCore](https://ncore.pro) torrentjeit streameli, letöltés bevárása
nélkül. Egy önálló Windows program: nincs Docker, nincs adatbázis, nincs külön torrent
kliens. A torrentmotor beágyazott **libtorrent 2.0.11**.

Amit megnyomsz a Stremióban, az néhány másodperc alatt indul, közben a fájl a háttérben
végig letöltődik, utána seedel, és amikor a seedelési kötelezettségét letudta, magától
törlődik. Egy évadcsomagból **csak azt az egy részt** hozza le, amit megnézel.

---

## Eredet és licenc

Ez a program a **StremHU** újraírása Rustban, Windowsra.

**Eredeti projekt: <https://github.com/s4pp1/stremhu-source>** — szerzője **s4pp1**, licence
**GPL-3.0**. Konténerképe a Docker Hubon `s4pp1/stremhu-source` néven érhető el, és több
trackert (nCore, BitHUmen, iNSANE, Majomparádé) és több klienst (Stremio, Nuvio, Kodi)
szolgál ki.

Az eredeti szerző hozzájárulásával és kérésének megfelelően:

- ez a projekt is **GPL-3.0** alatt van (lásd [LICENSE](LICENSE)),
- alább részletesen le van írva, **miben tér el** az eredetitől,
- és köszönet az ötletért meg a megoldásokért, amikből tanulni lehetett.

> „Örülök, hogy láttál benne fantáziát és foglalkoztál vele, hogy Windows környezeten tudd
> használni, ahogy neked kényelmes!" — s4pp1

Ez nem az eredeti projekt folytatása és nem is helyettesíti: az eredeti több trackert,
több felhasználót és Linux/Docker környezetet szolgál ki, ez pedig egy embert, egy gépen,
egy trackerrel.

---

## Miben tér el az eredetitől

### Ami az alapoknál más

| | StremHU (eredeti) | stremhu-rs |
|---|---|---|
| futtatás | Python, Docker, FastAPI | egyetlen Windows exe, ablak nélkül |
| adattárolás | SQLite + Alembic migrációk | egy JSON fájl, kézzel is olvasható |
| torrentmotor | python-libtorrent | libtorrent 2.0.11 beágyazva, C ABI shimen keresztül |
| trackerek | nCore, FileList, BitHumen, HunTorrent, Insane, Majomparade | csak nCore |
| felhasználók | többfelhasználós, jogosultságokkal | egy admin |

### Metaadat: nincs saját katalógus

Az eredeti kiszolgál Stremio katalógus- és meta-végpontokat. Ez nem: **csak stream listát ad**,
a címeket a TMDB addon szolgáltatja. Ennek gyakorlati oka van: a saját katalógus nem találja meg
azokat a magyar sorozatokat, amiknek nincs IMDb azonosítója. Az "Exek csatája" ilyen, és ezért
lett így.

### Fájlonkénti nyilvántartás

Az eredetiben egy torrent egy adatbázissor (`TorrentModel`, kulcs: info hash + indexer), és a
törlés az egész torrentet viszi. Itt minden **kiszolgált fájl** külön sor
(`info_hash:fájlindex`), külön megnézettséggel, seedelési idővel és törléssel. Egy 1.33 TiB-os
teljes sorozatpackból egyetlen 7 GB-os rész jön le, és amikor az leszolgálta a magáét, elmehet,
míg a torrent a többivel tovább seedel. Az utolsó fájl viszont soha nem megy el a torrent teljes
tartozásának letörlesztése előtt.

### Fájlválasztás

Az eredeti nem használ per-fájl prioritást: minden piece prioritása vagy 1 (teljes torrent), vagy
0, és ilyenkor csak a stream ablaka töltődik `set_piece_deadline`-nal. Itt a kért fájl kap
prioritást (`file_priority`), tehát végig letöltődik és utána seedelhető, a többi fájl pedig
nulla prioritáson marad. A minta és az nfo utólag jön le, ha **kicsi önmagában és a kért fájlhoz
képest is** — az utóbbi feltétel nélkül egy XviD sorozatcsomag minden 346 MB-os része
kísérőfájlnak számított, és 22 fájl jött le egy rész kiszolgálásához.

### Megnézettség: mérve, nem feltételezve

Az eredetiben a törlés feltétele az, hogy *elindult-e* egy lejátszás (`playback_histories`
legutóbbi sora). Itt egy bittérkép tartja számon, megabájtonként egy bit, hogy a fájlnak mely
részei mentek ki tényleg a lejátszóhoz. Ezért az újraolvasás nem fújja fel: aki egy film első
negyedét négyszer nézi meg, annál 25% marad, nem 100% lesz. Ezen felül a lejátszási pozíciónak
is el kell érnie a beállított százalékot.

### Törlés az nCore közölt szabálya szerint

Az eredeti egy fix `keep_seed_seconds` időt vár a legutóbbi lejátszás kezdetétől. Itt a
[tracker saját formulája](https://ncore.pro/wiki.php?action=read&id=609) dönt:

```
hátravan = (1 − arány) × (48 óra + 0.4 × letöltött GB) − eddig seedelt idő
```

és emellett:

- **a tracker mindkét listáját** beolvassa. A `showall=false` a még nyitott kötelezettségeket
  adja, a `showall=true` mindent, amiről a trackernek nyilvántartása van. Ami egyik listán sincs,
  arról a tracker még nem tud, tehát a hiányzása nem jelent semmit; ami a hosszún rajta van de a
  rövidön nem, az letörlesztette a tartozását. Egyetlen listából ez a két eset egyformán néz ki.
- **a tracker válasza előbbre van a helyi számításnál**, de csak ha vannak róla számai;
- **hat órás ráhagyás**, mert a tracker announce-onként számol újra (30-44 perc) és a hónapot
  2-3 órával a vége előtt zárja;
- ha a tracker listáját **nem sikerül beolvasni, az egész kör elmarad**. Az eredetinél a
  lekérés hibája feljebb elnyelődik és a kör lefut; ha a lista üresen jön vissza és a
  `keep_seed_seconds` nincs beállítva, az adott tracker összes nem kitűzött torrentje törlődik.
- **soha nem töröl olyan torrentből, amit épp néznek.** Az eredeti 04:00-as köre ezt nem
  vizsgálja.

### Helytakarékos mód

Pipálható: egy megnézett fájl azonnal törölhető, és csak az marad meg, amivel a torrent tovább
tud seedelni. Egy sorozatcsomagnál nyolc rész helyett egy a lemezen. Filmnél nem változtat
semmit, mert ott az az egy fájl a fizetőeszköz.

### Lemezhely

Az eredetiben **nincs szabad hely ellenőrzés** (se `shutil`, se `statvfs`, se `disk_usage` a
kódban). Itt van elsődleges és másodlagos mappa: minden az elsődlegesre megy, amíg elfér, és a
másodlagosat addig meg sem méri a program. Ha a kért fájl nem fér el, a letöltés a másodlagosra
indul, és erről értesítés megy. Ha egyikbe se fér, a kérés tiszta hibával elhasal, nem a motor
írási hibájával. A mérés a letöltendő **fájl** méretére megy, nem a torrentére: egy 1.33 TiB-os
csomagból egy 7 GB-os rész elfér oda, ahova a csomag nem.

### Előretöltés bájtban, nem piece-ben

A piece méret kiadásonként 0.5 és 16 MiB között van, tehát egy fix piece-szám 4K-nál egy
másodperc alatti puffert adott, és a stream pár másodperc után megállt. Itt az ablak
`readahead_bytes` (64 MB), vagyis minden torrenten ugyanannyi film.

### Hálózat és lejátszás

- **HTTPS a `local-ip` trükkel**: publikus wildcard DNS, ami a privát IP-re mutat, tanúsítványt
  a program magától letölt és megújít. Nem kell domain, nem kell DDNS fiók — az eredeti DDNS
  szolgáltatókat kezel.
- **a stream URL is HTTPS**, ha a HTTPS fut. Böngészőben a HTTPS oldalra töltött sima HTTP médiát
  a böngésző blokkolja.
- **`Access-Control-Allow-Private-Network`** a preflight válaszban. A Chrome egy publikus oldalról
  privát címre menő kérést ehhez köt, és a hiányát sima CORS hibaként mutatja.
- **a tartalomtípus a fájl kiterjesztéséből** jön. Egy `.avi` fájlt Matroskaként hirdetni annyi,
  hogy a lejátszó szó nélkül feladja.

### Üzemeltetés

- **ablak nélküli program.** Naplózás csak `--log` esetén; ha nem tud elindulni, azt mindig
  kiírja fájlba és hibaablakban is.
- **Discord vagy ntfy értesítés**, kapcsolókkal: a törlési körökről (akkor is ha nem törölt
  semmit), a lemezről, és a hibákról. A hibaértesítés a naplózásba van bekötve, nem
  egyenként, tehát a később hozzáírt hibaágakra is szól.
- **erőforrás-figyelő**: ha tíz egymást követő mérésen át kirívó a processzor- vagy
  memóriahasználat, szól. A privát memóriát méri, nem a munkakészletet: mérve 1808 MB
  munkakészlet mellett 71 MB volt a privát, a többi a libtorrent memóriába leképezett írási
  puffere.
- **`.torrent` fájlok és folytatási adatok** a lemezen, tehát egy újraindítás nem indít
  újraellenőrzést több száz gigabájton.

---

## Mire van szükség

- Windows 10 vagy 11, 64 bit.
- nCore fiók.
- [TMDB](https://www.themoviedb.org/settings/api) API kulcs, ingyenes.
- Semmi más. A kiadás zipje mindent tartalmaz, a Microsoft C++ futtatókörnyezetet is.

## Indulás nulláról

1. Töltsd le a [kiadás](../../releases) zipjét, és csomagold ki egy mappába, például
   `D:\stremhu-rs`.
2. Indítsd el a `stremhu-rs.exe`-t. **Nem nyílik ablak**: ez egy szerver, ami a háttérben fut.
3. Nyisd meg: <http://localhost:3080/ui>
4. Adj meg egy admin jelszót, aztán az nCore fiókot és a TMDB kulcsot.
5. A lap kiírja az **addon URL-t**. Ezt illeszd be a Stremióba.

Az első indításnál a program ezt hozza létre az exe mellé:

```
config.toml        a beállítások, alapértékekkel kitöltve
state.json         mi van a lemezen, mit néztünk meg
downloads\         ide jönnek a fájlok
torrents\          a .torrent és a folytatási (resume) adatok
certs\             a HTTPS tanúsítvány, magától letöltve
```

Semmi más: se registry, se AppData, se rejtett mappa. A mappa egészben másolható, mozgatható,
törölhető.

### Televízió és böngésző

A tévén a natív lejátszó megy, ott nincs kodekmegkötés. Böngészőben a Stremio Chromium-alapú
lejátszót használ, ami **DTS-t és TrueHD-t nem tud dekódolni** — a BluRay REMUX kiadások
jellemzően ilyen hangot vinnek, és ilyenkor a lejátszó "Video is not supported"-ot ír, akármilyen
helyesen szolgáljuk ki a fájlt. A stream listában látszik a hangformátum, tehát előre válaszható
AAC vagy AC3 hangú kiadás.

### Leállítás

A beállítások lap alján, a **Szerver leállítása** gombbal. Előtte kiírja az állapotot és minden
torrent folytatási adatait.

---

## Beállítások

Minden a `config.toml`-ban van, és a fontosabbak a felületről is állíthatók. A
[`config.toml.example`](config.toml.example) az összes kulcsot felsorolja.

| beállítás | mi ez |
|---|---|
| `pieces.readahead_bytes` | mennyi filmet töltsön a lejátszási fej előtt (64 MB) |
| `torrent.max_active_torrents` | egyszerre ennyi torrent aktív, `-1` a korlátlan |
| `torrent.complete_extras_below_bytes` | a minta és az nfo mérethatára (512 MiB), a kért fájl negyedéig |
| `torrent.save_path_secondary` | ha az elsődleges mappa megtelt, ide ír |
| `maintenance.space_saving` | helytakarékos mód |
| `maintenance.sweep_at` / `sweep_on_start` | mikor fusson a törlés |
| `maintenance.notify_sweep` / `notify_disk` / `notify_problems` | miről jöjjön értesítés |
| `maintenance.keep_seed_seconds` | tartalék, ha a trackertől nincs adat a torrentről |
| `maintenance.cache_retention_seconds` | elhagyott `.torrent` fájlok megtartása |

## Fordítás forrásból

```powershell
git clone https://github.com/microsoft/vcpkg D:\vcpkg
D:\vcpkg\bootstrap-vcpkg.bat
D:\vcpkg\vcpkg install libtorrent:x64-windows

$env:VCPKG_ROOT = "D:\vcpkg"
cargo build --release
cargo test --release
```

A `build.rs` lefordítja a C ABI shimet (`shim/shim.cpp`) CMake-kel, és a szükséges DLL-eket a
bináris mellé másolja. A shim azért van, mert a `set_piece_deadline` az egyetlen dolog, amivel a
letöltés sorrendje **utasítható**: a soros letöltés és a piece prioritások csak javaslatok, egy
valódi swarmon a folytonos front 13254-ből 8 piece-nél megállt, miközben a torrent 30%-a már
megvolt.

## Licenc

**GPL-3.0**, ahogy az eredeti StremHU is. A libtorrent (BSD), a Boost, az OpenSSL és a Microsoft
C++ futtatókörnyezet a saját licencük szerint kerül a kiadásba.
