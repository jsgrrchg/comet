**Investigación: pasar Pi de ACP a RPC nativo en Zeron — 2026-09-29**

Recomendación: implementar `PiHarness` en Rust y conectar directamente con `pi --mode rpc`. La interfaz `Harness` y los eventos de Zeron permiten conservar gran parte del engine y de la UI. El trabajo principal está en el ciclo de vida, la normalización de eventos, las sesiones y las extensiones. Esta investigación no implementa la migración.

Hoy el recorrido es `Zeron → AcpHarness → pi-acp → pi --mode rpc`. La propuesta es `Zeron → PiHarness → pi --mode rpc`. Se elimina un proceso intermediario y la dependencia de instalación del adaptador; Pi y sus dependencias siguen siendo necesarios. No se midió una mejora de latencia.

**Evidencia revisada**

- Código de este checkout: `crates/harness/src/acp/`, `jsonrpc.rs`, `process.rs`, `skills.rs`, interfaz `Harness`, `crates/proto/src/agent.rs`, registro del engine, Settings y pruebas de Pi.
- Paquete exacto `pi-acp@0.0.33`, descargado sin instalar ni ejecutar sus dependencias. Su `gitHead` es `1bfcb394088ed879db8fd936b570bb626017f878`.
- Pi instalado: `0.85.1`. Se leyó su implementación RPC y se ejecutaron pruebas aisladas con un proveedor simulado local.
- npm anunciaba Pi `0.87.1` al investigar. Se inspeccionó su código publicado de referencia, commit `f07218c4d4bbc12bef056a7058c3dd49dfe41abe`; no se instaló ni ejecutó esa versión. [Metadatos del paquete](https://registry.npmjs.org/@earendil-works/pi-coding-agent/latest).
- Documentación oficial actual. Hay diferencias entre esa documentación y las versiones publicadas revisadas; ver compatibilidad más abajo.

**Limitaciones concretas de la integración actual**

| Área | Evidencia y consecuencia | Cambio propuesto |
| --- | --- | --- |
| Steering | `pi_spec()` declara `StepBoundary`, pero el registro estático del engine anuncia `TurnBoundary`. Sin extensión ACP de steering, el harness cancela la generación y vuelve a enviar un prompt, esperando las herramientas abiertas. | Usar `steer` nativo cuando hay trabajo activo y `prompt` cuando está idle. Sin cancelar el modelo para insertar una instrucción. Alinear descriptor y driver. |
| Preguntas | El adaptador cancela `input` y `editor`. Convierte todas las opciones de `select` en `allow_once`; el clasificador ACP de Zeron las trata como permisos y autoacepta. | Traducir el subprotocolo `extension_ui_request/response` al puente de preguntas, preservando la selección real del usuario. |
| Errores | El adaptador cierra `agent_settled` como `end_turn` salvo cancelación, sin derivar el resultado del error final del assistant. Zeron recupera fallos mediante un `notify` añadido en su extensión MCP. | Leer directamente `message_end.stopReason/errorMessage`, conservar el resultado final tras reintentos y emitir `Done::Errored` cuando corresponda. |
| Comandos de extensiones | La consulta `get_commands` del adaptador usa `includeExtensionCommands: false`. | Exponer los comandos nativos por workspace, además de skills y templates. |
| Thinking | Zeron anuncia hasta `Max`; el adaptador fija su lista en `off…xhigh`. | Consultar `get_available_thinking_levels` por modelo, sin anunciar niveles inexistentes. Decidir cómo representar `off`, que no existe en `ReasoningLevel`. |
| Usage y contexto | El adaptador ofrece estadísticas por `/session`, pero su traductor de eventos no emite el `usage_update` que espera Zeron. | Consumir usage y modelo directamente. Distinguir ocupación de contexto, consumo acumulado y coste. |
| MCP | `mcpServers` se guarda pero no se conecta. Zeron ya añade una extensión y un wrapper ejecutable por run. | Conservar la extensión de herramientas e inyectarla directamente con `--extension`; eliminar el wrapper que exige `pi-acp`. |

Las observaciones sobre el adaptador corresponden al paquete fijado, no a todos los servidores ACP. Su [implementación de sesiones](https://github.com/svkozak/pi-acp/blob/1bfcb394088ed879db8fd936b570bb626017f878/src/acp/session.ts) sí transmite `thinking_delta` y algunos diálogos; su README está desactualizado en ese punto. El catálogo de modelos también se descubre actualmente: `default` es el fallback de Zeron, no su único modelo posible.

**Diseño del driver**

Crear `crates/harness/src/pi/{mod.rs,rpc.rs,normalize.rs}` y mover la extensión MCP a ese módulo. Implementar `Harness` conservando `HarnessId::Pi`, de modo que los chats y preferencias existentes sigan identificando al mismo proveedor.

El transporte debe tener un lector continuo de stdout, correlación por `id`, respuestas pendientes y un flujo de eventos ordenado. Pi utiliza JSONL con campos `type`, `command`, `success` y `data`, no los sobres JSON-RPC 2.0 de `jsonrpc.rs`. Se puede reutilizar el patrón de concurrencia, pero no ese cliente sin modificarlo. Separar por LF; admitir CRLF y divisiones de UTF-8 entre lecturas. Mantener stderr para diagnóstico. [Protocolo oficial](https://pi.dev/docs/latest/rpc).

Reutilizar resolución de ejecutables/PATH, `process::Command`, colas del harness, `StderrTail`, lease de ejecución y limpieza de procesos. Mantener el proceso vivo entre turnos mientras exista su mailbox, y retirarlo al cerrar la sesión, caer el consumidor o interrumpirse definitivamente. Conservar grupos de procesos en Unix y Job Objects en Windows; matar sólo el proceso padre puede dejar herramientas vivas.

| Entrada de Pi | Salida de Zeron propuesta |
| --- | --- |
| `get_state` | `SessionStarted`, identidad nativa, modelo y archivo de sesión |
| `message_update` con `text_delta` / `thinking_delta` | `TextDelta` / `ReasoningDelta` |
| `tool_execution_start/end` | `ToolCall` / `ToolResult`, correlacionados por `toolCallId` |
| `message_end` | Reconciliar mensaje final, usage y error/aborto |
| `agent_settled` | Cerrar la ejecución activa con un único `Done` |
| `extension_ui_request` de diálogo | Pregunta pendiente y posterior `extension_ui_response` |
| `get_commands` | `AvailableCommands` y descubrimiento de skills |

El stream actual lleva deltas, no todos los snapshots parciales del SDK. Hay que reconstruir por `contentIndex` y reconciliar con los mensajes finales, sin duplicar texto o herramientas. Para herramientas conocidas se mantienen los tipos de Zeron; herramientas de extensiones conservan nombre y argumentos. Los diffs de Pi requieren traducción: no son automáticamente el `ToolDiff` de ACP. Los resultados con imágenes necesitan una decisión de almacenamiento/renderizado adicional. [Eventos oficiales](https://pi.dev/docs/latest/json).

**Ciclo de vida y compatibilidad: la parte más delicada**

Una respuesta exitosa a `prompt` confirma aceptación; no finalización. `turn_end` cierra una respuesta y sus herramientas. `agent_end` puede preceder un reintento, compactación o continuación. El terminal correcto para trabajo del agente es `agent_settled`, combinándolo con el error/aborto final. No finalizar por silencio ni por un resultado de herramienta. Registrar el lector antes de enviar comandos y tolerar eventos/respuestas en distinto orden. [Ciclo de vida RPC](https://pi.dev/docs/latest/rpc#run-lifecycle).

Hay otra ruta terminal: comandos de extensiones o handlers de input que consumen el prompt sin iniciar un run. En la prueba `/probe-noop` produjo únicamente una respuesta exitosa; esperar `agent_settled` ahí bloquearía el chat.

La documentación actual incorpora `data.disposition = started | queued | handled`. Sin embargo, Pi 0.85.1 probado devuelve éxito sin ese campo, y el [código correspondiente a 0.87.1](https://github.com/earendil-works/pi/blob/f07218c4d4bbc12bef056a7058c3dd49dfe41abe/packages/coding-agent/src/modes/rpc/rpc-mode.ts) también usa una confirmación booleana de preflight sin disposición. No conviene implementar suponiendo que `latest` documentado equivale al npm instalado.

La implementación debe fijar una matriz de versiones y resolver explícitamente esa ruta: compatibilidad probada para las versiones sin disposición, o una versión publicada verificada que ya incluya el campo. Un timeout o una lectura aislada de `isStreaming: false` no demuestra finalización durante reintentos o compactación. Los comandos conocidos de extensiones pueden tratarse de forma específica, pero los handlers que consumen prompts normales requieren cobertura adicional.

Para steering, el ACK indica que se encoló, no que ya se incorporó al contexto. Rotar el segmento del transcript al observar su incorporación, mantener los `message_id` de Zeron y cubrir la carrera entre encolar y quedar idle. `follow_up` existe, pero `RunControls` sólo dispone hoy de una entrada de steering: ofrecer ambos comportamientos al usuario requiere ampliar el contrato/UI. Para interrumpir, vaciar la cola mediante `clear_queue`, enviar `abort` y conservar la escalada de terminación si no responde. [Comandos RPC](https://pi.dev/docs/latest/rpc-commands).

**Sesiones existentes**

El adaptador ya utiliza archivos JSONL nativos de Pi y normalmente publica su UUID nativo. Además guarda la relación UUID → archivo en `~/.pi/pi-acp/session-map.json`. Esto permite una migración sin convertir el historial, siempre que se resuelva el archivo correcto.

Mantener el UUID que Zeron tiene guardado; obtener y cachear la ruta en el host de ejecución. Para sesiones antiguas, consultar el mapa del adaptador y, si falta, buscar en las sesiones nativas respetando `PI_CODING_AGENT_DIR` y directorios configurados. Abrir con `--session <ruta-absoluta>`, verificar el UUID devuelto y evitar reproducir en el transcript mensajes que Zeron ya tiene.

En Pi 0.85.1, `--session <uuid>` funciona para una sesión local, pero la búsqueda que encuentra una sesión de otro proyecto puede abrir una confirmación interactiva de fork en stdout. La ruta absoluta evita esa rama. Un fallo de recuperación debe quedar explícito; no presentar una sesión nueva como si conservara el contexto anterior.

La prueba ejecutada reabrió un archivo nativo tras terminar Pi y mantuvo el UUID y sus 10 mensajes. La recuperación de un chat ACP real sigue siendo una prueba pendiente de la migración; el formato y el mapa se comprobaron por código.

**Extensiones y superficie de Zeron**

`select`, `confirm` e `input` encajan en gran parte en `RunControls.request_input`. `editor` necesita conservar texto prellenado y edición multilínea; el contrato actual `UserInputQuestion` no tiene campo de prefill. `notify`, `setStatus`, `setWidget`, `setTitle` y `set_editor_text` necesitan rutas específicas si se desea mostrarlos; no son preguntas. Los diálogos no deben bloquear el lector de eventos, y deben resolverse o cancelarse al cerrar el run. [UI RPC](https://pi.dev/docs/latest/rpc-extension-ui).

RPC no reproduce toda la TUI: `custom()` y varias APIs de componentes no están disponibles. La migración tampoco incorpora automáticamente sandboxing ni MCP nativo. Para Zeron, mantener la extensión MCP permite preservar sus herramientas de chats; se puede quitar de ella el parche de notificación de errores una vez que el driver lea los errores directamente. [Limitaciones UI](https://pi.dev/docs/latest/rpc-extension-ui), [modelo de ejecución de Pi](https://pi.dev/docs/latest/security).

Los comandos propios de la TUI no deben enviarse indiscriminadamente como prompts. El adaptador ofrece `/compact`, `/session`, `/name`, `/export`, `/autocompact`, `/steering` y `/follow-up`; conservar sus funciones requiere traducirlos a RPC. Descubrir comandos en el cwd real y reutilizar el enlace de skills de `skills.rs`. Fork, árbol de sesiones y estadísticas detalladas son extensiones posteriores del producto: el transporte los facilita, pero el contrato actual de `Harness` no los expone todos.

**Mapa de cambios y orden sugerido**

1. Transporte y normalización: crear `PiHarness`, lector JSONL, sesión persistente, streaming, herramientas, imágenes adjuntas, errores, terminales e interrupción. Añadir una fixture RPC determinista y pruebas del ciclo de vida.
2. Paridad e integración: sesiones antiguas, descubrimiento por cwd, modelos/thinking, comandos existentes, MCP, preguntas y steering. Registrar `PiHarness` en `crates/engine/src/registry.rs`. Incorporar `PI_EXECUTABLE`; `PI_ACP_EXECUTABLE` apunta a otro binario y no puede reinterpretarse como Pi.
3. Retirada del adaptador: quitar `pi_spec`, prewarm e instalación gestionada de Pi ACP, actualizar Settings, comentario de `HarnessId::Pi`, documentación de paridad/MCP y pruebas de instalación/resolución. Conservar la infraestructura ACP que usan los demás agentes.
4. Ampliaciones opcionales: estado/avisos de extensiones, editor con prefill, árbol/fork, controles de compactación y costes. Algunas requieren eventos nuevos y consumidores en desktop/móvil.

Pruebas prioritarias antes de activar el driver: fin único; errores antes y después de aceptación; recuperación tras retry; compactación; prompts consumidos sin run; múltiples steers y carrera con idle; cancelar durante generación/herramientas/diálogos; EOF y crash; reap de descendientes; reanudación de UUID heredado; MCP y su cancelación; imágenes; streams grandes y eventos desconocidos; executable paths con espacios y `.cmd` en Windows. Las pruebas actuales `pi_resume.rs`, `pi_live.rs`, `pi_mcp.rs` y `real_acp_lifecycle.rs` aportan escenarios que conviene trasladar.

**Validación realizada durante esta investigación**

Se ejecutó Pi 0.85.1 en un directorio temporal, con directorio de agente aislado, descubrimiento de recursos desactivado y una extensión explícita con un proveedor simulado. No se invocó ningún proveedor externo ni se modificó la configuración de Pi del usuario.

Pasaron: consultas de estado/modelos/thinking/comandos; prompt con streaming y `agent_settled`; fallo del proveedor en `message_end`; steering con dos turnos bajo un único cierre; `clear_queue` + `abort` con mensaje `aborted`; diálogo `input` contestado por RPC; comando sin LLM que sólo devuelve ACK; cierre y reanudación del archivo con UUID e historial preservados. Las primeras pruebas revelaron un fallo en el formateador del script de diagnóstico; corregido éste, la ejecución completa terminó correctamente.

No se ejecutaron pruebas Rust porque no se modificó código de producto. Tampoco se validaron modelos reales, Windows, retry/compactación en vivo ni migración end-to-end de un chat ACP. El alcance permite recomendar la arquitectura y localizar los riesgos; no equivale a certificar un driver todavía inexistente.
