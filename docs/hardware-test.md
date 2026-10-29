# Prueba en hardware (2 teléfonos)

Checklist manual con dos teléfonos reales, a pasar al final de las fases 1, 3
y 5. Anota fecha, modelo de teléfono y resultado de cada casilla.

## Preparación

**En el Mac, una vez:** `tools/build-android.sh` (compila la app Android y baja
el modelo) y después `cd apps/desktop && cargo tauri build --bundles app`.
Abre `target/release/bundle/macos/Phone Cluster.app`. Si macOS pide acceso a
la red local o a conexiones entrantes, acepta.

**En cada teléfono** (misma Wi-Fi que el Mac):

1. Escanea con la cámara el QR de "Añadir teléfonos" en la app del Mac.
2. Pulsa **Descargar la app**, instálala (permite instalar desde el navegador)
   y vuelve a la página.
3. Pulsa **Abrir la app y conectar** y luego **Aceptar** en el Mac.

El teléfono descarga el modelo del Mac (~1 minuto) y aparece como nodo.

<details>
<summary>Alternativas avanzadas: ADB inalámbrico o Termux</summary>

- **ADB**: en el dashboard, despliega "Alternativa avanzada", activa la
  Depuración inalámbrica en el teléfono y vincúlalo con el código de 6 dígitos.
- **Termux**: compila llama.cpp y `pc-agent` en el teléfono y ejecuta
  `pc-agent -- llama-server -m <modelo>.gguf --host 0.0.0.0 --port 8080`, con
  `coordinator_url` y `token` en `~/.config/pc-agent/config.toml`.
</details>

## Fase 1 · Agente

| ✓ | Paso | Esperado |
| --- | --- | --- |
| ☐ | QR → Descargar → Conectar → Aceptar en el Mac | La app pasa por "Esperando que lo aceptes" → "Descargando el modelo" → "Funcionando" y el nodo aparece Ready |
| ☐ | Samsung: ¿hizo falta desactivar el Bloqueador automático o confirmar Play Protect? | Anotar los avisos exactos que salieron |
| ☐ | `adb -s <ip>:<puerto> shell /data/local/tmp/phone-cluster/pc-agent --once --config /data/local/tmp/phone-cluster/config.toml` | JSON válido con batería, SoC y RAM reales (vía `dumpsys` si `/sys` no se puede leer) |
| ☐ | `pc-agent --once` con llama-server arrancado | `llama.status = "ok"`, `model`, `ctx_size`, `slots_total` correctos |
| ☐ | Comparar `thermal.max_c` con `cat /sys/class/thermal/thermal_zone*/temp` | Coincide con el máximo legible; anotar qué zonas son legibles en este SoC |
| ☐ | `pc-agent` sin coordinador arrancado, 2 min | Sigue vivo; reintentos cada 5→10→20→30 s en el log |
| ☐ | `pc-agent --thermal-zones thermal_zone999` | `thermal: null`, el ciclo sigue |
| ☐ | Reiniciar el teléfono y reactivar la depuración inalámbrica | Se reconecta y arranca solo, sin volver a copiar el modelo; mismo `node_id` |
| ☐ | Capturar respuestas reales para los fixtures (D-05) | `curl -s 127.0.0.1:8080/props`, `/slots` y `/health` (durante la carga) guardados en `crates/pc-agent/tests/fixtures/llama/`; `cargo test -p pc-agent` sigue en verde |

## Fase 3 · Streaming y reintentos

| ✓ | Paso | Esperado |
| --- | --- | --- |
| ☐ | Dos teléfonos con agente; abrir `http://127.0.0.1:7801` en el Mac | Ambos Ready en < 10 s |
| ☐ | `curl -N http://<IP del Mac>:7800/v1/chat/completions -H 'content-type: application/json' -d '{"model":"auto","stream":true,"messages":[{"role":"user","content":"hola"}]}'` | Tokens llegan uno a uno; cabecera `x-pc-node` |
| ☐ | Mismo con el SDK de Python (`base_url="http://<IP del Mac>:7800/v1"`) | Streaming correcto |
| ☐ | 10 peticiones concurrentes | Se reparten entre los dos (columna "Atendidas") |
| ☐ | Matar `llama-server` en un teléfono y lanzar peticiones | Ese nodo pasa a Down en ≤ 7 s; las peticiones van al otro |
| ☐ | Apagar el Wi-Fi de un teléfono | Offline a los ~15 s; ninguna petición falla por ello tras el cambio |
| ☐ | Parar ambos | 503 `no_nodes_available` |
| ☐ | Anotar tok/s por teléfono y modelo (CPU; Vulkan si se prueba) | — |

## Fase 5 · Protección y supervisión

| ✓ | Paso | Esperado |
| --- | --- | --- |
| ☐ | Carga sostenida hasta que el SoC pase de 45 °C | Hot, sale de rotación; vuelve a Ready al bajar de 40 °C |
| ☐ | Desenchufar con batería < 20 % | LowBattery; al enchufar vuelve |
| ☐ | Matar `llama-server` (`adb shell 'kill $(pidof llama-server)'`) | El agente lo relanza; Ready de nuevo en < 15 s |
| ☐ | Matar `pc-agent` | El coordinador lo relanza en ≤ 10 s |
| ☐ | Comando `restart_llama` | El agente reinicia llama-server y confirma en `acked_commands` |
| ☐ | 30 min con la pantalla apagada | Sin Offline intermitentes; si los hay, anotar el fabricante (ahorro de energía propio) y probar con el teléfono enchufado |
