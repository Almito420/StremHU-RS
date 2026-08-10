# stremhu-rs

Stremio addon, ami az [nCore](https://ncore.pro) torrentjeit streameli, letöltés bevárása
nélkül. Egy önálló Windows program: nincs Docker, nincs adatbázis, nincs külön torrent
kliens. A torrentmotor beágyazott **libtorrent 2.0.11**.

Amit megnyomsz a Stremióban, az néhány másodperc alatt indul, közben a fájl a háttérben
végig letöltődik, utána seedel, és amikor a seedelési kötelezettséget letudta, magától
törlődik. Egy évadcsomagból **csak azt az egy részt** hozza le, amit megnézel.

---

## Miért van ez

Egy meglévő megoldás (a python StremHU) helyett készült, és három dologban másképp
működik, mert azok gyakorlati problémát okoztak:

**A metaadat a TMDB addonból jön, nincs saját katalógus.** Ennek a szerver kereséséhez van
köze: a saját katalógus nem talál meg olyan magyar sorozatot, aminek nincs IMDb azonosítója.
Az "Exek csatája" ilyen. Ez a szerver csak a stream listát adja, a címeket a TMDB addon
szolgáltatja, és így minden megtalálható amit az nCore ismer.

**Fájlonkénti nyilvántartás.** Egy évadcsomag egy torrent, de sok rész. Itt minden kiszolgált
fájl külön sor: külön megnézettség, külön seedelési idő, külön törlés. Egy 1.33 TiB-os teljes
sorozatpackból egyetlen 7 GB-os rész jön le, és amikor az leszolgálta a magáét, elmehet, míg
a torrent a többivel tovább seedel.

**A megnézettség mérve van, nem feltételezve.** Nem az számít, hogy elindult egy lejátszás,
hanem hogy a fájlnak mekkora része ment ki tényleg a lejátszóhoz. Egy bittérkép tartja
számon, megabájtonként egy bit, ezért az újraolvasás nem fújja fel: aki egy film első
negyedét négyszer nézi meg, annál 25% marad, nem 100% lesz.

---

## Mire van szükség

- Windows 10 vagy 11, 64 bit.
- nCore fiók.
- [TMDB](https://www.themoviedb.org/settings/api) API kulcs, ingyenes.
- Semmi más. A kiadás zipje mindent tartalmaz, a Microsoft C++ futtatókörnyezetet is.

---

## Indulás nulláról

1. Töltsd le a [kiadás](../../releases) zipjét, és csomagold ki egy mappába, például
   `D:\stremhu-rs`.
2. Indítsd el a `stremhu-rs.exe`-t. **Nem nyílik ablak**: ez egy szerver, ami a háttérben
   fut. Nincs mit becsukni.
3. Nyisd meg: <http://localhost:3080/ui>
4. Adj meg egy admin jelszót, aztán az nCore fiókot és a TMDB kulcsot.
5. A lap kiírja az **addon URL-t**. Ezt illeszd be a Stremióba (Addons → jobb felül a
   beillesztés mező).

Az első indításnál a program ezt hozza létre az exe mellé:

```
config.toml        a beállítások, alapértékekkel kitöltve
state.json         mi van a lemezen, mit néztünk meg (az első letöltésnél jelenik meg)
downloads\         ide jönnek a fájlok
torrents\          a .torrent és a folytatási (resume) adatok
certs\             a HTTPS tanúsítvány, magától letöltve
```

Semmi más: se registry, se AppData, se rejtett mappa. A mappa egészben másolható,
mozgatható, törölhető.

### Televízió és más eszközök

A Stremio böngészőben fut, és a böngésző nem tölt be sima HTTP tartalmat egy másik gépről.
Ezért a szerver HTTPS-t is kiszolgál, tanúsítvánnyal, amit magától letölt és lejárat előtt
megújít. Ehhez csak a gép hálózati címét kell megadni a beállításokban (magától is
megpróbálja kitalálni), semmilyen domain vagy DDNS fiók nem kell.

A tanúsítvány a `local-ip.medicmobile.org` szolgáltatásból jön: egy publikus wildcard DNS,
ami a privát IP-kre mutat. A tévé így egy érvényes tanúsítvánnyal ellátott nevet lát.

### Leállítás

A beállítások lap alján, a **Szerver leállítása** gombbal. Előtte kiírja az állapotot és
minden torrent folytatási adatait, hogy a következő indulás ne kezdje újra több száz
gigabájt ellenőrzését.

---

## Hogyan dönt a törlésről

Ez a rész az nCore [seedelési szabályaira](https://ncore.pro/wiki.php?action=read&id=609)
épül. A tracker a kötelezettséget **torrentenként** rója ki minden olyan torrentre, amiből
legalább 5% vagy legalább 200 MB lejött, és kétféleképpen lehet teljesíteni: visszaosztod
amennyit letöltöttél (arány 1.0), vagy leseedelsz ennyit:

```
hátravan = (1 − arány) × (48 óra + 0.4 × letöltött GB) − eddig seedelt idő
```

A szerver ezt követi, és **a tracker saját válasza mindig előbbre van, mint ez a számítás**:
ha megkérdeztük és azt mondta, hogy nincs tartozás, akkor nincs. A formula arra van, hogy két
announce között (30-44 perc) is tudjunk következtetni.

Egy fájl akkor törölhető:

| | feltétel |
|---|---|
| **bármelyik fájl** | megnéztük, nincs megtartásra jelölve, nem játszik épp |
| **nem utolsó fájl** | letelt a *saját* ideje: `(1 − arány) × (48 óra + 0.4 × a fájl GB-ja)` |
| **utolsó fájl** | a torrent *teljes* tartozása letelt, plusz 6 óra ráhagyás |

Az "utolsó fájl" az, ami életben tartja a torrentet. Amíg egy torrentből van fájl a lemezen,
a kliens azt jelenti a trackernek, hogy nincs mit letöltenie, tehát seedernek látszik és
ketyeg a seedelési idő. Ezért lehet egy csomagból a leszolgált részeket kirotálni, és ezért
nem tűnik el soha egy torrent addig, amíg a tartozása nincs letörlesztve.

A törlés minden nap 20:00-kor fut (állítható), és induláskor is egyszer. Egy körben egy
torrentből az összes törölhető fájl egyszerre megy, hogy csak egy újraellenőrzés legyen.

---

## Értesítések

Egy webhook címet lehet megadni. A Discordot és az ntfy-t is ismeri: a Discordnak a
szükséges JSON alakot küldi, mindenki másnak sima szöveget.

Amiről szól:

- **minden takarítási kör után**, akkor is ha nem törölt semmit (az okokat csoportosítva);
- ha **kifogy a hely**, megnevezve melyik lemezen és melyik mappában;
- ha a letöltés **átcsúszik a másodlagos mappára**;
- **bármilyen hibáról**, amit a program bárhol jelent (ez a naplózásba van bekötve, nem
  egyenként, tehát a később hozzáírt hibaágakra is szól);
- ha a **processzor- vagy memóriahasználat** tíz egymást követő mérésen át kirívóan magas.

Mind fajtánként fojtva, hat óránként legfeljebb egy, hogy egy fennálló állapot ne
száz üzenet legyen.

---

## Beállítások

Minden a `config.toml`-ban van, és a fontosabbak a felületről is állíthatók. A
[`config.toml.example`](config.toml.example) az összes kulcsot felsorolja.

Amit érdemes ismerni:

| beállítás | mi ez |
|---|---|
| `pieces.readahead_bytes` | mennyi filmet töltsön a lejátszási fej előtt (64 MB). **Bájtban, nem piece-ben**: a piece méret kiadásonként 0.5 és 16 MiB között van, tehát egy fix piece-szám 4K-nál egy másodperc alatti puffert jelentett, és a stream pár másodperc után megállt |
| `torrent.max_active_torrents` | egyszerre ennyi torrent aktív, `-1` a korlátlan. A libtorrent alapból ötnél megállítaná a többit, és a megállított torrent nem seedel |
| `torrent.complete_extras_below_bytes` | a torrent maradékát is lehozza, ha az ennél kisebb (512 MiB). Egy film mellett ott van egy minta és egy nfo; nélkülük a torrent soha nem lesz teljes seed, és a tracker örökre 98.94%-ot mutat. Egy évadcsomagnál ez a korlát tartja vissza a többi részt |
| `torrent.save_path_secondary` | ha az elsődleges mappa megtelt, ide ír. Sorrend szerint: a másodlagosat addig meg sem méri |
| `maintenance.sweep_at` | mikor fusson a törlés |
| `maintenance.require_watched` | csak megnézett fájlt töröl |

---

## Fordítás forrásból

```powershell
# vcpkg és a libtorrent, egyszer
git clone https://github.com/microsoft/vcpkg D:\vcpkg
D:\vcpkg\bootstrap-vcpkg.bat
D:\vcpkg\vcpkg install libtorrent:x64-windows

$env:VCPKG_ROOT = "D:\vcpkg"
cargo build --release
```

A `build.rs` lefordítja a C ABI shimet (`shim/shim.cpp`) CMake-kel, és a szükséges DLL-eket
a bináris mellé másolja. A shim azért van, mert a `set_piece_deadline` az egyetlen dolog,
amivel a letöltés sorrendje **utasítható**: a soros letöltés és a piece prioritások csak
javaslatok, egy valódi swarmon a folytonos front 13254-ből 8 piece-nél megállt, miközben a
torrent 30%-a már megvolt.

```powershell
cargo test --release     # 258 teszt
```

---

## Ami tudatosan nincs benne

Több felhasználó és jogosultságok, több tracker, saját Stremio katalógus és meta végpont,
DDNS szolgáltatók, minőség szerinti kizárás, Kodi végpont, eszközpárosítás. Ez egy embernek
egy gépen szolgál ki egy trackert, és minden fenti nélkül kevesebb dolog van, ami elromolhat.

## Licenc

Magánprojekt, saját használatra. A libtorrent (BSD), a Boost, az OpenSSL és a Microsoft
C++ futtatókörnyezet a saját licencük szerint kerül a kiadásba.
