# Decisiones y supuestos

Decisiones tomadas donde la especificación era ambigua, contradictoria o no
decía nada. Regla aplicada: elegir lo más simple y anotar el motivo.

## Estructura y dependencias

**D-01 · El workspace vive en la raíz de esta carpeta.** La especificación
muestra `phone-cluster/` como raíz; aquí la raíz es la carpeta del proyecto.
Mismo contenido, un nivel menos.

**D-02 · `pc-agent`, `pc-coordinator` y `mock-llama` son lib + bin.** El
`main.rs` solo parsea flags y llama a la librería. Así los tests de
integración levantan coordinador, mocks y agentes en el mismo proceso, en
puertos efímeros, sin lanzar binarios.

**D-03 · `reqwest` sin TLS.** La especificación pide `rustls-tls`, pero en
`reqwest` 0.13 esa feature ya no existe, y la nueva `rustls` arrastra
`aws-lc-rs` (C + cmake), lo que viola la regla 6 (nada de código nativo en
`pc-agent`). Todo el tráfico es HTTP en la LAN, así que se usa
`default-features = false` sin ninguna feature de TLS. Comprobado con
`cargo tree -p pc-agent`: no aparecen `ring`, `aws-lc`, `openssl` ni `cc`.
Si algún día hiciera falta HTTPS: `rustls-no-provider` + un proveedor de
criptografía en Rust puro.

**D-04 · Versiones fijadas** (y en `Cargo.lock`): tokio 1.54, axum 0.8.9,
tower-http 0.7.1, reqwest 0.13.5, clap 4.6, serde 1.0.229, uuid 1.29,
time 0.3.55, thiserror 2.0, anyhow 1.0, tracing 0.1.44,
tracing-subscriber 0.3.23, toml 1.1, futures 0.3. Rust 1.85+ (edición 2024).

**D-04b · Sin `unwrap()`/`expect()` fuera de tests, comprobado por clippy.**
`clippy::unwrap_used` y `clippy::expect_used` están en `deny` para todo el
workspace (`clippy.toml` los permite en tests).

## llama-server

**D-05 · llama.cpp fijado en `b11963`** (commit `39febd2`). Campos verificados
leyendo `tools/server/` de ese tag, no de memoria:

| Endpoint | Lo que se usa |
| --- | --- |
| `GET /health` | 200 `{"status":"ok"}`; mientras carga, 503 `{"error":{"message":"Loading model","type":"unavailable_error","code":503}}` → `loading` |
| `GET /props` | `model_alias`, `model_path`, `total_slots`, `default_generation_settings.n_ctx` |
| `GET /slots` | array de `{id, n_ctx, is_processing, …}`; habilitado por defecto (`--no-slots` lo desactiva y responde 501 `not_supported_error`) |
| respuestas | `usage.completion_tokens`; `timings.predicted_n`, `timings.predicted_per_second` (en streaming, en el último chunk) |

Los fixtures de `crates/pc-agent/tests/fixtures/llama/` están construidos a
partir de ese código, **no capturados de un servidor real**: la checklist de
hardware incluye capturarlos y sustituirlos.

**D-06 · Nombre del modelo de un nodo = `model_alias` de `/props`**, que es
el mismo `id` que llama-server anuncia en `/v1/models` (primer `--alias`, o
nombre del modelo, o nombre del fichero). Si falta, nombre del fichero de
`model_path` sin `.gguf`. El coordinador compara `request.model` con él de
forma exacta. Recomendación práctica: arrancar cada `llama-server` con
`--alias <nombre>` para que los nombres sean cortos y coincidan entre teléfonos.

**D-06b · El cuerpo se reenvía tal cual, incluido `"model":"auto"`.** En modo
de un solo modelo llama-server sirve el modelo cargado sin importar el campo.

## Agente

**D-07 · En `--mock`, `node_id` = UUIDv5 de `mock/<name>`** y no se guarda en
disco. Así varios agentes en el mismo Mac tienen ids distintos y estables (y
reiniciar un mock con otra temperatura conserva el nodo).

