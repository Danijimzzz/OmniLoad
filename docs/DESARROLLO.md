# Desarrollo de OmniLoad

Notas técnicas para quien quiera compilar, probar o tocar el código. La
portada para usuarios es el [README](../README.md).

## Compilar

Hace falta [Rust](https://rustup.rs) 1.88 o más nuevo.

```sh
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

## Línea de comandos (`omni`)

La CLI sigue siendo la mejor forma de depurar, porque muestra los errores en la
terminal:

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
   el progreso en `archivo.part.omni`. Al reanudar se comprueba que tamaño,
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

Prioridades, descargas a la vez, pausa global, límites y persistencia. La usa
la ventana y la usará el servicio `omnid`. Detalles:

- Se guarda en SQLite (`omni-store`) cada cambio de estado y, cada 2 s, el
  progreso. El progreso fino (qué bloques faltan) sigue en el `.part.omni`:
  una descarga se reanuda aunque se borre la base de datos.
- Al cerrar, lo que corre se pausa guardando el progreso y queda "en espera";
  al abrir, arranca solo.
- Se guarda siempre el enlace original. Antes de cada arranque, un
  `Resolver` lo convierte en enlace directo (con cabeceras y nombre si hace
  falta). Hoy solo existe `DirectLinks`; los plugins de Mega, Gofile, Google
  Drive y Mediafire serán resolutores, porque sus enlaces directos caducan.
- Cada descarga guarda sus redes (vacío = la del sistema) para la multi-red.

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
  omni-grab/        capturador de enlaces: archivo o página con archivos
  omni-store/       base de datos SQLite (lista, ajustes, servidores)
  omni-queue/       cola de descargas con límites y persistencia
  omni-testserver/  servidor HTTP falso para las pruebas (cortes, 503, ETag)
apps/
  omni-cli/         binario `omni`
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
