//! The Kotlin `ws_rpc` client: a transport that owns the socket, a per-operation client, and a
//! dispatcher attachment for a service the app implements. A reply's `error` key carries the
//! operation's declared error verbatim, or `{ "isServiceFault": true, "fault": <fault fields> }`.

use super::result::result_name;
use crate::features::kotlin::kotlin_typename;
use crate::field_type::get_field_def;
use crate::rename_rule::RenameRule;
use crate::service_schema::parse::{
    OperationDef, OperationInputs, OperationOutcome, ServiceDef, is_unit_type,
};
use crate::service_schema::support::fault_fields_typescript_name;
use core::fmt::Write as _;
use syn::Type;

pub fn emit(service: &ServiceDef) -> Vec<String> {
    let named = service.ident.to_string();
    let fn_prefix = RenameRule::CamelCase.apply_to_variant(&named);
    let mut published = vec![
        socket_interface(&named),
        options_class(&named),
        frames_class(&named),
    ];
    if has_one_way(service) {
        published.push(refusal_class(&named));
    }
    published.push(transport_class(&named));
    published.push(client_class(service, &named, &fn_prefix));
    published.push(handlers_interface(service, &named));
    published.push(attachment_class(&named));
    published.push(attach_dispatcher_fn(&named, &fn_prefix));
    published.push(dispatch_fn(service, &named, &fn_prefix));
    published.extend(fault_helpers(&named, &fn_prefix));
    published
}

fn has_one_way(service: &ServiceDef) -> bool {
    service
        .operations
        .iter()
        .any(|operation| matches!(operation.outcome, OperationOutcome::OneWay))
}

// ---------------------------------------------------------------------------------------------
// The socket seam, the heartbeat options, and the structural record an attachment dispatches
// over.
// ---------------------------------------------------------------------------------------------

fn socket_interface(named: &str) -> String {
    format!(
        "/// The seam a `{named}` `ws_rpc` transport owns: the only place `onMessage` and\n\
         /// `onClose` are ever set, so a real socket satisfies this once and the transport takes\n\
         /// it from there.\n\
         interface {named}WsSocket {{\n  \
         fun send(text: String)\n  \
         fun close()\n  \
         var onMessage: ((String) -> Unit)?\n  \
         var onClose: (() -> Unit)?\n\
         }}"
    )
}

fn options_class(named: &str) -> String {
    format!(
        "/// How often a `{named}` `ws_rpc` transport pings the far side, and how long it waits\n\
         /// for the answering pong before closing the connection. `heartbeat = null` turns\n\
         /// probing off entirely.\n\
         data class {named}WsOptions(\n  \
         val heartbeat: Heartbeat? = Heartbeat(intervalMs = 30_000, timeoutMs = 10_000),\n\
         ) {{\n  \
         data class Heartbeat(val intervalMs: Long, val timeoutMs: Long)\n\
         }}"
    )
}

fn frames_class(named: &str) -> String {
    format!(
        "/// The structural seam a `{named}` `ws_rpc` transport publishes and\n\
         /// `attach{named}WsDispatcher` takes: every inbound frame the transport does not itself\n\
         /// correlate to a pending `request` (a ping and a pong included nowhere), the raw-text\n\
         /// sender to answer over, and the transport's own scope — so closing the transport\n\
         /// cancels this attachment, and every one it shared the socket with, along with it.\n\
         data class {named}WsFrames(\n  \
         val inbound: SharedFlow<String>,\n  \
         val send: (String) -> Unit,\n  \
         val scope: CoroutineScope,\n\
         )"
    )
}

// ---------------------------------------------------------------------------------------------
// The one exception a client still throws: a one-way method's own fault, having no reply arm to
// carry it through instead. Named apart from `kotlin_http_client`'s own bare `{named}Refusal` so
// both coexist in one bundle.
// ---------------------------------------------------------------------------------------------

fn refusal_class(named: &str) -> String {
    let fields = fault_fields_typescript_name(named);
    format!(
        "/// What a one-way `{named}` `ws_rpc` method throws when it cannot deliver its `notify`\n\
         /// frame. A one-way operation declares no error, so there is nothing else to throw.\n\
         class {named}WsRefusal(val fault: {fields}) : Exception(fault.detail)"
    )
}

