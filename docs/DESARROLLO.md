# Desarrollo de OmniLoad

Notas técnicas para quien quiera compilar, probar o tocar el código. La
portada para usuarios es el [README](../README.md).

## Compilar

Hace falta [Rust](https://rustup.rs) 1.88 o más nuevo.

```sh
cargo build --release -p omni-gui -p omnid -p omni-cli   # los tres programas
cargo run --release -p omni-gui            # abre la ventana
cargo run --release -p omni-gui -- <url>   # y analiza ese enlace
cargo build --release -p omni-gui          # target/release/omniload(.exe)
cargo build --release -p omni-cli          # target/release/omni(.exe)
```

En Linux hacen falta `libfontconfig` y `libxkbcommon-x11` para ejecutar (vienen
en cualquier escritorio) y, para compilar, `sudo apt install libfontconfig-dev
libxkbcommon-dev` en Debian/Ubuntu.

Los ajustes de la ventana se guardan en `ajustes.json` dentro de la carpeta de
configuración del sistema (`%APPDATA%\OmniLoad`, `~/.config/omniload` o
`~/Library/Application Support/OmniLoad`). La variable `OMNILOAD_AJUSTES` usa
otro archivo, útil para pruebas.

La lista de descargas, los ajustes de la cola y lo aprendido de cada servidor
van en una base de datos SQLite, `omniload.db`, en la carpeta de datos
(`%APPDATA%\OmniLoad`, `~/.local/share/omniload` o `~/Library/Application
Support/OmniLoad`). La variable `OMNILOAD_DB` usa otro archivo. SQLite va
compilado dentro del programa (`rusqlite` con `bundled`): no hay que instalar
nada.

## Los tres programas

- `omniload`: la ventana.
- `omnid`: el servicio que descarga. Tiene la cola y la ofrece por una API
  HTTP local (ver abajo). La ventana y la CLI lo lanzan solos si no está en
  marcha, buscándolo en la misma carpeta que ellos.
- `omni`: la terminal.

Si la ventana no encuentra `omnid` (por ejemplo, con `cargo run -p omni-gui`
sin haberlo compilado), arranca el mismo servicio dentro de ella: todo va
igual, pero las descargas se paran al cerrarla.

Cerrar o minimizar la ventana la esconde en la bandeja del sistema (si el
escritorio tiene bandeja) y las descargas siguen. Al salir de verdad («Salir»
en el menú del icono, o cerrar la ventana si no hay bandeja) se para el
servicio (guardando el progreso) salvo que esté activado "Seguir descargando
al salir". `omnid` escribe su
registro en `omnid.log`, junto a la base de datos; para verlo en directo,
ejecútalo en una terminal (`RUST_LOG=debug omnid`; Ctrl+C lo para).

### Quitar descargas, velocidad estable y versión

- Quitar: `Queue::remove` solo saca de la lista; `Queue::remove_with_files`
  (API `POST /jobs/remove` con `{"ids":…, "files":true, "keep":"<carpeta de
  descargas>"}`, CLI `omni remove --archivos`) borra también el archivo, el
  `.part`, el estado de reanudar y la carpeta del paquete **solo si queda
  vacía** y no es la de descargas ni una carpeta personal del sistema.
- `apps/omni-gui/src/smooth.rs`: la velocidad y el tiempo restante que se ven
  cambian una vez por segundo (media más lenta que la del motor); el tiempo
  restante cuenta hacia atrás entre actualizaciones y se redondea (1 s hasta
  1 min, 5 s hasta 1 h, 1 min después).
- `apps/omni-gui/src/update.rs`: al abrir pide `releases/latest` a la API de
  GitHub (una sola petición, con `User-Agent: OmniLoad/<versión>`), compara
  versiones y avisa; nunca descarga ni instala. Se desactiva en Ajustes ›
  Versión. Sin releases publicadas (404) lo dice sin dar error.

### Bandeja, ventana única y notificaciones

- `apps/omni-gui/src/tray.rs`: el icono. Windows y macOS usan `tray-icon`
  (Win32 y AppKit); Linux y BSD, `ksni` (StatusNotifierItem por D-Bus, sin
  GTK: lo muestran KDE, XFCE, Cinnamon, MATE, LXQt y GNOME con la extensión
  «AppIndicator»). Si no hay bandeja, cerrar la ventana sale como antes. Si la
  bandeja desaparece con la ventana escondida, la ventana vuelve sola.
- `apps/omni-gui/src/single.rs`: solo una ventana. La que está en marcha
  escucha en `127.0.0.1` y apunta puerto y token en `ventana.json`, junto a
  `ajustes.json`; abrir `omniload` otra vez le pasa los argumentos (enlaces o
  archivos) y la enseña.
- `apps/omni-gui/src/notify.rs`: notificación del sistema al terminar un
  archivo o un paquete, o si falla (con `notify-rust`), solo si la ventana no
  está a la vista. Mientras la ventana está escondida no se pintan las tablas.

Para probar la bandeja de Linux sin escritorio hace falta un
`org.kde.StatusNotifierWatcher` en el bus de sesión (por ejemplo, uno de
prueba hecho con `zbus`); con él, `dbus-send ... org.kde.StatusNotifierItem.Activate`
equivale al clic y `com.canonical.dbusmenu.Event` a elegir en el menú.

### Horario, reglas por tipo, acción al terminar y columnas

- **Horario** (`omni-queue/src/schedule.rs`): `Schedule { enabled, days, from_min,
  to_min }` va dentro de `QueueSettings`. El tramo pertenece al día en que
  empieza; si `to <= from` cruza la medianoche (`from == to` son 24 h). El
  `ticker` de la cola mira el reloj local cada tick: al salirse del horario
  pausa lo que corre (guardando progreso) y no arranca nada (`sched_blocked`,
  independiente del «Pausar» del usuario; el estado dice «En pausa»); al entrar
  de nuevo, `pump` sigue solo.
- **Reglas por tipo de archivo** (`omni-grab`): `Dest.rules` es una lista de
  `FileRule { extensions, folder }`; gana la primera que coincide y un paquete
  con carpeta propia no se toca. Las reglas se guardan en `ajustes.json` de la
  ventana.
- **Acción al terminar** (`omni-gui/src/automation.rs`): `FinishWatch` detecta
  el final de la tanda (no salta si se pausó o se quitó algo); suspender y apagar
  dan 60 s con un aviso y botón «Cancelar». **Iniciar con el sistema**: clave
  `Run` de HKCU en Windows, LaunchAgent en macOS, `autostart/omniload.desktop`
  en Linux; arranca con `--minimizado` (a la bandeja).
- **Lista con miles de filas**: `POST /api/v1/jobs/changes` devuelve solo lo
  que cambió desde la última vez que preguntó esa ventana (`JobsDelta`: el
  orden si cambió y las filas modificadas; el servicio recuerda la última foto
  de cada ventana 60 s). `RemoteQueue` lo aplica y pide todo si algo no cuadra.
  La ventana solo rehace la tabla si la cola cambió y agrupa por paquete con un
  `HashMap` (antes era cuadrático). Con 10.000 filas en una compilación
  release, la ventana gasta ~20 % de un núcleo y el servicio ~7 %.
- **Interfaz**: tipografía Inter incluida (`ui/fonts`, licencia OFL), barra de
  título propia (`no-frame`, mover y redimensionar con `drag_window` y
  `drag_resize_window` de winit), `ComboBox` y `SpinBox` propios con la misma
  interfaz que los estándar, columnas en `DlCols` (modelo `ColState` rellenado
  desde `Settings.columns`), `Splitter` para los anchos de panel y escala de
  la interfaz con `SLINT_SCALE_FACTOR` (se fija al arrancar).

## Línea de comandos (`omni`)

Con el servicio (la misma lista que ve la ventana; lo lanza si hace falta):

```sh
omni add <url>... -d Descargas -p "Mi paquete"   # a la cola (--parada: sin empezar)
omni list                                        # lista con estado y velocidad
omni pause [id...]     omni resume [id...]       # sin ids: todas
omni remove <id...>                              # quita de la lista (no borra archivos)
omni limit 2M          omni limit off            # límite global
omni limit 500K --id 3                           # límite de una descarga
omni grab <url o archivo.txt>...                 # al capturador de la ventana
omni service status    omni service stop
```

Sin servicio, para comprobar enlaces sin descargar (nombre, tamaño, si están
disponibles; sale con error si alguno está caído):

```sh
omni check https://ejemplo.com/a.zip lista.txt
```

Sin servicio, descargando directamente en la terminal (lo mejor para depurar,
porque muestra los errores):

```sh
omni get https://ejemplo.com/archivo.iso              # a la carpeta actual
omni get https://ejemplo.com/archivo.iso -d Descargas # a otra carpeta
omni get https://ejemplo.com/archivo.iso -o copia.iso # nombre exacto
omni get <url> -x 16 --sha256 <hash>                  # 16 conexiones y verificación
```

Ctrl+C pausa la descarga. Volver a ejecutar el mismo comando la reanuda donde
se quedó, también tras un cierre forzado o un corte de luz.

## Cómo descarga el motor

1. **Sondeo**: un `GET` con `Range: bytes=0-0` averigua tamaño, nombre, ETag,
   Last-Modified y si el servidor acepta rangos.
2. **Reserva**: se crea `archivo.part` con su tamaño final, para detectar la
   falta de espacio al principio.
3. **Cola de bloques**: el archivo se reparte en bloques de 8 MiB. Empiezan 8
   conexiones repartidas a lo largo del archivo; cada una pide con una sola
   petición HTTP todo el tramo libre que tiene por delante y va encadenando
   bloques mientras el siguiente siga libre. Así la conexión más rápida
   descarga más y el servidor recibe pocas peticiones (algunos limitan cuántas
   aceptan por minuto).
4. **Final rápido**: cuando no quedan bloques libres, una conexión ociosa parte
   por la mitad el bloque al que más le falta (mínimo 1 MiB por mitad) o, si ya
   es pequeño, compite con él; gana la primera que acabe.
5. **Escritura directa**: cada conexión escribe en su posición del archivo
   final (`pwrite` en Unix, `seek_write` en Windows); no hay paso de "unir
   partes".
6. **Reanudación**: cada medio segundo se hace `fsync` del `.part` y se guarda
   el progreso en un archivo de estado que **no** va en la carpeta de
   descargas (antes era `archivo.part.omni`, que aparecía y desaparecía sin
   parar), sino en la carpeta de datos del programa: `reanudar/<nombre>-<huella
   de la ruta>.json` (`%LOCALAPPDATA%\OmniLoad`, `~/.local/share/omniload` o
   `~/Library/Application Support/OmniLoad`; `OMNILOAD_REANUDAR` la cambia).
   Si queda un `.part.omni` de una versión anterior, se adopta al reanudar.
   Al reanudar se comprueba que tamaño,
   ETag y Last-Modified no hayan cambiado; si cambiaron, se empieza de cero.
   Además cada petición lleva `If-Range`, así que un cambio a mitad de
   descarga se detecta en vez de mezclar dos versiones del archivo.
7. **Reintentos**: cortes de red, 5xx y 429 se reintentan con espera
   exponencial (1, 2, 4… hasta 60 s, respetando `Retry-After`). 403, 404 y 410
   marcan el enlace como caducado.
   Si el servidor rechaza conexiones por exceso (429 o 503 con varias
   abiertas), se baja al número que acepta, se recuerda ese máximo y se
   trabaja con él sin volver a probar por encima.
   Siempre HTTP/1.1: cada conexión es una conexión TCP propia.
8. **Cierre**: se comprueba el tamaño (y el SHA-256 si se pidió) y se renombra
   `.part` al nombre final.

Si el servidor no acepta rangos, se descarga en un solo flujo sin reanudación.

### Límites compartidos entre descargas

- **Velocidad** (`RateLimit`): un cubo de fichas. Cada conexión descuenta lo
  que recibe y, si se pasa, espera; al no leer del socket, TCP frena al
  servidor solo. `DownloadOptions::rate_limits` admite varios a la vez (la
  cola usa uno global y otro por descarga) y se cambian en caliente.
- **Conexiones por servidor** (`ServerLimits`): el límite adaptativo ante
  429/503 es por servidor y red, compartido por todas las descargas, porque
  el servidor cuenta conexiones por IP. Encima va el tope del usuario. Lo
  aprendido se guarda en SQLite y vale un mes.
- **Reparto justo**: si una descarga espera hueco con menos de su parte
  (límite / descargas), las que tienen más que su parte no cogen huecos
  nuevos y sueltan una conexión en cuanto vuelcan a disco. Así una descarga
  lenta no deja sin conexiones a las demás.

## La cola (`omni-queue`)

Prioridades, descargas a la vez, pausa global, límites y persistencia. Vive
en el servicio `omnid`. Detalles:

- Se guarda en SQLite (`omni-store`) cada cambio de estado y, cada 2 s, el
  progreso. El progreso fino (qué bloques faltan) sigue en el archivo de estado de arriba:
  una descarga se reanuda aunque se borre la base de datos.
- Al cerrar, lo que corre se pausa guardando el progreso y queda "en espera";
  al abrir, arranca solo.
- Se guarda siempre el enlace original. Antes de cada arranque, un
  `Resolver` lo convierte en enlace directo (con cabeceras y nombre si hace
  falta). Hoy solo existe `DirectLinks`; los plugins de Mega, Gofile, Google
  Drive y Mediafire serán resolutores, porque sus enlaces directos caducan.
- Cada descarga guarda sus redes (vacío = la del sistema) para la multi-red.

## La API local (`omni-api`)

- `omnid` escucha en `127.0.0.1`, en un puerto libre, y escribe `omnid.json`
  (puerto, token, pid y versión) junto a la base de datos. En Unix ese
  archivo es legible solo por el usuario.
- Cada petición lleva `Authorization: Bearer <token>`: otra web abierta en el
  navegador no puede mandar órdenes.
- `omnid.lock` (candado del sistema) impide dos servicios con la misma base de
  datos.
- Rutas, todas bajo `/api/v1` y en JSON:

| Método | Ruta | Qué hace |
|---|---|---|
| GET | `/status` | versión, pid y ajustes |
| GET | `/jobs` | la lista |
| POST | `/jobs` | añadir (`NewJob`) |
| POST | `/jobs/start`, `/jobs/stop`, `/jobs/remove` | `{"ids": [..]}` |
| POST | `/jobs/priority` | `{"ids": [..], "priority": "high"}` |
| POST | `/jobs/speed-limit` | `{"ids": [..], "bytes_per_sec": 1048576}` |
| POST | `/queue/start-all`, `/queue/pause-all`, `/queue/stop-all` | botones de la barra |
| GET/PUT | `/settings` | ajustes de la cola |
| GET | `/servers` | conexiones por servidor |
| GET | `/grabber` | el capturador: paquetes, archivos y avisos |
| POST | `/grabber/links` | `{"text": "...", "new_batch": true}` → `{"added", "batch"}` |
| POST | `/grabber/command` | `{"op": "select_item", "id": 3, "selected": false}`, `rename_package`, `recheck`… (`GrabCommand`) |
| POST | `/grabber/take` | `{"pick": "selected", "dest": {..}, "start": true}`: a la cola |
| POST | `/shutdown` | parar el servicio |

- La ventana usa `RemoteQueue` y `RemoteGrabber`: preguntan cuatro veces por
  segundo en segundo plano y mandan las órdenes sin bloquear su hilo.

## El capturador (`omni-grab`)

- Vive en `omnid`, junto a la cola: lo que llega desde la ventana, la
  terminal (`omni grab`) o, más adelante, el navegador va a la misma lista. Se
  guarda en la base de datos cada vez que cambia (como mucho cada 2 s) y al
  parar.
- Comprobar un enlace cuesta una petición: `GET` con `Range: bytes=0-0`; la
  respuesta dice si es página (`text/html`) o archivo, y de un archivo su
  tamaño, nombre y si acepta rangos (`omni_core::info_from_response`, lo mismo
  que usa el motor). De una página se sacan los enlaces a archivos, un nivel.
- Hasta 24 comprobaciones a la vez, y 8 como mucho por servidor (o lo que
  diga su plugin) para no provocar bloqueos. Cien enlaces a un servidor que
  tarda 100 ms en contestar: ~1,3 s (`hundred_links_quickly`).
- Los enlaces que ya están en la lista de descargas (mismo enlace) salen
  desmarcados y con el aviso "Ya está en Descargas".
- Al copiar, además del texto se lee el HTML del portapapeles: los enlaces
  escondidos detrás de palabras se añaden si parecen archivos o son de hosts
  conocidos (`FILE_HOSTS`), para no traer los menús de una web al copiar un
  párrafo. El `#fragmento` se conserva (en Mega es la clave).
- Archivos de enlaces: `.txt`, `.html`, `.crawljob`. Los `.dlc` de JDownloader
  van cifrados con una clave que solo da su servidor: se explica y no se
  abren.

## Los hosts (`omni-hosts`, fase 5)

Cada host es un tipo que implementa `omni_hosts::Host` y sirve a la vez de
`HostChecker` (capturador: reconoce, comprueba, devuelve carpetas) y de
`Resolver` (cola: enlace directo justo antes de bajar). `Hosts::standard()`
los registra todos: Pixeldrain, Archive.org, Dropbox, Mediafire, Google Drive,
Gofile, Mega, yt-dlp, OneDrive, 1fichier y torrents. `omni-api` y `omni` lo
usan; para añadir uno, crea el módulo y añádelo ahí (y a la prueba de la lista
estándar en `tests/hosts.rs`).

Un `ResolvedLink` puede traer, además de URL y cabeceras, uno de dos motores
que no son el HTTP por bloques:

- `external`: un programa externo (yt-dlp). Habla por stdout con líneas
  `OMNI-PROGRESS` y `OMNI-FILE`; se mata al cancelar y los errores llevan la
  cola de stderr (`omni-queue/src/external.rs`).
- `custom`: un motor propio (`CustomDownload`): torrents con librqbit (una sola
  sesión) y Mega (descifrado AES al vuelo, sin comprobar el MAC).

Las páginas de los hosts cambian: Gofile (token), Mediafire y 1fichier (HTML)
son lo más frágil. Las pruebas usan servidores falsos (`tests/hosts.rs`,
`tests/mega.rs`); la de torrents se salta si no hay IPv6 (librqbit lo exige).

El cliente HTTP es el de una `Network`, así que la multi-red no cambia nada.

## Instaladores y actualización

`.github/workflows/release.yml` construye, al subir una etiqueta `vX.Y.Z`
(tiene que coincidir con la versión de `Cargo.toml`), el instalador de Windows
(Inno Setup, por usuario), el `.dmg` de macOS (firma ad-hoc) y los `.deb`,
`.tar.gz` y `.AppImage` de Linux, y los publica en la Release con
`SHA256SUMS.txt`. En los PR que tocan `installer/` solo se construyen, sin
publicar. Los scripts están en `installer/`.

Dentro de la app (`apps/omni-gui/src/update.rs`), «Actualizar ahora» elige el
archivo del sistema, lo baja a una carpeta temporal, comprueba su SHA-256 con
`SHA256SUMS.txt`, lo ejecuta (instalación silenciosa en Windows) y reabre la
app. Solo sigue enlaces de GitHub y solo con la aceptación del usuario.

### Preparado para varias redes

La descarga multi-red irá al final del plan, pero el motor ya trabaja con
varias redes:

- `DownloadOptions::networks` acepta una lista de `Network`, cada una con su
  propio cliente HTTP, y el motor abre sus conexiones en cada una.
- La cola de bloques sabe qué red descarga cada bloque y, al competir por el
  último trozo, prefiere ayudar a otra red.
- El límite de conexiones y el progreso van por red.
- La prueba `splits_blocks_between_networks` descarga con dos redes a la vez.

Lo que faltará entonces es descubrir las interfaces del equipo y construir el
cliente HTTP de cada una atado a su interfaz (en `platform/`), sin tocar la
cola ni el motor.

## Estructura

```
crates/
  omni-core/        motor de descarga (biblioteca)
    src/platform/   todo el código propio de cada sistema operativo
  omni-grab/        capturador de enlaces: comprobación, paquetes
  omni-hosts/       plugins de hosts (Mega, Drive, torrents, yt-dlp…)
  omni-store/       base de datos SQLite (lista, ajustes, servidores)
  omni-queue/       cola de descargas con límites y persistencia
  omni-api/         servicio (API local) y sus clientes
  omni-testserver/  servidor HTTP falso para las pruebas (cortes, 503, ETag)
apps/
  omni-cli/         binario `omni`
  omnid/            binario `omnid`, el servicio
  omni-gui/         binario `omniload`, la ventana (Slint)
    ui/app.slint    diseño de la ventana
```

## Desarrollo

```sh
cargo test --workspace      # pruebas, incluida la de matar el proceso y reanudar
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p omni-cli   # target/release/omni(.exe)
cargo build --release -p omni-gui   # target/release/omniload(.exe)
```

## Comprobaciones

Antes de subir cambios, pasa en tu equipo las mismas comprobaciones que hace
el CI (GitHub Actions compila y prueba en Windows, Linux y macOS):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Versiones por PR y columnas (PR #9)

- **Regla de versiones**: cada PR sube la versión de `[workspace.package]` en
  `Cargo.toml`. Mientras el PR no está aprobado lleva el sufijo `-prN`
  (`0.2.0-pr9`) y Ajustes › Versión lo marca como «versión del PR sin
  aprobar». En el último commit antes de fusionar se quita el sufijo.
  `update.rs` trata el sufijo como versión previa, así que `0.2.0-pr9` nunca
  se considera más nueva que `0.2.0`.
- **Columnas**: `settings::Table` (Descargas, Capturador) y `ColCfg` con
  `id` 0 = Nombre, que se mueve y se ajusta como las demás (no se oculta).
  `fix_columns` repara lo guardado (añade Nombre al principio si falta). En
  Slint, `ColsHeader` calcula el ancho con la posición absoluta del ratón (sin
  saltos) y el arrastre de cabecera llama a `reorder(origen, destino)`.
- **Ventana fantasma en Windows**: era la consola de `reg query` al guardar
  ajustes; ahora se usa `CREATE_NO_WINDOW` y solo se consulta al cambiar la
  casilla.
