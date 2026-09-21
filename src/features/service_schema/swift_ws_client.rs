//! The Swift `ws_rpc` client: a socket seam over which one actor correlates requests to their
//! replies, probes liveness on a heartbeat, checks every reply against the operation's own
//! declared type, and settles every waiting call with a `transport-failure` fault when the socket
//! closes — the TypeScript client's feature set, item for item, read at [`super::ws_client`].
//!
//! # One actor, not a transport plus a client
//!
//! The transport is an `actor` rather than a class guarded by a lock: a lock compiles but leaves a
//! forgotten `lock()` to break silently, while an actor has the compiler enforce the isolation.
//! There is also no seam left to cut between "the transport" and "the client": the reply's own
//! success or error type is known only to the
//! per-operation method, so the actor that owns the correlation map is the same actor that decodes
//! the reply. [`transport_actor`] is that one actor, carrying one public method per operation with
//! the REST client's own signatures; [`client_alias`] publishes `{Named}WsClient` as another name
//! for it, so a caller constructs it the way it constructs `{Named}HttpClient`.
//!
//! # Ordering: one stream, not one task per frame
//!
//! A socket's `onMessage` callback fires once per inbound frame, synchronously, in the order
//! frames arrived — but handing each one to the actor through its own freshly spawned `Task`
//! gives Swift no reason to run those tasks in that same order. [`transport_actor`] instead feeds
//! every inbound frame into one `AsyncStream`, fed by `onMessage` alone, and drains it with one
//! `for await` loop bound to the actor: `AsyncStream` delivers what was `yield`ed in the order it
//! was `yield`ed, and a single consuming loop processes one frame to completion before the next is
//! read, so arrival order is preserved end to end without depending on how the runtime happens to
//! schedule unstructured tasks.
//!
//! # The shared failure and refusal types
//!
//! `{Named}{Operation}Failure` and `{Named}Refusal` are declared once, by
//! [`super::swift_http_client`], and named here rather than redeclared — the same operation
//! answers the same failure whichever transport carried the call. What this module still owns is
//! the fault *it* can report on its own: a reply that will not decode, or a call that never
//! finished because the socket closed, built through [`transport_failure_helper`] and
//! [`failed_validation_helper`] and named with a `Ws` infix so a bundle carrying both clients
//! never declares two functions under one name.

use crate::features::swift::swift_reference_type;
use crate::field_type::get_field_def;
use crate::rename_rule::RenameRule;
use crate::service_schema::parse::{
    OperationDef, OperationInputs, OperationOutcome, ServiceDef, is_unit_type,
};
use syn::Type;

pub fn emit(service: &ServiceDef) -> Vec<String> {
    let named = service.ident.to_string();
    let prefix = RenameRule::CamelCase.apply_to_variant(&named);
    let mut published = vec![
        socket_type(&named),
        options_type(&named),
        support_types(&named),
    ];
    published.push(transport_failure_helper(&named, &prefix));
    published.push(failed_validation_helper(&named, &prefix));
    let mut aux = Vec::new();
    published.push(transport_actor(service, &named, &prefix, &mut aux));
    published.extend(aux);
    published.push(client_alias(&named));
    published
}

/// `{Named}{PascalOperation}Failure`, the failure arm every reply method answers with —
/// [`super::swift_http_client`]'s own name, read again here rather than redeclared. `None` for a
/// one-way operation, mirroring [`super::result::result_name`].
fn failure_name(named: &str, operation: &OperationDef) -> Option<String> {
    match &operation.outcome {
        OperationOutcome::OneWay => None,
        OperationOutcome::Reply {
            error: _error,
            success: _success,
        } => Some(format!(
            "{named}{}Failure",
            RenameRule::PascalCase.apply_to_field(&operation.ident.to_string())
        )),
    }
}

/// `{Named}Refusal`, the error every one-way method throws — also [`super::swift_http_client`]'s
/// own name.
fn refusal_name(named: &str) -> String {
    format!("{named}Refusal")
}

