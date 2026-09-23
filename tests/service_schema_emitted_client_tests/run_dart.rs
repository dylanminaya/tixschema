//! The emitted Dart client run by the Dart VM, with a real message object.
//!
//! The Dart twin of [`super::run_node`], asking the same three questions. Dart erases nothing, so
//! the module carries the real generated classes: the client calls the `toJson` the `dart` backend
//! wrote, not a stand-in.

#![cfg(feature = "dart")]

use super::runtime::ran;
use super::tests::{
    ConversationClientServiceSchema, conversation_id_dart, window_error_dart, window_page_dart,
    window_request_dart,
};

/// Names the runtime to run, for a machine that has one somewhere other than `PATH`.
const RUNTIME_VAR: &str = "TIXSCHEMA_DART";

/// Records the request it is handed and answers each operation's own declared status. Implements
/// the seam's own full record shape — `bodyStream` and `parts` included — even though this
/// service declares neither streaming nor multipart, exactly as fdz's fix requires every service's
/// transport interface to read the identical anonymous shape.
const DRIVER: &str = "
class _Recorder implements ConversationClientServiceHttpTransport {
  final List<Map<String, String>> sent = <Map<String, String>>[];

  @override
  Future<({int status, List<(String, String)> headers, List<int> body, Stream<List<int>> bodyStream})> send(
    ({String method, String path, String query, List<(String, String)> headers, List<int> body, List<(String, dynamic)> parts}) request,
  ) async {
    sent.add(<String, String>{
      'method': request.method,
      'path': request.path,
      'query': request.query,
    });
    if (request.method == 'DELETE') {
      return (status: 204, headers: <(String, String)>[], body: <int>[], bodyStream: const Stream<List<int>>.empty());
    }
    return (
      status: 200,
      headers: <(String, String)>[],
      body: utf8.encode(jsonEncode(<String, dynamic>{'items': <String>[]})),
      bodyStream: const Stream<List<int>>.empty(),
    );
  }
}

void main() async {
  final recorder = _Recorder();
  final client = ConversationClientServiceHttpClient(recorder);
  final outcome = await client.window(WindowRequest(
    conversation_id: '652f1a3b4c5d6e7f8a9b0c1d',
    limit: 10,
  ));
  await client.window(WindowRequest(conversation_id: '652f1a3b4c5d6e7f8a9b0c1d'));
  await client.purgeConversation(ConversationId('652f1a3b4c5d6e7f8a9b0c1d'));
  final ok = outcome is ConversationClientServiceWindowResultOk;
  print(jsonEncode(<String, dynamic>{
    'sent': recorder.sent,
    'ok': ok,
    'items': ok ? (outcome as ConversationClientServiceWindowResultOk).value.items : null,
  }));
}
";

/// An adapter whose every `send` throws the marker exception this crate's own `dart_http_client`
/// emits — never a network or server failure, but the caller cancelling. Answers `window` (a
/// reply operation) and `purgeConversation` (one-way) each once, so the driver can report how
/// each outcome told a cancellation apart from an ordinary fault.
const CANCEL_DRIVER: &str = "
class _CancellingTransport implements ConversationClientServiceHttpTransport {
  @override
  Future<({int status, List<(String, String)> headers, List<int> body, Stream<List<int>> bodyStream})> send(
    ({String method, String path, String query, List<(String, String)> headers, List<int> body, List<(String, dynamic)> parts}) request,
  ) async {
    throw ConversationClientServiceHttpTransportCancelled();
  }
}

void main() async {
  final client = ConversationClientServiceHttpClient(_CancellingTransport());
  final outcome = await client.window(WindowRequest(conversation_id: '652f1a3b4c5d6e7f8a9b0c1d'));
  var oneWayRethrewTheMarker = false;
  var oneWayThrewSomethingElse = false;
  try {
    await client.purgeConversation(ConversationId('652f1a3b4c5d6e7f8a9b0c1d'));
  } on ConversationClientServiceHttpTransportCancelled {
    oneWayRethrewTheMarker = true;
  } catch (_) {
    oneWayThrewSomethingElse = true;
  }
  print(jsonEncode(<String, dynamic>{
    'replyIsCancelled': outcome is ConversationClientServiceWindowResultCancelled,
    'replyIsFault': outcome is ConversationClientServiceWindowResultFault,
    'oneWayRethrewTheMarker': oneWayRethrewTheMarker,
    'oneWayThrewSomethingElse': oneWayThrewSomethingElse,
  }));
}
";

/// The generated classes the client calls, the client, and the driver.
fn module() -> String {
    [
        "import 'dart:convert';".to_owned(),
        conversation_id_dart::dart_definition(),
        window_request_dart::dart_definition(),
        window_page_dart::dart_definition(),
        window_error_dart::dart_definition(),
        ConversationClientServiceSchema::dart_definition(),
        ConversationClientServiceSchema::dart_http_client(),
        DRIVER.to_owned(),
    ]
    .join("\n\n")
}

/// What the driver wrote: the requests it recorded, and the outcome of its first `window` call —
/// `None` where no runtime was reachable.
fn driven() -> Option<serde_json::Value> {
    let wrote = ran("dart", RUNTIME_VAR, "dart", "client.dart", &module())?;
    Some(serde_json::from_str(wrote.trim()).unwrap())
}

/// The requests the driver recorded, or `None` where no runtime was reachable.
fn sent() -> Option<Vec<serde_json::Value>> {
    driven().map(|written| written["sent"].as_array().unwrap().clone())
}