// ---------------------------------------------------------------------------------------------
// The transport: correlates a `request` to its reply, answers an inbound ping, runs the
// heartbeat, and hands every other frame to `frames`.
// ---------------------------------------------------------------------------------------------

fn transport_class(named: &str) -> String {
    format!(
        "{header}{request}{notify_and_send}{on_frame}{heartbeat}{close}\n\
         }}",
        header = transport_header_stmt(named),
        request = transport_request_stmt(named),
        notify_and_send = transport_notify_and_send_stmt(named),
        on_frame = transport_on_frame_stmt(),
        heartbeat = transport_heartbeat_stmt(named),
        close = transport_close_stmt(named),
    )
}

/// The class's own doc, its constructor, the fields every other piece reaches for, `init`, and the
/// `frames` seam an attachment dispatches over.
fn transport_header_stmt(named: &str) -> String {
    format!(
        "/// A `{named}` `ws_rpc` transport that owns `socket` under the preferred ownership\n\
         /// shape: correlates a `request` to its reply through a `CompletableDeferred`, answers\n\
         /// an inbound ping with one pong, and runs its own heartbeat on a scope it owns and\n\
         /// hands out through `frames` — closing this transport cancels that scope, and with it\n\
         /// every dispatcher reading `frames`, shared ones included.\n\
         ///\n\
         /// `request` runs on its caller's coroutine, `onFrame` on whatever thread the socket\n\
         /// delivers on, and the heartbeat and `close` on the transport's own scope — so every\n\
         /// read and write of `pending`, `nextId` and `pongJob` is guarded by `synchronized(this)`,\n\
         /// a call to the socket or a pending deferred never made while holding it.\n\
         class {named}WsTransport(\n  \
         private val socket: {named}WsSocket,\n  \
         private val options: {named}WsOptions = {named}WsOptions(),\n\
         ) {{\n  \
         private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)\n  \
         private val pending = mutableMapOf<String, CompletableDeferred<JsonObject?>>()\n  \
         private val uncorrelated = MutableSharedFlow<String>(extraBufferCapacity = 64)\n  \
         private var nextId = 1\n  \
         private var pongJob: Job? = null\n  \
         private var closed = false\n\n  \
         init {{\n    \
         socket.onMessage = {{ text -> onFrame(text) }}\n    \
         socket.onClose = {{ close() }}\n    \
         scheduleHeartbeat()\n  \
         }}\n\n  \
         /// The structural seam an attachment dispatches over. Every `{named}WsTransport`\n  \
         /// exposes the same shape, so an attachment for any service sharing this socket reaches\n  \
         /// it here rather than through a transport type of its own.\n  \
         val frames: {named}WsFrames\n    \
         get() = {named}WsFrames(uncorrelated.asSharedFlow(), socket::send, scope)\n\n  "
    )
}

/// Assigns the next id and registers the pending deferred under one lock, then sends the request
/// frame and awaits the reply outside it.
fn transport_request_stmt(named: &str) -> String {
    format!(
        "/// Sends `operation` with `payload` as a `request` frame and answers with the matching\n  \
         /// `reply` frame's own fields, or `null` once the connection closes before one arrives.\n  \
         suspend fun request(operation: String, payload: JsonElement?): JsonObject? {{\n    \
         val deferred = CompletableDeferred<JsonObject?>()\n    \
         val id = synchronized(this) {{\n      \
         val assigned = (nextId++).toString()\n      \
         pending[assigned] = deferred\n      \
         assigned\n    \
         }}\n    \
         send(\n      \
         buildJsonObject {{\n        \
         put(\"kind\", \"request\")\n        \
         put(\"id\", id)\n        \
         put(\"service\", \"{named}\")\n        \
         put(\"operation\", operation)\n        \
         put(\"payload\", payload ?: JsonNull)\n      \
         }},\n    \
         )\n    \
         return deferred.await()\n  \
         }}\n\n  "
    )
}

