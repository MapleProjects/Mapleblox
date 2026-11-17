# Reemplazo en Rust del camino de texturas ETC2

Objetivo: quitar de la ruta caliente las dos construcciones que dejaban un fence
sin señalizar, y recuperar (o mejorar) el rendimiento base que daba el módulo
C++ actual.

## Qué se elimina y por qué

El módulo `src/graphics/vulkan_etc2_emulation.cc` hace dos cosas que rompen el
modelo de sincronización de Vulkan:

1. **Inyecta sus propios command buffers** dentro del submit de la aplicación
   (`PrepareEtc2Submit`, `FinishEtc2Encoder`) y **reescribe las barreras de la
   aplicación** para cubrir los mips privados que crea (`RemapBarriers`).
2. **Espera un semáforo de timeline** que señaliza su dispatcher con un "ticket"
   (`SignalDecoded`) desde otro hilo.

Si ese ticket no llega, o una barrera reescrita deja el orden de dependencias
incompleto, el fence que espera la aplicación nunca se señaliza. El driver
agota su propio timeout (12 s con NVIDIA 620.70) y devuelve
`VK_ERROR_DEVICE_LOST`. Encaja con lo medido: CPU idle, 12 s exactos, ningún
Xid del kernel, y desaparición total de los bloqueos al desactivar el módulo.

Cambiar de lenguaje no arregla eso. Lo arregla el diseño: **cero inyección,
cero esperas cruzadas**.

## Diseño propuesto

El nuevo camino en Rust nunca toca los command buffers ni las barreras de la
aplicación. Convierte las texturas en CPU, en paralelo, y escribe el resultado
en el staging buffer que la aplicación ya usa. La aplicación copia con su propio
comando y su propia barrera, como si fueran texturas normales.

```
ETC2 comprimido (staging de la app)
      │
      ▼  mapleblox_etc2_decode_image()      [Rust, por bloques, multi-hilo]
RGBA8 o BC7 (staging nuestro, anillo de buffers)
      │
      ▼  vkCmdCopyBufferToImage de la app (sin cambios)
imagen del guest (formato que la app ya declaró)
```

Invariantes que se garantizan por construcción:

- Ningún `vkQueueSubmit` ajeno al de la aplicación.
- Ninguna barrera de la aplicación se reescribe.
- Ningún valor de timeline que la aplicación espere lo señaliza otro hilo; si se
  usa trabajo asíncrono, se sincroniza con un `VkFence` **nuestro**, esperado
  solo por nosotros, nunca dentro del submit del guest.
- Si un trabajo falla, se descarta y la textura se sube sin convertir; jamás se
  deja una espera pendiente.

## Rendimiento

- Decodificador ETC2/EAC en Rust con núcleos AVX2, por bloques de 4x4, paralelo
  entre núcleos. Es trabajo CPU puro, sin bloqueos del driver.
- Se puede mantener la compresión a BC7 para no subir VRAM (bloques de 16 B por
  4x4, igual que ETC2), o subir RGBA sin comprimir cuando sobre memoria. La
  decisión pasa a ser una política medida (VRAM libre, ancho de banda), no un
  camino fijo.
- Al no haber pases inyectados ni remapeo de barreras, desaparecen los picos de
  12 s y la cola de barreras que hoy se ve en el log (412 barreras remapeadas en
  8,1 s).

## Fases

1. **Crate `rust/etc2` con el decodificador y pruebas diferenciales.** El
   decodificador C++ actual (`src/graphics/etc2_decoder.cc`) es la referencia:
   se generan bloques aleatorios y se compara RGBA byte a byte contra él. Sin
   esa prueba no entra nada.
2. **Enlace por C ABI**, igual que `rust/policy`: staticlib que se compila con
   cargo y se enlaza a `mocktail_runtime`.
3. **Sustitución del camino de subida** detrás de una opción, con el módulo
   viejo aún disponible para comparar.
4. **Medición**: stalls, VRAM y fps con el módulo viejo, con el módulo apagado y
   con el nuevo. Solo se adopta si gana en los tres.