/// `{Named}Fault`, the shared fault type both the REST and the `ws_rpc` client answer a defect
/// with — [`super::swift_http_client`]'s own binding over the service's generated
/// `{Named}FaultFields`.
fn fault_name(named: &str) -> String {
    format!("{named}Fault")
}

// ---------------------------------------------------------------------------------------------
// The socket seam and the heartbeat options.
// ---------------------------------------------------------------------------------------------

/// The socket seam: four members, none of them naming a networking type, so an app's own
/// `URLSessionWebSocketTask` wrapper — or anything else that can hand text in and take text out —
/// satisfies it without an adapter.
fn socket_type(named: &str) -> String {
    format!(
        "/// What binds a `{named}` `ws_rpc` transport to a socket, in plain terms: send text \
         out,\n\
         /// close the connection, and hand every inbound text frame and the eventual close back \
         through\n\
         /// the two callbacks. Names no networking type.\n\
         public protocol {named}WsSocket: AnyObject, Sendable {{\n  \
         func send(_ text: String)\n  \
         func close()\n  \
         var onMessage: (@Sendable (String) -> Void)? {{ get set }}\n  \
         var onClose: (@Sendable () -> Void)? {{ get set }}\n\
         }}"
    )
}

/// How often the transport probes the socket, and how long it waits for the answer; `heartbeat`
/// left `nil` turns probing off. Defaults to a 30 second interval and a 10 second timeout.
fn options_type(named: &str) -> String {
    format!(
        "/// How often a `{named}` `ws_rpc` transport probes the socket, and how long it waits \
         for\n\
         /// the answering pong. `heartbeat` left `nil` turns probing off.\n\
         public struct {named}WsOptions: Sendable {{\n  \
         public struct Heartbeat: Sendable {{\n    \
         public var intervalMs: Int\n    \
         public var timeoutMs: Int\n    \
         public init(intervalMs: Int, timeoutMs: Int) {{\n      \
         self.intervalMs = intervalMs\n      \
         self.timeoutMs = timeoutMs\n    \
         }}\n  \
         }}\n  \
         public var heartbeat: Heartbeat?\n  \
         public init(heartbeat: Heartbeat? = Heartbeat(intervalMs: 30_000, timeoutMs: 10_000)) \
         {{\n    \
         self.heartbeat = heartbeat\n  \
         }}\n\
         }}"
    )
}

// ---------------------------------------------------------------------------------------------
// The frame and envelope shapes the actor encodes and decodes through — declared once per
// service, reused by every operation method.
// ---------------------------------------------------------------------------------------------

/// The outbound frame shapes and the inbound decode probes every operation method shares: a
/// generic `request`/`notify` writer over any `Encodable` payload, a bare probe reading `kind`,
/// `id` and `service` off a frame before its `value`/`error` shape is known, and the three
/// generic envelopes that read `value`, a declared `error`, or the wire's own
/// `{ isServiceFault, fault }` shape once it is.
fn support_types(named: &str) -> String {
    let fault = fault_name(named);
    format!(
        "struct {named}WsRequestFrame<Payload: Encodable>: Encodable {{\n  \
         let kind = \"request\"\n  \
         let id: String\n  \
         let service: String\n  \
         let operation: String\n  \
         let payload: Payload\n\
         }}\n\n\
         struct {named}WsNotifyFrame<Payload: Encodable>: Encodable {{\n  \
         let kind = \"notify\"\n  \
         let service: String\n  \
         let operation: String\n  \
         let payload: Payload\n\
         }}\n\n\
         struct {named}WsFrameProbe: Decodable {{\n  \
         let kind: String\n  \
         let id: String?\n  \
         let service: String?\n\
         }}\n\n\
         struct {named}WsOkProbe: Decodable {{\n  \
         let ok: Bool\n\
         }}\n\n\
         struct {named}WsValueEnvelope<Success: Decodable>: Decodable {{\n  \
         let value: Success\n\
         }}\n\n\
         struct {named}WsDeclaredEnvelope<Declared: Decodable>: Decodable {{\n  \
         let error: Declared\n\
         }}\n\n\
         struct {named}WsFaultEnvelope: Decodable {{\n  \
         struct Marker: Decodable {{\n    \
         let isServiceFault: Bool?\n    \
         let fault: {fault}?\n  \
         }}\n  \
         let error: Marker\n\
         }}"
    )
}