/// The generated classes the cancelling adapter calls, the client, and [`CANCEL_DRIVER`].
fn cancel_module() -> String {
    [
        "import 'dart:convert';".to_owned(),
        conversation_id_dart::dart_definition(),
        window_request_dart::dart_definition(),
        window_page_dart::dart_definition(),
        window_error_dart::dart_definition(),
        ConversationClientServiceSchema::dart_definition(),
        ConversationClientServiceSchema::dart_http_client(),
        CANCEL_DRIVER.to_owned(),
    ]
    .join("\n\n")
}

/// What [`CANCEL_DRIVER`] wrote, or `None` where no runtime was reachable.
fn driven_cancel() -> Option<serde_json::Value> {
    let wrote = ran(
        "dart",
        RUNTIME_VAR,
        "dart",
        "cancel_client.dart",
        &cancel_module(),
    )?;
    Some(serde_json::from_str(wrote.trim()).unwrap())
}

#[test]
fn dart_definition_publishes_the_result_pair_and_no_pair_for_the_one_way_operation() {
    let written = ConversationClientServiceSchema::dart_definition();
    assert!(
        written.contains(
            "sealed class ConversationClientServiceWindowResult {\n  \
             const ConversationClientServiceWindowResult();\n}"
        ),
        "got: {written}"
    );
    assert!(
        written.contains(
            "final class ConversationClientServiceWindowResultOk extends \
             ConversationClientServiceWindowResult {\n  \
             const ConversationClientServiceWindowResultOk(this.value);\n  \
             final WindowPage value;\n}"
        ),
        "got: {written}"
    );
    assert!(
        written.contains(
            "final class ConversationClientServiceWindowResultOperation extends \
             ConversationClientServiceWindowResult {\n  \
             const ConversationClientServiceWindowResultOperation(this.error);\n  \
             final WindowError error;\n}"
        ),
        "got: {written}"
    );
    assert!(
        written.contains(
            "final class ConversationClientServiceWindowResultFault extends \
             ConversationClientServiceWindowResult {\n  \
             const ConversationClientServiceWindowResultFault(this.fault);\n  \
             final ConversationClientServiceFaultFields fault;\n}"
        ),
        "got: {written}"
    );
    assert!(
        !written.contains("PurgeConversationResult"),
        "purge_conversation is one-way and declared no reply to join into a pair. Got: {written}"
    );
}

#[test]
fn window_answers_the_ok_result_pair_carrying_the_recorded_items() {
    let Some(written) = driven() else {
        return;
    };
    assert_eq!(
        written["ok"], true,
        "the recorder answers `window`'s own declared `ok_status`, so the client's `Future` \
         resolves to the pair's `Ok` member rather than `Operation` or `Fault`. Got: {written:#?}"
    );
    assert_eq!(
        written["items"],
        serde_json::json!([]),
        "the `Ok` member carries the page the recorder's own canned body decoded into. \
         Got: {written:#?}"
    );
}

#[test]
fn a_lone_placeholder_sends_the_field_it_names_and_never_the_rendered_message() {
    let Some(sent) = sent() else {
        return;
    };
    assert_eq!(
        sent[0]["path"], "/v1/conversations/652f1a3b4c5d6e7f8a9b0c1d/window",
        "the placeholder is filled by the field it names. Got: {sent:#?}"
    );
    assert!(
        !sent[0]["path"].as_str().unwrap().contains("%7B"),
        "rendering the whole message puts its own map spelling in the segment. Got: {sent:#?}"
    );
}

#[test]
fn a_field_the_path_does_not_bind_reaches_the_query_string() {
    let Some(sent) = sent() else {
        return;
    };
    assert_eq!(
        sent[0]["query"], "limit=10",
        "`limit` is bound to no placeholder, so the query string is the only place left for it. \
         Got: {sent:#?}"
    );
    assert_eq!(
        sent[1]["query"], "",
        "the same operation with `limit` absent sends no key for it rather than `limit=null`. \
         Got: {sent:#?}"
    );
}

#[test]
fn a_scalar_message_is_still_the_whole_segment() {
    let Some(sent) = sent() else {
        return;
    };
    assert_eq!(
        sent[2]["path"], "/v1/conversations/652f1a3b4c5d6e7f8a9b0c1d",
        "a message that already is a wire scalar has no field to read: it is the segment. \
         Got: {sent:#?}"
    );
    assert_eq!(
        sent[2]["query"], "",
        "and no key is left over for a query. Got: {sent:#?}"
    );
}

/// jzv, end to end: an adapter throwing the emitted marker exception answers the result pair's
/// own `Cancelled` member for a reply operation — never the generic transport `Fault` a real
/// network failure would also produce — and a one-way operation rethrows the same marker rather
/// than wrapping it into `ConversationClientServiceHttpRefusal`.
#[test]
fn a_transport_cancellation_is_told_apart_from_a_fault_by_type_for_both_a_reply_and_a_one_way_call()
{
    let Some(written) = driven_cancel() else {
        return;
    };
    assert_eq!(
        written["replyIsCancelled"], true,
        "a reply operation whose transport threw the marker answers the pair's own `Cancelled` \
         member. Got: {written:#?}"
    );
    assert_eq!(
        written["replyIsFault"], false,
        "a cancellation must never also read as the generic transport fault a real network \
         failure would produce. Got: {written:#?}"
    );
    assert_eq!(
        written["oneWayRethrewTheMarker"], true,
        "a one-way operation has no reply arm to carry a cancellation through, so it rethrows \
         the same marker the caller catches by type. Got: {written:#?}"
    );
    assert_eq!(
        written["oneWayThrewSomethingElse"], false,
        "the one-way call must not wrap the cancellation into `ConversationClientServiceHttpRefusal` \
         instead. Got: {written:#?}"
    );
}
