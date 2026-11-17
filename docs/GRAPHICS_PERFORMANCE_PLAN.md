# Plan de rendimiento de gráficos

Cuatro frentes abiertos después de mover el decodificado ETC2 a Rust. Este
documento fija el orden, qué se mide en cada uno y cómo se revierte. El estado
del decodificador está en [ETC2_RUST_REPLACEMENT.md](ETC2_RUST_REPLACEMENT.md),
y la forma de medir en [BENCHMARKING.md](BENCHMARKING.md).

## Línea base

Traza de 23 minutos en partida con `MOCKTAIL_ETC2_RUST_DECODE=1`, RTX 3050
Mobile, resumida con `scripts/summarize_profile_trace.py`:

| métrica | valor |
| --- | ---: |
| frames | 585.406 |
| frame interval mean | 8,46 ms |
| frame interval p99 | 18,93 ms |
| frame interval max | 533,52 ms |
| `vkQueueSubmit` max | **324,59 ms** |
| `etc2 decode` count / total | 8.911 / 12.727 ms |
| `etc2 decode` max | **323,41 ms** |
| `vkWaitForFences` max | 71,11 ms |
| `emulated texture (MB)` peak | **1.492,10** |
| `compressed texture (MB)` peak | 232,07 |
| `vkAllocateMemory` + `vkFreeMemory` | 13,7 s en 34.892 llamadas |
| `vkCreateGraphicsPipelines` max | 101,92 ms |

Los cuatro números en negrita son los que este plan quiere mover. El
`frame interval mean` de 8,46 ms ya está en el techo de 120 Hz del monitor, así
que el objetivo no es subir la media sino quitar los picos.

## Fase 0. Icono — hecho, unificación pendiente

Hecho: `packaging/mapleblox.svg` pasa de la gradiente roja a índigo
(`#8e74e8 → #6c4fd1 → #2f2a6b`), que son los tonos del mark que ya usa la
ventana. El lanzador lo instala el PKGBUILD tal cual, sin regenerar PNG.

Pendiente, cuando se apruebe el tono: el icono de ventana sigue empotrando
`packaging/icons/hicolor/48x48/apps/io.github.CoderDayton.nightcap.png`, un mark
índigo distinto del de la ventana del lanzador. Unificarlo es generar ese PNG
desde `mapleblox.svg` con `scripts/generate_desktop_icons.sh` y apuntar
`MOCKTAIL_WINDOW_ICON_PNG` en `cmake/MocktailPlatformGraphics.cmake:15` al
archivo nuevo. Sin prisa: los dos son índigo, solo cambia el dibujo.

## Fase 1. Decodificar fuera del hilo que envía — hecho

**Qué se gana.** Quita los dos picos de la tabla: el `vkQueueSubmit` de
324,59 ms y el frame de 533,52 ms. En régimen estable quita los 16 frames por
encima de 100 ms de la corrida.

**Por qué se puede y antes no.** El motor viejo esperaba un valor de timeline
que señalizaba otro hilo; si el ticket no llegaba, el fence del cliente se
quedaba colgado. Lo que el diseño prohíbe es *eso*: una espera ajena dentro del
submit del guest. Trabajo asíncrono sincronizado con un `VkFence` **nuestro**,
esperado solo por nosotros, sí está permitido por el propio ADR.

**Diseño.**

1. El decodificado pasa a un pool persistente, el mismo `RunOnDecodeWorkers` de
   `etc2_decoder.cc`, que ya existe y ya reparte bandas entre núcleos.
2. Cada lote lleva un `VkFence` creado por nosotros. Nadie del guest lo ve.
3. El staging se convierte en anillo de dos (o tres) bloques: el lote N escribe
   en el bloque N mientras el bloque N-1 está en vuelo hacia la GPU.
4. `PrepareSubmit` espera el fence del lote anterior **solo si** ese bloque va a
   reutilizarse ahora, con lo que en el caso normal el trabajo de este frame
   salió del frame anterior y no se espera nada.
5. `ReleaseCommandBuffer` espera el fence del lote que tocaba ese command
   buffer, igual que hoy espera el ticket.