// ---------------------------------------------------------------------------------------------
// The two faults this transport can report on its own: a reply that would not become the
// operation's own declared shape, and a call that never finished because the socket closed.
// ---------------------------------------------------------------------------------------------

fn transport_failure_helper(named: &str, prefix: &str) -> String {
    let fault = fault_name(named);
    format!(
        "/// The fault a `{named}` `ws_rpc` call answers with when the transport could not carry \
         it:\n\
         /// the frame never went out, or the reply never came back before the socket closed.\n\
         func {prefix}WsTransportFailure(_ operation: String, _ detail: String) -> {fault} {{\n  \
         {fault}(detail: detail, field: nil, kind: .transportFailure, operation: operation)\n\
         }}"
    )
}

fn failed_validation_helper(named: &str, prefix: &str) -> String {
    let fault = fault_name(named);
    format!(
        "/// The fault a `{named}` `ws_rpc` call answers with when a reply will not become the \
         operation's\n\
         /// own declared success or error, or when a payload will not become the wire shape \
         going out.\n\
         func {prefix}WsFailedValidation(_ operation: String, _ detail: String) -> {fault} {{\n  \
         {fault}(detail: detail, field: nil, kind: .failedValidation, operation: operation)\n\
         }}"
    )
}

// ---------------------------------------------------------------------------------------------
// The transport actor.
// ---------------------------------------------------------------------------------------------

fn transport_actor(
    service: &ServiceDef,
    named: &str,
    prefix: &str,
    aux: &mut Vec<String>,
) -> String {
    let methods = service
        .operations
        .iter()
        .map(|operation| operation_method(named, prefix, operation, aux))
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "{header}{init}{close}{correlate}{notify}{handle_message}{heartbeat}{handle_close}\n\n\
         {methods}\n\
         }}",
        header = actor_header(named),
        init = actor_init(named),
        close = actor_close_method(),
        correlate = actor_correlate(named),
        notify = actor_send_notify(named),
        handle_message = actor_handle_message(named),
        heartbeat = actor_heartbeat_machinery(),
        handle_close = actor_handle_close(),
    )
}

fn actor_header(named: &str) -> String {
    format!(
        "/// A `{named}` transport over one `ws_rpc` socket, correlating requests to their \
         replies,\n\
         /// probing liveness on its own heartbeat, and settling every call still waiting with a\n\
         /// `transport-failure` fault once the socket closes. One method per operation, with the\n\
         /// REST client's own signatures.\n\
         public actor {named}WsTransport {{\n  \
         private let socket: any {named}WsSocket\n  \
         private let heartbeat: {named}WsOptions.Heartbeat?\n  \
         private var next = 0\n  \
         private var pending: [String: CheckedContinuation<Data?, Never>] = [:]\n  \
         private var pingTask: Task<Void, Never>?\n  \
         private var pongDeadlineTask: Task<Void, Never>?\n  \
         private var inboundContinuation: AsyncStream<String>.Continuation?\n  \
         private var closed = false\n\n"
    )
}