**D-08 · "Cargando" = enchufado.** De `termux-battery-status` se usa
`plugged != "UNPLUGGED"`, porque con límite de carga al 80 % Android informa
`NOT_CHARGING` estando conectado, y para la regla LowBattery lo que importa es
si está enchufado. Sin `plugged`, se usa `status` ∈ {`CHARGING`, `FULL`}. En
el fallback de `/sys` se usa `status` ∈ {`Charging`, `Full`}.

**D-09 · Lecturas térmicas**: valores con |x| ≥ 1000 se interpretan como
miligrados y el resto como grados; fuera de −30…150 °C la zona se ignora.
`thermal_zones` acepta el nombre del directorio (`thermal_zone3`) o el
contenido de `type` (`cpu-0-0`).

**D-10 · La primera recolección se hace antes del primer heartbeat**, para que
el nodo no aparezca como Down durante 5 s al arrancar. Después cada métrica va
en su propia tarea: batería 30 s (timeout de 3 s para `termux-battery-status`),
SoC/RAM/health/slots 5 s, props 60 s (5 s hasta conseguirlas la primera vez).

**D-11 · Backoff**: tras el primer fallo se espera `heartbeat_secs`, y luego el
doble cada vez, hasta 30 s. Con éxito vuelve a `heartbeat_secs`.