6. Si el pool no arranca o un lote falla, se decodifica en línea como ahora y
   se sube sin convertir en el peor caso. Nunca queda una espera pendiente.

**Criterio de aceptación.** `vkQueueSubmit` max por debajo de 50 ms y
`frame interval max` por debajo de 150 ms, con `frame interval p99` sin empeorar
más de un 5%. Cero eventos del `stall_recorder` por encima de 1 s.

**Riesgo.** Medio. Es tocar el mismo mecanismo que causaba el cuelgue, así que
la prueba es la misma: partida larga con el registrador de stalls activo y
`vkWaitForFences max` como vigilante. Un fence nuestro mal esperado no cuelga
al cliente, cuelga un frame.

**Reversión.** El flag `MOCKTAIL_ETC2_RUST_DECODE` sigue gobernando todo; si la
fase 1 necesita su propio interruptor, será `MOCKTAIL_ETC2_RUST_ASYNC=0` para
volver al decodificado en línea sin cambiar nada más.

## Fase 2. Packer BC7/BC1 en CPU — hecho

**Qué se gana.** El `emulated texture (MB) peak` de 1.492,10 baja a la cuarta
parte, entre 370 y 400 MB. Es el frente que más cambia para tarjetas de 4 y 6
GB, donde 1,5 GB de texturas es un cuarto de la VRAM. No cambia la velocidad.

**Diseño.** Después del decodificado ETC2, el mismo pool empaqueta a BC7 las
texturas con alfa y a BC1 las opacas. La imagen del host vuelve a ser BC, pero
alimentada por CPU: el encoder de GPU y su self-test no vuelven. El modo deja de
forzar `CompressesTextures` a falso y pasa a forzar `DecodesOnCpu`, con lo que
`record.transcoded` puede ser verdadero sin que exista un pase en GPU.

**Aviso de orden.** Esto depende de la fase 1. Empaquetar BC7 es entre 3 y 10
veces más caro que decodificar ETC2; hacerlo en el hilo que envía cambiaría un
pico de 324 ms por otro peor.

**Criterio de aceptación.** `emulated texture (MB) peak` por debajo de 450 MB,
con `etc2 decode` (más empaquetado) por debajo de 100 ms de máximo y sin
regresión en `frame interval p99`.

**Riesgo.** Alto en calidad visual, bajo en estabilidad. BC7 es con pérdida y
hay que mirar el resultado en texturas con alfa y en degradados; BC1 hace
banding visible en cielos. Antes de adoptarlo, comparar capturas del mismo
encuadre con y sin packer.

**Reversión.** `MOCKTAIL_ETC2_BC_TRANSCODE=0` ya existe y devuelve el camino
RGBA8 sin tocar código.

## Fase 3. Pipelines en segundo plano — sin trabajo que hacer

**Qué se gana.** Un pico de 101,92 ms y 232 llamadas que suman 254 ms. Poco
total, pero un tirón de 100 ms se nota.

**Diseño.** Dos partes, en este orden:

1. Persistir el blob de `vkCreatePipelineCache` entre sesiones. Ya se crea el
   objeto; si el blob no se guarda, cada arranque compila de cero.
2. Compilar en un hilo aparte los pipelines que el juego pide durante la carga,
   devolviendo el handle cuando esté listo, con el `vkCreateGraphicsPipelines`
   en línea como respaldo si el juego lo usa en el mismo frame.

**Criterio de aceptación.** `vkCreateGraphicsPipelines` max por debajo de 20 ms
en la segunda sesión con el mismo contenido.

**Riesgo.** Bajo, si se hace solo la parte 1. La parte 2 toca la semántica de
creación y puede romper si Roblox usa el pipeline antes de que exista.

## Fase 4. Suballocator de memoria — hecho, tras `MOCKTAIL_SUBALLOCATE_MEMORY`

**Qué se gana.** 13,7 s de CPU en 34.892 llamadas, un 0,4% de una corrida de una
hora. Menos llamadas al kernel y menos fragmentación.