/// Wires the socket to one `AsyncStream`, so every inbound frame is decoded in the order it
/// arrived rather than through a `Task` per frame — see the module's own doc for why. `self` is
/// captured weakly in both spawned tasks so neither keeps the actor alive past its own last
/// strong reference.
fn actor_init(named: &str) -> String {
    format!(
        "  public init(socket: any {named}WsSocket, options: {named}WsOptions = .init()) {{\n    \
         self.socket = socket\n    \
         self.heartbeat = options.heartbeat\n    \
         let (inbound, outbound) = AsyncStream<String>.makeStream()\n    \
         self.inboundContinuation = outbound\n    \
         socket.onMessage = {{ text in outbound.yield(text) }}\n    \
         socket.onClose = {{ outbound.finish() }}\n    \
         Task {{ [weak self] in\n      \
         for await text in inbound {{\n        \
         await self?.handleMessage(text)\n      \
         }}\n      \
         await self?.handleClose()\n    \
         }}\n    \
         Task {{ [weak self] in await self?.schedulePing() }}\n  \
         }}\n\n"
    )
}

fn actor_close_method() -> String {
    "  public func close() {\n    \
     socket.close()\n    \
     handleClose()\n  \
     }\n\n"
        .to_owned()
}

/// Sends a `request` frame and awaits the matching `reply`'s own raw bytes, or `nil` once the
/// socket closes before one arrives. Registering the pending continuation and writing the frame
/// happen inside the same non-suspending closure, so no reply for this id can be read before it
/// is recorded.
fn actor_correlate(named: &str) -> String {
    format!(
        "  private func correlate<Payload: Encodable>(\n    \
         operation: String,\n    \
         payload: Payload,\n  \
         ) async throws -> Data? {{\n    \
         let id = String(next)\n    \
         next += 1\n    \
         let frame = {named}WsRequestFrame(id: id, service: \"{named}\", operation: operation, \
         payload: payload)\n    \
         let data = try JSONEncoder().encode(frame)\n    \
         if closed {{\n      \
         return nil\n    \
         }}\n    \
         let text = String(decoding: data, as: UTF8.self)\n    \
         return await withCheckedContinuation {{ (continuation: CheckedContinuation<Data?, \
         Never>) in\n      \
         pending[id] = continuation\n      \
         socket.send(text)\n    \
         }}\n  \
         }}\n\n"
    )
}

fn actor_send_notify(named: &str) -> String {
    format!(
        "  private func sendNotify<Payload: Encodable>(operation: String, payload: Payload) \
         throws {{\n    \
         let frame = {named}WsNotifyFrame(service: \"{named}\", operation: operation, payload: \
         payload)\n    \
         let data = try JSONEncoder().encode(frame)\n    \
         let text = String(decoding: data, as: UTF8.self)\n    \
         socket.send(text)\n  \
         }}\n\n"
    )
}

/// Reads one frame's `kind`: a `ping` is answered with a `pong`; a `pong` re-arms the next probe;
/// a `reply` naming this service and an id still pending resumes that continuation with the
/// frame's own raw bytes, for the waiting method to decode; anything else — another service, an
/// id nobody is waiting on, an unrecognised `kind` — is dropped.
fn actor_handle_message(named: &str) -> String {
    format!(
        "  private func handleMessage(_ text: String) async {{\n    \
         guard let data = text.data(using: .utf8) else {{ return }}\n    \
         guard let probe = try? JSONDecoder().decode({named}WsFrameProbe.self, from: data) else \
         {{ return }}\n    \
         switch probe.kind {{\n    \
         case \"ping\":\n      \
         socket.send(\"{{\\\"kind\\\":\\\"pong\\\"}}\")\n    \
         case \"pong\":\n      \
         pongDeadlineTask?.cancel()\n      \
         pongDeadlineTask = nil\n      \
         schedulePing()\n    \
         case \"reply\":\n      \
         guard probe.service == \"{named}\", let id = probe.id, let waiting = \
         pending.removeValue(forKey: id) else {{ return }}\n      \
         waiting.resume(returning: data)\n    \
         default:\n      \
         return\n    \
         }}\n  \
         }}\n\n"
    )
}