fn transport_notify_and_send_stmt(named: &str) -> String {
    format!(
        "/// Sends `operation` with `payload` as a `notify` frame. No reply is expected.\n  \
         fun notify(operation: String, payload: JsonElement?) {{\n    \
         send(\n      \
         buildJsonObject {{\n        \
         put(\"kind\", \"notify\")\n        \
         put(\"service\", \"{named}\")\n        \
         put(\"operation\", operation)\n        \
         put(\"payload\", payload ?: JsonNull)\n      \
         }},\n    \
         )\n  \
         }}\n\n  \
         private fun send(frame: JsonObject) {{\n    \
         socket.send(frame.toString())\n  \
         }}\n\n  "
    )
}

/// Reads a frame off the socket: a pong cancels the outstanding watcher under one lock, a ping
/// draws one pong, and a reply either settles a pending deferred (removed under one lock) or,
/// unmatched, reaches `frames.inbound` beside every frame this service does not itself correlate.
fn transport_on_frame_stmt() -> String {
    "private fun onFrame(text: String) {\n    \
     val frame = try {\n      \
     Json.parseToJsonElement(text).jsonObject\n    \
     } catch (malformed: Throwable) {\n      \
     return\n    \
     }\n    \
     when ((frame[\"kind\"] as? JsonPrimitive)?.contentOrNull) {\n      \
     \"pong\" -> {\n        \
     val watcher = synchronized(this) {\n          \
     val current = pongJob\n          \
     pongJob = null\n          \
     current\n        \
     }\n        \
     watcher?.cancel()\n      \
     }\n      \
     \"ping\" -> send(buildJsonObject { put(\"kind\", \"pong\") })\n      \
     \"reply\" -> {\n        \
     val id = (frame[\"id\"] as? JsonPrimitive)?.contentOrNull\n        \
     val deferred = id?.let { key -> synchronized(this) { pending.remove(key) } }\n        \
     if (deferred != null) deferred.complete(frame) else uncorrelated.tryEmit(text)\n      \
     }\n      \
     else -> uncorrelated.tryEmit(text)\n    \
     }\n  \
     }\n\n  "
        .to_owned()
}

/// Arms the pong watcher under one lock before sending the ping — never after, which would let a
/// pong that answers faster than the watcher is assigned find nothing to cancel — and cancels
/// whatever watcher the previous round left outside the lock.
fn transport_heartbeat_stmt(named: &str) -> String {
    format!(
        "private fun scheduleHeartbeat() {{\n    \
         val heartbeat = options.heartbeat ?: return\n    \
         scope.launch {{\n      \
         while (isActive) {{\n        \
         delay(heartbeat.intervalMs)\n        \
         val previous = synchronized(this@{named}WsTransport) {{\n          \
         val current = pongJob\n          \
         pongJob = scope.launch {{\n            \
         delay(heartbeat.timeoutMs)\n            \
         close()\n          \
         }}\n          \
         current\n        \
         }}\n        \
         previous?.cancel()\n        \
         send(buildJsonObject {{ put(\"kind\", \"ping\") }})\n      \
         }}\n    \
         }}\n  \
         }}\n\n  "
    )
}

fn transport_close_stmt(named: &str) -> String {
    format!(
        "/// Closes the socket and settles every pending `request` with `null` — a value, not an\n  \
         /// error — which every waiting `{named}WsClient` method reads as the transport-failure\n  \
         /// fault, then cancels the scope `frames` hands out, tearing every dispatcher reading it\n  \
         /// down along with it.\n  \
         fun close() {{\n    \
         if (closed) return\n    \
         closed = true\n    \
         socket.close()\n    \
         val watcher = synchronized(this) {{ pongJob }}\n    \
         watcher?.cancel()\n    \
         val settled = synchronized(this) {{\n      \
         val waiting = pending.values.toList()\n      \
         pending.clear()\n      \
         waiting\n    \
         }}\n    \
         for (deferred in settled) deferred.complete(null)\n    \
         scope.cancel()\n  \
         }}"
    )
}

// ---------------------------------------------------------------------------------------------
// The client: one class, one constructor, one method per operation, calling out over the
// transport's own `request`/`notify`.
// ---------------------------------------------------------------------------------------------

fn client_class(service: &ServiceDef, named: &str, fn_prefix: &str) -> String {
    let methods = service
        .operations
        .iter()
        .map(|operation| client_method(named, fn_prefix, operation))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "/// A `{named}` caller over `ws_rpc`.\n\
         class {named}WsClient(private val transport: {named}WsTransport) {{\n\
         {methods}\n\
         }}"
    )
}