**Por qué va al final.** Es el que menos se nota y el que más puede romper. Un
suballocator tiene que respetar `VkMemoryRequirements`, la alineación por
`bufferImageGranularity`, `VK_WHOLE_SIZE` al mapear, y las asignaciones
dedicadas que el guest puede pedir con `VK_KHR_dedicated_allocation`. Un error
ahí no da un frame lento, da memoria corrupta.

**Diseño mínimo.** Pool solo para las clases de tamaño dominantes, con free
list, y solo para asignaciones que el guest libera de verdad (los conteos de
`vkAllocateMemory` y `vkFreeMemory` son casi iguales, así que el patrón es
1:1). Todo detrás de un flag, apagado por defecto.

**Criterio de aceptación.** `vkAllocateMemory` + `vkFreeMemory` por debajo de
2 s en la misma corrida, sin un solo fallo nuevo en el log.

**Reversión.** Flag propio, apagado por defecto. No se adopta hasta que la
corrida larga salga limpia.

## Resultados medidos

Cuatro corridas en la misma máquina, resumidas con
`scripts/summarize_profile_trace.py`. La línea base son 23 minutos en una
experiencia ligera; las otras tres son el jardín (`place 10707252093`), una
escena bastante más pesada, y por eso **el `frame interval` no se puede comparar
entre columnas**: solo son comparables las llamadas al driver y la VRAM, que no
dependen de lo que dibuje el juego.

| métrica | base (RGBA8) | fase 1 | fase 2 | fase 2 + 4 |
| --- | ---: | ---: | ---: | ---: |
| `emulated texture (MB)` peak | 1.492,10 | 568,81 | **158,47** | **161,44** |
| `compressed texture (MB)` peak | 232,07 | 89,99 | 87,68 | 90,97 |
| `vkQueueSubmit` max | 324,59 | 118,03 | 1.016,44 | 1.074,22 |
| `etc2 decode` count | 8.911 | 366 | 197 | 151 |
| `etc2 decode` max | 323,41 | **24,19** | 91,97 | 81,16 |
| `vkAllocateMemory` | 17.719 / 7.152 ms | 1.912 / 667 ms | 749 / 267 ms | **693 / 157 ms** |
| `vkFreeMemory` | 17.173 / 6.574 ms | 1.912 / 664 ms | 749 / 437 ms | **379 / 289 ms** |
| `vkCreateGraphicsPipelines` max | 101,92 | 0,57 | 0,85 | 0,62 |

**Fase 1, cumplida en régimen.** `etc2 decode` pasa de 323,41 a 24,19 ms y el
submit de 324,59 a 118,03 ms, y en el jardín el submit máximo es de 19,48 ms. El
criterio de 50 ms se cumple en juego pero no en el lote de carga, que sigue
siendo el peor evento de la corrida.

**Fase 2, cumplida de sobra en VRAM.** El pico baja de 1.492 a 158 MB, un 89%
menos, muy por debajo del objetivo de 450 MB. El precio está en la carga: el
decodificado más empaquetado pasa de 24 a 92 ms de máximo y el submit de carga
de 118 ms a 1.016 ms, porque empaquetar BC7 cuesta del orden de diez veces más
que decodificar ETC2 y el submit espera a que su lote esté empaquetado. Es un
tirón único al entrar, con cero eventos del `stall_recorder` después, no una
regresión de juego.

**Fase 3, sin trabajo que hacer.** El cliente persiste su propio blob de
`vkCreatePipelineCache` (creció de 8,05 a 8,38 MB entre corridas) y no hay
`vkGetPipelineCacheData` nuestro. El máximo de `vkCreateGraphicsPipelines` es de
0,57 ms en régimen y los 101,92 ms de la base eran tres pipelines nuevos
compilados sin caché a los 43 minutos. No se toca nada.

**Fase 4, cumplida.** Memoria más liberación pasan de 13,7 s a 446 ms, un 97%
menos, con el criterio de 2 s holgado. De las 693 asignaciones de la corrida,
511 (el 74%) se sirven del pool y suman 0,6 ms en total; las 182 restantes son
bloques reales de 32 MB que cuestan 0,86 ms de media. El resto de la mejora
frente a la base viene de que esta corrida es más corta y de que la imagen host
comprimida pide menos memoria al driver.