fn actor_heartbeat_machinery() -> String {
    "  private func schedulePing() {\n    \
     guard let heartbeat else { return }\n    \
     pingTask?.cancel()\n    \
     let intervalNanoseconds = UInt64(heartbeat.intervalMs) * 1_000_000\n    \
     pingTask = Task { [weak self] in\n      \
     try? await Task.sleep(nanoseconds: intervalNanoseconds)\n      \
     guard !Task.isCancelled else { return }\n      \
     await self?.sendPing()\n    \
     }\n  \
     }\n\n  \
     private func sendPing() {\n    \
     guard !closed, let heartbeat else { return }\n    \
     socket.send(\"{\\\"kind\\\":\\\"ping\\\"}\")\n    \
     pongDeadlineTask?.cancel()\n    \
     let timeoutNanoseconds = UInt64(heartbeat.timeoutMs) * 1_000_000\n    \
     pongDeadlineTask = Task { [weak self] in\n      \
     try? await Task.sleep(nanoseconds: timeoutNanoseconds)\n      \
     guard !Task.isCancelled else { return }\n      \
     await self?.pongMissed()\n    \
     }\n  \
     }\n\n  \
     private func pongMissed() {\n    \
     guard !closed else { return }\n    \
     socket.close()\n    \
     handleClose()\n  \
     }\n\n"
        .to_owned()
}

/// Tears the transport down once, whether reached from the socket's own close, a missed pong, or
/// the transport's own [`actor_close_method`]: stops the heartbeat, ends the inbound stream, and
/// resumes every request still waiting with `nil` — a value, not a thrown error — which each
/// waiting method reads as the transport-failure fault.
fn actor_handle_close() -> String {
    "  private func handleClose() {\n    \
     guard !closed else { return }\n    \
     closed = true\n    \
     pingTask?.cancel()\n    \
     pongDeadlineTask?.cancel()\n    \
     inboundContinuation?.finish()\n    \
     pingTask = nil\n    \
     pongDeadlineTask = nil\n    \
     let waiting = pending\n    \
     pending.removeAll()\n    \
     for continuation in waiting.values {\n      \
     continuation.resume(returning: nil)\n    \
     }\n  \
     }"
    .to_owned()
}

// ---------------------------------------------------------------------------------------------
// One method per operation.
// ---------------------------------------------------------------------------------------------

fn operation_method(
    named: &str,
    prefix: &str,
    operation: &OperationDef,
    aux: &mut Vec<String>,
) -> String {
    match &operation.outcome {
        OperationOutcome::OneWay => one_way_method(named, prefix, operation, aux),
        OperationOutcome::Reply { error, success } => {
            reply_method(named, prefix, operation, error, success, aux)
        }
    }
}

fn one_way_method(
    named: &str,
    prefix: &str,
    operation: &OperationDef,
    aux: &mut Vec<String>,
) -> String {
    let call = &operation.ts_name;
    let wire = &operation.wire_name;
    let (param_ty, param_aux) = message_swift_type(operation);
    aux.extend(param_aux);
    let refusal = refusal_name(named);
    format!(
        "  /// Calls `{wire}` over `ws_rpc`.\n  \
         public func {call}(_ req: {param_ty}) async throws {{\n    \
         do {{\n      \
         try sendNotify(operation: \"{wire}\", payload: req)\n    \
         }} catch {{\n      \
         throw {refusal}(fault: {prefix}WsFailedValidation(\"{wire}\", \"\\(error)\"))\n    \
         }}\n  \
         }}"
    )
}

