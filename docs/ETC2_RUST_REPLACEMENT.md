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

- [x] Crate creado, ABI establecida, decodificador de bloques ETC1 en modo
      individual (el más común) con pruebas.
- [ ] Modo diferencial, T, H, planar y EAC (R11/RG11) con pruebas diferenciales
      contra el C++.
- [ ] Packer BC7/BC4/BC5.
- [ ] Integración en el runtime.