## Estado

- [x] Crate creado, ABI establecida.
- [x] Decodificador completo: ETC2 RGB, RGB8A1 (punch-through), RGBA8 con alfa
      EAC, y EAC R11/RG11 con y sin signo.
- [x] Pruebas diferenciales contra el C++: 49 casos, 7 formatos, bloques
      pseudoaleatorios que cubren modo individual, diferencial, T y H. Comparación
      byte a byte, en verde.
- [x] Decodificación en paralelo por bandas con hilos con ámbito, sin locks.
- [x] Integración en el runtime detrás de `MOCKTAIL_ETC2_RUST_DECODE`.
- [ ] Packer BC7/BC4/BC5.

## Cómo se activa

    MOCKTAIL_ETC2_RUST_DECODE=1 mocktail

El modo hace cuatro cosas a la vez, y las cuatro son necesarias para que no
quede ninguna espera cruzada:

1. `DecodeEtcImageBlockRows` delega en `mapleblox_etc2_decode_block_rows`, así
   que el decodificador C++ queda solo como referencia y como respaldo si la
   ABI no coincide.
2. `DecodesOnCpu` devuelve verdadero para toda subida: no se graban pases de
   decodificación en GPU, así que no hay command buffers ajenos que inyectar.
3. `CompressesTextures` devuelve falso: sin BC7 no hay imagen comprimida en el
   host, y por tanto ninguna subida necesita el encoder de GPU. Las texturas
   suben como RGBA8/R16.
4. `PrepareSubmit` no toma el camino asíncrono: decodifica en el hilo que envía
   y devuelve un submit sin semáforo de timeline nuestro. Ningún fence del
   cliente puede quedarse esperando un ticket que no llega.

El decodificador C++ sigue siendo el que corre sin la variable, y los suites
`vulkan_etc2_emulation_test` y `vulkan_etc2_gpu_emulation_test` fijan el modo
viejo para seguir describiéndolo.

## Medición en partida (RTX 3050 Mobile, i7-11800H, 23 minutos)

Place 71278628540517, `--profile`, 162.963 frames presentados.

    fps medio                 117,1
    frame time p50            7,94 ms
    frame time p90           11,29 ms
    frame time p99           19,52 ms
    frame time p99.9         26,10 ms
    frame time max          533,50 ms   (carga de texturas)
    frames > 100 ms              16
    frames > 1000 ms              0
    vkWaitForFences max      13,24 ms

Congelamientos registrados por `stall_recorder`, mismo archivo antes y después:

    motor viejo   215 eventos, 10 de ellos de 12,01-12,03 s, 29 entre 1 y 10 s
    motor Rust     14 eventos, ninguno por encima de 1 s, todos en la carga

El máximo de `vkWaitForFences` es la medida que importa: el fallo viejo era un
fence esperando 12 s, y en 163 mil submits no pasa de 13 ms.

Coste del decodificador: 2.074 slices, 3,49 s de CPU en 23 minutos, p50 0,80 ms.
El peor es el lote de carga (2.841 subidas, 52 MB comprimidos a 315 MB de
texels, 323 ms). En régimen estable el pico es 29 ms.

Coste en VRAM: 880 MiB de texturas emuladas en GPU, frente a la cuarta parte
que ocuparían comprimidas en BC. Con 2,4 GiB libres no aprieta en esta máquina,
pero es lo que devolvería el packer BC7 en CPU.

## Rendimiento medido (1024x1024, 65536 bloques, 16 núcleos)

    C++ de referencia, un hilo        5,2 ms   200 Mtexel/s    772 MiB/s
    Rust, un hilo                     7,4 ms   142 Mtexel/s    542 MiB/s
    Rust, 16 hilos                    1,7 ms   620 Mtexel/s   2364 MiB/s

El port en un hilo es algo más lento que el C++ porque este último usa
`pshufb` para la paleta; el paralelo lo supera por 3x y deja margen de sobra
para alimentar el streaming de texturas sin tocar la GPU.