fn client_method(named: &str, fn_prefix: &str, operation: &OperationDef) -> String {
    let wire = &operation.wire_name;
    let call = &operation.ts_name;
    let req_ty = message_kotlin_typename(operation);
    let doc = format!("  /// Calls `{wire}` over `ws_rpc`.");
    match &operation.outcome {
        OperationOutcome::OneWay => format!(
            "{doc}\n  \
             suspend fun {call}(req: {req_ty}) {{\n    \
             try {{\n      \
             transport.notify(\"{wire}\", Json.encodeToJsonElement(serializer<{req_ty}>(), req))\n    \
             }} catch (uncarried: Throwable) {{\n      \
             throw {named}WsRefusal({fn_prefix}WsTransportFailure(\"{wire}\", uncarried.toString()))\n    \
             }}\n  \
             }}"
        ),
        OperationOutcome::Reply { error, success } => {
            let result = result_name(named, operation).unwrap();
            let decode = reply_decode_stmt(named, fn_prefix, wire, &result, error, success);
            format!(
                "{doc}\n  \
                 suspend fun {call}(req: {req_ty}): {result} {{\n    \
                 val reply = try {{\n      \
                 transport.request(\"{wire}\", Json.encodeToJsonElement(serializer<{req_ty}>(), req))\n    \
                 }} catch (uncarried: Throwable) {{\n      \
                 return {result}.Fault({fn_prefix}WsTransportFailure(\"{wire}\", uncarried.toString()))\n    \
                 }}\n    \
                 if (reply == null) {{\n      \
                 return {result}.Fault(\n        \
                 {fn_prefix}WsTransportFailure(\"{wire}\", \"the connection closed before a reply arrived\"),\n      \
                 )\n    \
                 }}\n\
{decode}\
                 }}"
            )
        }
    }
}

/// Reads the reply's own `ok`/`value`/`error` keys: the declared success on `true`, the wire's
/// own fault shape (`error.isServiceFault`) or the declared error otherwise.
fn reply_decode_stmt(
    named: &str,
    fn_prefix: &str,
    wire: &str,
    result: &str,
    error: &Type,
    success: &Type,
) -> String {
    let fields = fault_fields_typescript_name(named);
    let success_block = success_decode_block(fn_prefix, wire, result, success);
    let error_ty = kotlin_type_of(error);
    format!(
        "    val ok = (reply[\"ok\"] as? JsonPrimitive)?.booleanOrNull ?: false\n    \
         if (ok) {{\n{success_block}    }}\n    \
         val error = reply[\"error\"]\n    \
         val isServiceFault = (error as? JsonObject)?.get(\"isServiceFault\")\n      \
         ?.let {{ (it as? JsonPrimitive)?.booleanOrNull }} ?: false\n    \
         if (isServiceFault) {{\n      \
         return try {{\n        \
         {result}.Fault(\n          \
         Json.decodeFromJsonElement(serializer<{fields}>(), error.getValue(\"fault\")),\n        \
         )\n      \
         }} catch (rejected: Throwable) {{\n        \
         {result}.Fault({fn_prefix}WsFailedValidation(\"{wire}\", rejected.toString()))\n      \
         }}\n    \
         }}\n    \
         return try {{\n      \
         {result}.Declared(Json.decodeFromJsonElement(serializer<{error_ty}>(), error ?: JsonNull))\n    \
         }} catch (rejected: Throwable) {{\n      \
         {result}.Fault({fn_prefix}WsFailedValidation(\"{wire}\", rejected.toString()))\n    \
         }}\n"
    )
}

fn success_decode_block(fn_prefix: &str, wire: &str, result: &str, success: &Type) -> String {
    if is_unit_type(success) {
        return format!("      return {result}.Ok\n");
    }
    let success_ty = kotlin_type_of(success);
    format!(
        "      return try {{\n        \
         {result}.Ok(Json.decodeFromJsonElement(serializer<{success_ty}>(), reply.getValue(\"value\")))\n      \
         }} catch (rejected: Throwable) {{\n        \
         {result}.Fault({fn_prefix}WsFailedValidation(\"{wire}\", rejected.toString()))\n      \
         }}\n"
    )
}