**El frente abierto que queda es el pico de carga.** No lo arregla ninguna de
las cuatro fases: el `vkQueueSubmit` de ~1 s al entrar es el submit esperando a
que se empaquete su lote. Las salidas son empaquetar más barato (BC1 para las
texturas opacas, o un encoder BC7 de un solo modo más rápido) o dejar de esperar
en ese submit. Cualquiera de las dos es una fase nueva, con su propia medición.

## Orden y calendario

Cada fase es un pull request propio, con su tabla de antes y después sacada con
`scripts/summarize_profile_trace.py` de la misma máquina.

| # | fase | gana | riesgo | depende de |
| --- | --- | --- | --- | --- |
| 0 | icono | nada medible | nulo | — |
| 1 | decodificar fuera del hilo | −324 ms en submit, −533 ms de pico | medio | — |
| 2 | packer BC7/BC1 | 1.492 → ~400 MB de VRAM | alto en calidad | fase 1 |
| 3 | pipelines | −100 ms de pico | bajo (parte 1) | — |
| 4 | suballocator | −11 s de CPU por hora | alto | — |

La 1 antes que la 2 porque el packer necesita un hilo donde correr. La 3 y la 4
son independientes y se pueden intercalar cuando apriete el aburrimiento.

## Lo que este plan no hace

No hay otra paralelización grande que hacer. El hilo principal está ocupado el
46,7% del reloj y ese tiempo es entero `nativeCallMessagesFromMainThread`, el
pump de llamadas nativas del propio Roblox, que no es nuestro. El resto de
nuestras categorías son llamadas al driver: `submit` 67 s, `present` 76 s,
memoria 14 s en una hora de juego. Repartir eso entre núcleos no es posible sin
cambiar el orden en que el guest ve sus propias llamadas.

## Copias entre imágenes comprimidas sin GPU

Había un camino por el que una copia `vkCmdCopyImage` entre dos imágenes
comprimidas se descartaba en silencio con `skipped a copy into a compressed
image`. `TranscodeCopy` re-encodaba los bloques guardados en la GPU, y en el
modo de decodificador Rust no hay GPU que transcodifique: `DecodesOnCpu`
devuelve verdadero, `PlanGpuUpload` declina, y la copia se perdía. El mip de
destino se quedaba con lo que tuviera, así que la textura se veía mal a
distancia y bien al acercarse, cuando entraba su mip 0.

`PackPiecesOnCpu` cubre ese hueco: copia la ventana de bloques del mip, la
decodifica con el formato de origen y la empaqueta a BC7 en el staging que ya se
había dimensionado en bloques. Dos detalles importan y costaron un SIGSEGV:

- El formato que se decodifica es el de la imagen de origen, no el de destino.
- Un staging de bloques se copia con `bufferRowLength` en bloques, como hace una
  subida a una imagen empaquetada, no con el paso de texels de la región.

## El packer y las texturas escaladas

Con el packer encendido quedan 52 texturas escaladas 4x frente a 170 sin él, y
en una imagen escalada el empaquetado no cubre los mips extra, así que se ven
mal a distancia. Las dos formas de resolverlo medidas en la misma escena:

| | BC7 packer | RGBA8 |
| --- | ---: | ---: |
| texturas en GPU | 235 MiB | 486 MiB |
| escaladas 4x | 52 | 170 |
| frame interval p50 | 20,3 ms | 20,4 ms |
| frame interval p99 | 32,2 ms | 30,6 ms |
| frame interval max | 192,4 ms | 74,5 ms |

Los frames son equivalentes, así que la elección es de VRAM contra escalado:
RGBA8 usa el doble de memoria y escala el triple de texturas. Se queda RGBA8
(`MOCKTAIL_ETC2_BC_PACK=0`, el valor por defecto) hasta que el packer cubra los
mips de una imagen escalada.
