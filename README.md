# OmniLoad

Gestor de descargas de escritorio escrito en Rust, pensado como alternativa
rápida y ligera a JDownloader. Funciona en Windows, Linux y macOS.

> Estado: **fase 1**, núcleo por línea de comandos, y una primera versión de la
> interfaz nativa (Slint).

## Interfaz gráfica

```sh
cargo run --release -p omni-gui            # abre la ventana
cargo run --release -p omni-gui -- <url>   # y analiza ese enlace
```

El ejecutable queda en `target/release/omniload(.exe)`. Funciona como el
capturador de enlaces de JDownloader:

1. Pega uno o varios enlaces (sueltos o dentro de un texto) y pulsa
   **Analizar enlaces**.
2. La pestaña **Capturador de enlaces** enseña los archivos encontrados, con
   nombre, tamaño y si están disponibles, antes de descargar nada. Si el enlace
   es una página web, aparecen los archivos enlazados desde ella (un nivel, sin
   seguir otras páginas).
3. Marca los que quieras, elige carpeta y conexiones y pulsa **Descargar
   seleccionados**.

La pestaña **Descargas** muestra progreso, velocidad, tiempo restante,
conexiones activas (y el límite si el servidor rechaza conexiones de más) y
los botones de pausar y reanudar.

Pausar guarda el progreso; reanudar sigue donde se quedó. Al cerrar la ventana
se pausa todo. La lista aún no se guarda entre sesiones (llegará con
`omni-store` en la fase 2), pero volver a añadir el mismo enlace en la misma
carpeta reanuda desde el `.part`.

En Linux hacen falta `libfontconfig` y `libxkbcommon-x11` (vienen en cualquier
escritorio). Para compilar: `sudo apt install libfontconfig-dev libxkbcommon-dev`
en Debian/Ubuntu.

## Línea de comandos

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

## Cómo descarga

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
  omni-grab/        capturador de enlaces: archivo o página con archivos
    src/platform/   todo el código propio de cada sistema operativo
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

Antes de subir cambios, pasa estas tres comprobaciones en tu equipo (son las
mismas que haría el CI):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Además, GitHub Actions compila y prueba en Windows, Linux y macOS en cada
cambio.

## Licencia

GPL-3.0-or-later. Ver [LICENSE](LICENSE).