// ---------------------------------------------------------------------------------------------
// Handlers: what an app implementing this service answers inbound frames with, and the
// attachment that dispatches them.
// ---------------------------------------------------------------------------------------------

fn handler_member(named: &str, operation: &OperationDef) -> String {
    let call = &operation.ts_name;
    let req_ty = message_kotlin_typename(operation);
    result_name(named, operation).map_or_else(
        || format!("  suspend fun {call}(req: {req_ty})"),
        |result| format!("  suspend fun {call}(req: {req_ty}): {result}"),
    )
}

fn handlers_interface(service: &ServiceDef, named: &str) -> String {
    let members = service
        .operations
        .iter()
        .map(|operation| handler_member(named, operation))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "/// What a `{named}` `ws_rpc` attachment dispatches an inbound frame to: one handler per\n\
         /// declared operation. A reply operation answers the same sealed result the client\n\
         /// reads rather than throwing its declared error; anything a handler throws regardless\n\
         /// reaches `onFault` instead.\n\
         interface {named}Handlers {{\n\
         {members}\n\
         }}"
    )
}

fn attachment_class(named: &str) -> String {
    format!(
        "/// One socket's `{named}` dispatcher: the scope it and every service it shared the\n\
         /// socket with run on, and the hook that shares that socket.\n\
         class {named}WsAttachment internal constructor(\n  \
         val scope: CoroutineScope,\n  \
         private val send: (String) -> Unit,\n  \
         private val job: Job,\n\
         ) {{\n  \
         private val shared = mutableListOf<() -> Unit>()\n\n  \
         /// Detaches every service this attachment shared the socket with, then this one.\n  \
         fun detach() {{\n    \
         shared.toList().forEach {{ it() }}\n    \
         shared.clear()\n    \
         job.cancel()\n  \
         }}\n\n  \
         /// Hands the guarded sender and this attachment's scope to another service's\n  \
         /// dispatcher. The detach it returns also runs when this attachment detaches. Throws\n  \
         /// once this attachment has detached.\n  \
         fun share(attach: (send: (String) -> Unit, scope: CoroutineScope) -> () -> Unit): () -> Unit {{\n    \
         check(job.isActive) {{ \"the socket is closed\" }}\n    \
         val detachShared = attach(send, scope)\n    \
         shared += detachShared\n    \
         return detachShared\n  \
         }}\n\
         }}"
    )
}

fn attach_dispatcher_fn(named: &str, fn_prefix: &str) -> String {
    let fields = fault_fields_typescript_name(named);
    format!(
        "/// Attaches `handlers` to `frames`, dispatching every inbound `{named}` frame, decoded\n\
         /// through the generated codec, to its own handler on `frames`' own scope — so closing\n\
         /// the transport this attachment reads from ends it too. A frame naming another\n\
         /// service, or carrying no operation at all, is left on `frames.inbound` for that\n\
         /// service's own attachment; one naming an operation this service does not declare\n\
         /// reaches `onFault` instead. Every frame that carried an id is answered, one-way\n\
         /// operations included, so a caller waiting on a reply is never left hanging.\n\
         fun attach{named}WsDispatcher(\n  \
         frames: {named}WsFrames,\n  \
         handlers: {named}Handlers,\n  \
         onFault: ({fields}) -> Unit,\n\
         ): {named}WsAttachment {{\n  \
         val job = frames.scope.launch {{\n    \
         frames.inbound.collect {{ text ->\n      \
         val frame = runCatching {{ Json.parseToJsonElement(text).jsonObject }}.getOrNull() ?: return@collect\n      \
         if ((frame[\"service\"] as? JsonPrimitive)?.contentOrNull != \"{named}\") return@collect\n      \
         val id = (frame[\"id\"] as? JsonPrimitive)?.contentOrNull\n      \
         val operation = (frame[\"operation\"] as? JsonPrimitive)?.contentOrNull ?: return@collect\n      \
         launch {{\n        \
         {fn_prefix}WsDispatch(id, operation, frame[\"payload\"], handlers, frames.send, onFault)\n      \
         }}\n    \
         }}\n  \
         }}\n  \
         return {named}WsAttachment(frames.scope, frames.send, job)\n\
         }}"
    )
}

