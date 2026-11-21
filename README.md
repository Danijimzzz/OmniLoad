# OmniLoad

Gestor de descargas de escritorio escrito en Rust, pensado como alternativa
rápida y ligera a JDownloader. Funciona en Windows, Linux y macOS.

> Estado: **fase 1**, núcleo por línea de comandos. La interfaz nativa (Slint)
> llega en la fase 4.

## Uso

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

Cada conexión pertenece a una `Network`. Hoy solo existe la red por defecto del
sistema, pero la cola de bloques ya sabe qué red descarga cada bloque y, al
competir por el último trozo, prefiere ayudar a otra red. La descarga
multi-red (fase 2b) solo tendrá que añadir redes atadas a una interfaz.

## Estructura

```
crates/
  omni-core/        motor de descarga (biblioteca)
    src/platform/   todo el código propio de cada sistema operativo
  omni-testserver/  servidor HTTP falso para las pruebas (cortes, 503, ETag)
apps/
  omni-cli/         binario `omni`
```

## Desarrollo

```sh
cargo test --workspace      # pruebas, incluida la de matar el proceso y reanudar
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p omni-cli   # target/release/omni(.exe)
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