**D-12 · Flags `--mock-temp`, `--mock-battery`, `--mock-charging`.** Son parte
del modo `--mock` y hacen falta para el criterio de la Fase 5 ("simulando
46 °C en un mock"). Para cambiar la temperatura se reinicia el agente mock
(mantiene el `node_id`, D-07).

**D-13 · Si `advertise_url` no está definido**, se usa la IP local de la
interfaz con ruta hacia el coordinador (un `connect` UDP, que no envía nada)
más el puerto de `llama_url`, y se avisa en el log.

## Coordinador

**D-14 · Temperatura para Hot = máximo entre SoC y batería.** La especificación
dice solo "temperatura"; usar la más alta es lo conservador para el riesgo de
baterías calientes.

**D-15 · Sin lectura de temperatura, Hot conserva su valor anterior.** Un
sensor que falla intermitentemente no saca a un teléfono caliente de Hot.

**D-16 · Slots desconocidos = 1 total y 0 ocupados**, para que un nodo sin
`/slots` ni `/props` siga siendo utilizable de uno en uno.

**D-17 · Estado extra `Incompatible`**, evaluado justo después de Offline,
para mostrar en el dashboard un nodo con `proto_version` desconocido (el
heartbeat recibe 422). Si el cuerpo no trae `node_id`, se deriva uno del nombre.

**D-18 · Dos listeners con distinta confianza.** `listen` (7800, LAN):
heartbeats y administración exigen token; inferencia según
`require_token_for_inference` (por defecto `false`). `dashboard_listen`
(7801, loopback por defecto): sirve la página y permite administración e
inferencia sin token, porque una página estática no tiene forma segura de
guardar el token. Si `dashboard_listen` no es loopback, se avisa en el log.

**D-19 · `GET /api/v1/nodes` devuelve un objeto**, no un array:
`{nodes, recent_requests_ms, now_ms, openai_base_url, thresholds}`. Lo
necesita la barra superior (peticiones por minuto, URL a copiar) y la barra
térmica (umbrales de la configuración).

**D-20 · `node_updated` se emite en cada heartbeat**, al cambiar `in_flight` y
al cambiar de estado en la tarea de 2 s. Si un cliente SSE se queda atrás
(canal lleno), se le cierra el stream y el dashboard reconecta y pide la lista
completa.

**D-21 · El desempate determinista concentra el tráfico secuencial.** Con nodos
idénticos y ociosos, el orden de la especificación (slots libres, in_flight,
temperatura, node_id) elige siempre el mismo nodo; el reparto aparece con
peticiones concurrentes (via `in_flight`) o con temperaturas distintas, como
en teléfonos reales. Por eso el test de "30 peticiones entre 3 nodos" las
lanza en paralelo.

**D-22 · Reintentos**: `max_retries = 2` significa hasta 3 nodos distintos por
petición; nunca se reintenta en el mismo nodo. Si se agotan y el último fallo
fue un 5xx, se devuelve ese 5xx tal cual (con `X-PC-Node`); si todos fallaron
por conexión, 502 `upstream_unreachable`.

**D-23 · `X-PC-Node` lleva el nombre del nodo** (o su id si el nombre no es
ASCII visible) y `X-PC-Node-Id` su UUID.

**D-24 · El token del coordinador no puede estar vacío** (error al arrancar).

**D-25 · Cuerpos de hasta 32 MB** en las rutas del coordinador. Un cuerpo que
no es JSON se reenvía igual (como `model: auto`) y responde el llama-server.

**D-26 · Peticiones por minuto** = peticiones terminadas en los últimos 60 s
(reloj del coordinador).

## Fase 5 · Supervisión y comandos

**D-27 · `llama_command` en el agente.** Si está definido (en `config.toml`
como array, o tras `--` en la línea de comandos), el agente lanza
`llama-server` con él, lo relanza si muere (1 s, doblando hasta 10 s en bucle
de fallos) y vuelca su salida en `<state_dir>/llama-server.log`. Si no está
definido, el agente no supervisa nada y `llama-server` se gestiona aparte.
Con SIGTERM o Ctrl+C el agente mata a su `llama-server` antes de salir, para
no dejar un huérfano ocupando el puerto.

**D-28 · Cómo se emite `restart_llama`.** La especificación define el comando
pero no cómo se pide. Se añade `POST /api/v1/nodes/{id}/restart_llama` (202
con el comando, 404 si el nodo no existe) y el botón "Reiniciar llama" en el
dashboard.

**D-29 · Entrega "al menos una vez".** El coordinador reenvía cada comando en
la respuesta a cada heartbeat hasta verlo en `acked_commands`. El agente
recuerda los últimos 64 ids ejecutados, así que un comando reenviado se
confirma otra vez sin volver a ejecutarse. Los comandos pendientes viven en
memoria: si el coordinador se reinicia, se pierden.

**D-30 · Un comando que el agente no puede ejecutar también se confirma** (por
ejemplo `restart_llama` sin `llama_command`, o un tipo desconocido), con un
aviso en su log, para que no se reenvíe para siempre.

**D-31 · La vista de nodo incluye `pending_commands`** (tipos en snake_case,
como en el protocolo), para que el dashboard muestre "Reinicio pendiente…".

## Instalación remota por ADB (cambio de alcance pedido por el usuario)

La especificación dejaba fuera el descubrimiento mDNS y el despliegue
automático. El usuario pidió que instalar un teléfono fuera mucho más simple y
remoto, así que se añaden.

**D-32 · Sin Termux.** Los teléfonos se manejan con ADB inalámbrico (Android 11+).
En el teléfono solo hay que activar "Depuración inalámbrica" y vincularlo una
vez con el código de 6 dígitos, que se escribe en el dashboard. El resto lo hace
el coordinador: descubre los teléfonos con `adb mdns services`, los conecta, y
copia y arranca todo en `/data/local/tmp/phone-cluster` como usuario `shell`.

**D-33 · Compilación cruzada en el Mac** con `tools/build-android.sh` (Android
NDK + CMake del SDK): `pc-agent` para `aarch64-linux-android` y `llama-server`
b11963 estático (`c++_static`, sin OpenMP, sin curl ni OpenSSL). La arquitectura
por defecto es `armv8.2-a+dotprod`, que cubre teléfonos de ~2018 en adelante;
para teléfonos más viejos, `GGML_CPU_ARM_ARCH=armv8-a tools/build-android.sh`.
Modelo por defecto: Qwen2.5-1.5B-Instruct Q4_K_M (~1 GB), cambiable con `MODEL_URL`.

**D-34 · Instalación idempotente.** El coordinador calcula una huella (FNV-1a)
de los ficheros locales (nombre, tamaño, fecha) y del `config.toml` generado, y
la guarda en el teléfono (`.deployed`). Misma huella y agente vivo: no hace
nada. Misma huella y agente muerto: solo lo arranca. Huella distinta: copia los
binarios, el modelo solo si cambió su tamaño, y reinicia. Cada teléfono
conectado se comprueba cada 10 s, así que un agente que muere se relanza solo.

**D-35 · Sin Termux no hay `termux-battery-status`, y `/sys/.../battery` da
"Permission denied" al usuario `shell`** (comprobado en el emulador). El agente
cae a `dumpsys battery`, y para la temperatura a `dumpsys thermalservice` cuando
`/sys/class/thermal` no se puede leer.

**D-36 · El agente arranca con `setsid nohup` desde `start.sh`**, desacoplado de
la sesión de adb. Si el teléfono se reinicia, los procesos mueren; al volver a
activar la depuración inalámbrica, el coordinador los arranca otra vez (no hay
que reinstalar).

**D-37 · `config.toml` en el teléfono lleva el token**: la carpeta queda en
700 y el fichero en 600, solo para el usuario `shell`.

**D-38 · Nombre del nodo** = modelo del teléfono en minúsculas + último octeto
de su IP (`pixel-7-21`). Clave del dispositivo = su IP (o el serial de ADB por
USB o en el emulador).

**D-39 · Emulador Android.** Si el serial es `emulator-NNNN`, el agente llega al
coordinador por `10.0.2.2` y el Mac llega a su llama-server con
`adb forward tcp:18NNN tcp:8080`. Sirve para probar todo sin teléfono.

**D-40 · Los tests de integración desactivan ADB** (`[adb] enabled = false`)
para no tocar los dispositivos conectados al Mac.

**D-41 · El coordinador reinicia el servidor de `adb` al arrancar.** Con el
primer Samsung real, `adb mdns services` no veía el teléfono aunque el Bonjour
de macOS (`dns-sd -B _adb-tls-connect._tcp`) sí lo veía. El servidor de adb
llevaba horas arrancado (desde las pruebas con el emulador) y tras
`adb kill-server` lo detectó en segundos. Contrapartida: corta un momento las
sesiones de adb abiertas (Android Studio).

## App Android y app de Mac (cambio de alcance pedido por el usuario)

Con un Samsung real, la vinculación por ADB resultó demasiado complicada para
el usuario: no encontraba el código de vinculación. El usuario pidió "instala
una app y listo" y que la parte del Mac fuera una app de escritorio. ADB queda
como alternativa avanzada, plegada en el dashboard.

**D-42 · App Android sin Play Store.** `android-app/` está escrita en Java sin
dependencias (sin AndroidX), con minSdk 29 y targetSdk 36. El dashboard muestra
un QR que abre `/join` en el teléfono, una página con dos botones: «Descargar
la app» (`/app`, el APK) y «Abrir la app y conectar». El segundo es un enlace
`intent://connect?host=…&port=…` que abre la app con la dirección del Mac, así
que no depende de mDNS. Si ese enlace no llega, la app busca el Mac por Bonjour
(`_phonecluster._tcp`, que el coordinador anuncia con `dns-sd -R`) y, como
último recurso, admite la IP escrita a mano.

**D-43 · Los binarios viajan dentro del APK como `lib*.so`.** `pc-agent` y
`llama-server` van en `jniLibs/arm64-v8a/` como `libpcagent.so` y
`libllamaserver.so`, con `useLegacyPackaging = true`. Android los extrae en
`nativeLibraryDir`, el único directorio de la app desde el que se pueden
ejecutar (W^X). Comprobado en el emulador: corren como el usuario de la app y
`llama-server` responde a 23 tok/s.

**D-44 · Servicio en primer plano de tipo `specialUse`.** No tiene el límite de
6 h al día que sí tienen `dataSync` y `mediaProcessing`. Mantiene un
`PARTIAL_WAKE_LOCK` y un WifiLock, pide la exención de ahorro de batería y
reinicia `pc-agent` si muere. Antes de cada arranque mata los `lib*.so`
huérfanos de una ejecución anterior.

**D-45 · Alta con un clic en el Mac.** La app llama a `POST /api/v1/join` (sin
token) y queda "pendiente" hasta que el usuario pulsa «Aceptar» en el
dashboard. Entonces recibe el token, su nombre de nodo, el modelo y los
argumentos de llama-server. Las solicitudes viven en memoria. Si la app
presenta el token vigente, se acepta sin preguntar, así que reiniciar el Mac no
obliga a volver a aceptar los teléfonos.

**D-46 · El modelo se descarga del Mac, no de internet.**
`GET /api/v1/files/model` (con token) usa `ServeFile` y admite `Range`, así que
una descarga cortada se reanuda. En la LAN, 1 GB tarda alrededor de un minuto.

**D-47 · La batería la escribe la app.** Dentro de una app, `dumpsys` y
`/sys/.../battery` no están permitidos. La app escribe `battery.json`, con el
formato de `termux-battery-status`, a partir de `ACTION_BATTERY_CHANGED`, y
`pc-agent` lo lee mediante la nueva opción `battery_file`. La temperatura del
SoC suele no estar disponible para apps; la regla Hot usa la temperatura de la
batería (D-14).

**D-48 · El APK se firma con la clave de depuración de este Mac.** Basta para
instalarlo a mano. Una actualización solo se instala encima si la firma es la
misma, así que hay que compilar siempre en el mismo Mac (o crear un keystore
propio si algún día hace falta).

**D-49 · App de Mac con Tauri 2** (`apps/desktop`). Arranca el coordinador
dentro del propio proceso y muestra el dashboard en su ventana. La primera vez
crea `coordinator.toml` con un token aleatorio en
`~/Library/Application Support/dev.phonecluster.desktop/`, copia el APK y los
binarios que lleva dentro y descarga el modelo con barra de progreso. No hace
falta terminal. `Info.plist` declara el permiso de red local y los servicios
Bonjour.

**D-50 · Sin `alert()`/`confirm()` en el dashboard.** WebKit, dentro de la app
de escritorio, puede no mostrarlos. Se usan avisos dentro de la página, y
«Olvidar» se confirma con un segundo clic.

## Primer teléfono real: Galaxy S24 (SM-S921U)

**D-51 · `llama-server` con 4 hilos como máximo y `--poll 0` en la app Android.**
Con la app en pantalla, el S24 daba 31 tok/s. En segundo plano una respuesta
corta tardaba minutos: Android deja la app en menos núcleos, y llama.cpp, con
un hilo por núcleo y espera activa, casi se detiene. La app añade
`-t min(4, núcleos)` y `--poll 0` (los hilos duermen al esperar), salvo que el
Mac ya mande otros valores en `llama_args`.

**D-52 · Batería y procesador tienen umbrales distintos.** La especificación
pone 45 °C sin decir de qué sensor. El SoC del S24 marca 45–53 °C bajo carga
normal, así que el nodo quedaba casi siempre Hot. Ahora la batería usa
`max_temp_c`/`resume_temp_c` (45/40 °C), que es lo que importa para la
seguridad, y el SoC usa `max_soc_temp_c`/`resume_soc_temp_c` (80/70 °C). Sale de
Hot solo cuando los dos se han enfriado. Esto sustituye a D-14. En `--mock`,
`--mock-temp` es la batería y el SoC va 10 °C por encima.

**D-53 · macOS exige el permiso «Red local» a la app de Mac.** Sin él,
macOS bloquea las conexiones de la app a los teléfonos con `EHOSTUNREACH`,
aunque los teléfonos sí lleguen al Mac. El proxy lo detecta y el error explica
dónde activarlo. El bundle se firma ad hoc con su identificador
(`signingIdentity: "-"`) para que macOS lo reconozca y muestre el aviso.

**D-54 · El dashboard solo atiende peticiones del propio Mac.** No pide token
porque escucha en loopback. Aun así, una web abierta en el Mac podría aceptar un
teléfono suyo por CSRF o DNS rebinding y llevarse el token. Ahora se exige
`Host` local y, en peticiones que cambian algo, `Origin` local o ausente.

**D-55 · Endurecimiento tras la revisión.**
- Alta: como mucho 64 solicitudes a la vez, y nombres de 64 caracteres sin
  caracteres de control. Un teléfono rechazado se puede aceptar después.
- La URL del QR se recalcula en cada petición, porque la IP del Mac puede
  cambiar.
- La app Android:
  - vuelve a buscar el Mac si deja de responder durante 60 s, y reinicia el
    agente si cambia su propia IP;
  - solo envía el token al Mac que se lo dio;
  - no procesa dos veces el enlace del QR al girar la pantalla;
  - resuelve el error 416 al reanudar un modelo ya completo.
- La descarga del modelo en la app de Mac reintenta ante cortes, comprueba el
  tamaño final y muere con la app.

**D-56 · Modo dividido con el RPC de llama.cpp.** Hasta ahora cada teléfono
tenía el modelo completo. En modo dividido cada teléfono corre `rpc-server`
(de llama.cpp b11963, `-DGGML_RPC=ON`) y presta su CPU. El Mac corre la
«cabeza»: un `llama-server` con `--rpc ip1:50052,ip2:50052 -ngl 999 -ts …`.
La cabeza reparte las capas, encadena el cálculo y se registra como un nodo
más («dividido»), así que el proxy no cambia.
- **El Mac es la cabeza.** Ya tiene el `.gguf` (no hay que subirlo a un
  teléfono) y el coordinador lee su log. Se compila sin Metal ni BLAS: sin
  dispositivos locales, `-ngl 999` manda todas las capas a los teléfonos.
  Solo la capa de entrada (embedding) se queda en la CPU del Mac, como
  siempre hace llama.cpp.
- **El modo es un estado, no un comando.** `HeartbeatResponse.mode`
  (`solo`/`worker`) se repite en cada respuesta y aplicarlo dos veces no
  cambia nada. El agente nunca corre los dos procesos a la vez: parar
  llama-server libera la RAM para las capas.
- **Reparto.** `--tensor-split` es proporcional a la RAM libre de cada
  teléfono. El panel calcula qué capas tiene cada uno con la misma fórmula
  de `llama-model.cpp`. Lo comprobamos en el Mac con dos `rpc-server`: con 28
  capas a partes iguales salen 15 + 13, más la capa de salida. Los MiB reales
  salen del log de la cabeza (`-lv 4`).
- **Seguridad.** `rpc-server` no tiene autenticación y su README pide no
  exponerlo en redes abiertas. Mitigaciones:
  - solo corre mientras el modo dividido está activo;
  - escucha en la IP de la Wi-Fi con la que el teléfono llega al Mac, nunca
    en `0.0.0.0`;
  - el panel avisa de que es solo para redes de confianza.
- **Rendimiento.** Cada token cruza la Wi-Fi una vez por teléfono. Con un
  modelo que cabe en un teléfono será más lento que el modo normal. Merece la
  pena con modelos que no caben en uno solo.
- **Procesos vigilados.** La cabeza va envuelta en el mismo `sh` vigilante
  que `dns-sd`. Ese `sh` ya no lleva `kill_on_drop`: un SIGKILL no ejecuta su
  `trap`, y por eso al cerrar la app quedaban `dns-sd` huérfanos. Se para con
  SIGTERM; si el coordinador muere, el `sh` lo detecta por `$PPID`.

**D-57 · Dos variantes de llama.cpp en el APK.** El build optimizado
(`armv8.2-a+dotprod`, D-51) muere por instrucción ilegal en procesadores
ARMv8.0 como el Snapdragon 680 del Redmi Note 11: carga el modelo, cae en el
primer cálculo y se relanza en bucle. Por eso el APK lleva también
`libllamaserver_v8.so` y `librpcserver_v8.so`, compilados para `armv8-a`. La
app usa los optimizados solo si todas las líneas `Features` de
`/proc/cpuinfo` tienen `asimddp`; si no puede leerlo, usa los genéricos, que
funcionan en todos. Las dos variantes salen del mismo tag, así que hablan el
mismo protocolo RPC.

**D-58 · Un teléfono con la app no se instala también por ADB.** Si el S24
estaba conectado por cable o con depuración inalámbrica, el Mac le instalaba
y arrancaba un agente por ADB. A la vez, la app tenía el suyo. Resultado: el
teléfono salía dos veces en el panel (`galaxy-s24-234` y
`sm-s921u-<serie>`) y cargaba el modelo dos veces, lo que le quitaba ~1.9 GB
de RAM al modo dividido. Ahora el escaneo de ADB mira si está instalado
`dev.phonecluster.app`. Si lo está, para el agente de ADB y marca el
dispositivo como «Usa la app». Reinstalar a mano desde el panel sigue
forzando la instalación por ADB.