fn reply_method(
    named: &str,
    prefix: &str,
    operation: &OperationDef,
    error: &Type,
    success: &Type,
    aux: &mut Vec<String>,
) -> String {
    let call = &operation.ts_name;
    let wire = &operation.wire_name;
    let (param_ty, param_aux) = message_swift_type(operation);
    aux.extend(param_aux);
    let failure = failure_name(named, operation).unwrap();
    let unit = is_unit_type(success);
    let (success_ty, success_block) = if unit {
        (
            "Void".to_owned(),
            "        return .success(())\n".to_owned(),
        )
    } else {
        let field_hint = format!(
            "{named}{}Success",
            RenameRule::PascalCase.apply_to_field(&operation.ident.to_string())
        );
        let (success_ty, success_aux) =
            swift_reference_type(&get_field_def("value", success, ""), &field_hint);
        aux.extend(success_aux);
        let block = format!(
            "        do {{\n          \
             let decoded = try JSONDecoder().decode({named}WsValueEnvelope<{success_ty}>.self, \
             from: raw)\n          \
             return .success(decoded.value)\n        \
             }} catch {{\n          \
             return .failure(.fault({prefix}WsFailedValidation(\"{wire}\", \"\\(error)\")))\n        \
             }}\n"
        );
        (success_ty, block)
    };
    let error_hint = format!(
        "{named}{}Error",
        RenameRule::PascalCase.apply_to_field(&operation.ident.to_string())
    );
    let (error_ty, error_aux) =
        swift_reference_type(&get_field_def("error", error, ""), &error_hint);
    aux.extend(error_aux);
    format!(
        "  /// Calls `{wire}` over `ws_rpc`.\n  \
         public func {call}(_ req: {param_ty}) async -> Result<{success_ty}, {failure}> {{\n    \
         let raw: Data?\n    \
         do {{\n      \
         raw = try await correlate(operation: \"{wire}\", payload: req)\n    \
         }} catch {{\n      \
         return .failure(.fault({prefix}WsFailedValidation(\"{wire}\", \"\\(error)\")))\n    \
         }}\n    \
         guard let raw else {{\n      \
         return .failure(.fault({prefix}WsTransportFailure(\"{wire}\", \"the socket closed \
         before the reply arrived\")))\n    \
         }}\n    \
         let ok: Bool\n    \
         do {{\n      \
         ok = try JSONDecoder().decode({named}WsOkProbe.self, from: raw).ok\n    \
         }} catch {{\n      \
         return .failure(.fault({prefix}WsFailedValidation(\"{wire}\", \"\\(error)\")))\n    \
         }}\n    \
         if ok {{\n\
         {success_block}    \
         }}\n    \
         if let probed = try? JSONDecoder().decode({named}WsFaultEnvelope.self, from: raw),\n       \
         probed.error.isServiceFault == true,\n       \
         let fault = probed.error.fault {{\n      \
         return .failure(.fault(fault))\n    \
         }}\n    \
         do {{\n      \
         let declared = try JSONDecoder().decode({named}WsDeclaredEnvelope<{error_ty}>.self, \
         from: raw)\n      \
         return .failure(.declared(declared.error))\n    \
         }} catch {{\n      \
         return .failure(.fault({prefix}WsFailedValidation(\"{wire}\", \"\\(error)\")))\n    \
         }}\n  \
         }}"
    )
}

/// [`crate::features::swift::swift_reference_type`] for the operation's own message: the type an
/// operation's one argument already is, or the type the macro declared for an operation that
/// named none — mirrors `dart_ws_client`'s own `message_dart_typename`.
fn message_swift_type(operation: &OperationDef) -> (String, Vec<String>) {
    match &operation.inputs {
        OperationInputs::Named(declared) => {
            swift_reference_type(&get_field_def("req", declared, ""), "Req")
        }
        OperationInputs::Empty | OperationInputs::Generated(_) => {
            operation.generated_message_ident().map_or_else(
                || ("String".to_owned(), Vec::new()),
                |ident| {
                    let named: Type = syn::parse_quote! { #ident };
                    swift_reference_type(&get_field_def("req", &named, ""), "Req")
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The client callers construct.
// ---------------------------------------------------------------------------------------------

fn client_alias(named: &str) -> String {
    format!(
        "/// A `{named}` caller over `ws_rpc` — the same actor \
         `{named}WsTransport` is, under the\n\
         /// name a caller constructs, mirroring `{named}HttpClient`'s own method signatures.\n\
         public typealias {named}WsClient = {named}WsTransport"
    )
}