/// The reply frame `{ok}` writes back over `send`, correlated to the inbound frame's own `id`.
fn reply_frame_expr(named: &str, ok: &str, key: &str, value_expr: &str) -> String {
    format!(
        "buildJsonObject {{ put(\"kind\", \"reply\"); put(\"id\", id); put(\"service\", \"{named}\"); \
         put(\"ok\", {ok}); put(\"{key}\", {value_expr}) }}.toString()"
    )
}

fn fault_envelope_expr(named: &str, fault_expr: &str) -> String {
    let fields = fault_fields_typescript_name(named);
    format!(
        "buildJsonObject {{ put(\"isServiceFault\", true); put(\"fault\", \
         Json.encodeToJsonElement(serializer<{fields}>(), {fault_expr})) }}"
    )
}

/// Answers a waiting request with the fault envelope — guarded on `id != null`, which is `null`
/// only for a `notify` frame.
fn fault_reply_stmt(named: &str, fault_expr: &str) -> String {
    let envelope = fault_envelope_expr(named, fault_expr);
    let frame = reply_frame_expr(named, "false", "error", &envelope);
    format!("        if (id != null) send({frame})\n")
}

fn dispatch_fn(service: &ServiceDef, named: &str, fn_prefix: &str) -> String {
    let fields = fault_fields_typescript_name(named);
    let arms = service
        .operations
        .iter()
        .map(|operation| dispatch_arm(named, fn_prefix, operation))
        .collect::<Vec<_>>()
        .join("\n");
    let unknown_fault = format!(
        "{fn_prefix}WsUnknownOperation(operation, \"this service answers to no operation by that name\")"
    );
    format!(
        "private suspend fun {fn_prefix}WsDispatch(\n  \
         id: String?,\n  \
         operation: String,\n  \
         payload: JsonElement?,\n  \
         handlers: {named}Handlers,\n  \
         send: (String) -> Unit,\n  \
         onFault: ({fields}) -> Unit,\n\
         ) {{\n  \
         when (operation) {{\n\
{arms}\n    \
         else -> {{\n      \
         val fault = {unknown_fault}\n      \
         onFault(fault)\n\
{unknown_reply}    \
         }}\n  \
         }}\n\
         }}",
        unknown_reply = fault_reply_stmt(named, "fault"),
    )
}

fn dispatch_arm(named: &str, fn_prefix: &str, operation: &OperationDef) -> String {
    let wire = &operation.wire_name;
    let req_ty = message_kotlin_typename(operation);
    let decode_fault = fault_reply_stmt(named, "fault");
    let mut arm = format!(
        "    \"{wire}\" -> {{\n      \
         val decoded = try {{\n        \
         Json.decodeFromJsonElement(serializer<{req_ty}>(), payload ?: JsonNull)\n      \
         }} catch (rejected: Throwable) {{\n        \
         val fault = {fn_prefix}WsFailedValidation(\"{wire}\", rejected.toString())\n        \
         onFault(fault)\n\
{decode_fault}        \
         return\n      \
         }}\n"
    );
    match &operation.outcome {
        OperationOutcome::OneWay => {
            let call = &operation.ts_name;
            let panic_fault = fault_reply_stmt(named, "fault");
            let ack = reply_frame_expr(named, "true", "value", "JsonNull");
            let _ = write!(
                arm,
                "      try {{\n        \
                 handlers.{call}(decoded)\n      \
                 }} catch (unexpected: Throwable) {{\n        \
                 val fault = {fn_prefix}WsHandlerPanic(\"{wire}\", unexpected.toString())\n        \
                 onFault(fault)\n\
{panic_fault}        \
                 return\n      \
                 }}\n      \
                 if (id != null) send({ack})\n    \
                 }}\n"
            );
        }
        OperationOutcome::Reply { error, success } => {
            let call = &operation.ts_name;
            let result = result_name(named, operation).unwrap();
            let panic_fault = fault_reply_stmt(named, "fault");
            let ok_value = if is_unit_type(success) {
                "JsonNull".to_owned()
            } else {
                let success_ty = kotlin_type_of(success);
                format!("Json.encodeToJsonElement(serializer<{success_ty}>(), answered.value)")
            };
            let ok_frame = reply_frame_expr(named, "true", "value", &ok_value);
            let error_ty = kotlin_type_of(error);
            let declared_frame = reply_frame_expr(
                named,
                "false",
                "error",
                &format!("Json.encodeToJsonElement(serializer<{error_ty}>(), answered.error)"),
            );
            let fault_envelope = fault_envelope_expr(named, "answered.fault");
            let fault_frame = reply_frame_expr(named, "false", "error", &fault_envelope);
            let _ = write!(
                arm,
                "      val answered = try {{\n        \
                 handlers.{call}(decoded)\n      \
                 }} catch (unexpected: Throwable) {{\n        \
                 val fault = {fn_prefix}WsHandlerPanic(\"{wire}\", unexpected.toString())\n        \
                 onFault(fault)\n\
{panic_fault}        \
                 return\n      \
                 }}\n      \
                 if (id != null) {{\n        \
                 when (answered) {{\n          \
                 is {result}.Ok -> send({ok_frame})\n          \
                 is {result}.Declared -> send({declared_frame})\n          \
                 is {result}.Fault -> send({fault_frame})\n        \
                 }}\n      \
                 }}\n    \
                 }}\n"
            );
        }
    }
    arm
}

// ---------------------------------------------------------------------------------------------
// The faults every method and every dispatch arm reaches for.
// ---------------------------------------------------------------------------------------------

fn fault_helpers(named: &str, fn_prefix: &str) -> Vec<String> {
    vec![
        fault_helper(
            named,
            fn_prefix,
            "WsTransportFailure",
            "TransportFailure",
            &format!(
                "The fault a `{named}` `ws_rpc` client answers with when the transport could not \
                 carry a call: the frame never went out, or the reply never came back."
            ),
        ),
        fault_helper(
            named,
            fn_prefix,
            "WsFailedValidation",
            "FailedValidation",
            &format!(
                "The fault a `{named}` `ws_rpc` reply answers with when it will not become the \
                 operation's own declared type through the generated codec."
            ),
        ),
        fault_helper(
            named,
            fn_prefix,
            "WsUnknownOperation",
            "UnknownOperation",
            &format!(
                "The fault a `{named}` `ws_rpc` attachment answers with when an inbound frame \
                 names an operation nothing on this service declares."
            ),
        ),
        fault_helper(
            named,
            fn_prefix,
            "WsHandlerPanic",
            "HandlerPanic",
            &format!(
                "The fault a `{named}` `ws_rpc` attachment answers with when a handler raises \
                 anything at all rather than answering the sealed result."
            ),
        ),
    ]
}

fn fault_helper(named: &str, fn_prefix: &str, suffix: &str, kind: &str, doc: &str) -> String {
    let fields = fault_fields_typescript_name(named);
    format!(
        "/// {doc}\n\
         private fun {fn_prefix}{suffix}(operation: String, detail: String): {fields} = {fields}(\n  \
         detail = detail,\n  \
         kind = {named}FaultKind.{kind},\n  \
         operation = operation,\n\
         )"
    )
}

// ---------------------------------------------------------------------------------------------
// Small, Kotlin-flavored value rendering, duplicated from `kotlin_http_client` rather than shared
// with it: this module needs none of its HTTP-shaped machinery, only a type's name.
// ---------------------------------------------------------------------------------------------

/// The message's Kotlin type: the type the operation named, or the one the macro declared for an
/// operation that named none — mirrors `kotlin_http_client`'s own `message_kotlin_typename`.
fn message_kotlin_typename(operation: &OperationDef) -> String {
    match &operation.inputs {
        OperationInputs::Named(declared) => kotlin_type_of(declared),
        OperationInputs::Empty | OperationInputs::Generated(_) => {
            operation.generated_message_ident().map_or_else(
                || "Unit".to_owned(),
                |ident| {
                    let named: Type = syn::parse_quote! { #ident };
                    kotlin_type_of(&named)
                },
            )
        }
    }
}

/// `ty`'s own Kotlin type name, read through the same `FieldDef` walk every field's type goes
/// through — mirrors `kotlin_http_client`'s own `kotlin_type_of`.
fn kotlin_type_of(ty: &Type) -> String {
    kotlin_typename(&get_field_def("value", ty, ""))
}
