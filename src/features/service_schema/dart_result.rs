//! The `{Service}{Operation}Result` sealed class pair: Dart's own construct for the two-armed
//! outcome every client in every language now returns from a request-and-reply call.
//!
//! Built the way [`crate::features::dart`] already builds a sealed hierarchy for a Rust enum whose
//! variants carry payloads — a `sealed` base and one `final` subclass per arm — except this pair
//! carries no `fromJson`/`toJson`: it is a value the client builds for its own caller, never a
//! shape a codec reads off the wire.
//!
//! Named exactly as [`super::result`] names the TypeScript twin (`{Service}{Operation}Result`), and
//! its members exactly as [`crate::features::dart`] names every other sealed member: the base name
//! plus the arm it stands for — `Ok` for the success, `Operation` and `Fault` for the two arms of
//! the Rust client's own `CallError`, and `Cancelled` for the one arm neither of those two names: a
//! `{Service}HttpTransportCancelled` the transport threw ([`super::dart_http_client`]'s own
//! `send_stmt_reply`), carrying nothing beyond its own type, so a caller tells "the user backed
//! out" apart from a `Fault` by matching on the pair rather than parsing a fault's own detail.
//!
//! A one-way operation declared no reply and therefore no pair, mirroring [`super::result`].

use super::dart_http_client::{carries_no_value, dart_success_type, dart_type_of};
use super::result::result_name;
use crate::service_schema::parse::{HttpShape, OperationDef, OperationOutcome, ServiceDef};
use crate::service_schema::support::fault_fields_typescript_name;

pub fn emit(service: &ServiceDef) -> Vec<String> {
    let named = service.ident.to_string();
    service
        .operations
        .iter()
        .filter_map(|operation| result_pair(&named, operation))
        .collect()
}

fn result_pair(named: &str, operation: &OperationDef) -> Option<String> {
    let OperationOutcome::Reply {
        error,
        success: _success,
    } = &operation.outcome
    else {
        return None;
    };
    let published = result_name(named, operation)?;
    let shape = HttpShape::of(operation);
    let ok_member = if carries_no_value(operation, &shape) {
        format!("final class {published}Ok extends {published} {{\n  const {published}Ok();\n}}")
    } else {
        let value = dart_success_type(operation, &shape);
        format!(
            "final class {published}Ok extends {published} {{\n  \
             const {published}Ok(this.value);\n  final {value} value;\n}}"
        )
    };
    let failure = dart_type_of(error);
    let fault = fault_fields_typescript_name(named);
    let ident = &operation.ident;
    Some(format!(
        "/// What `{ident}` answers: the success, the error the operation declared, a fault it \
         never\n\
         /// declared, or that the caller itself cancelled the call.\n\
         sealed class {published} {{\n  const {published}();\n}}\n\n\
         {ok_member}\n\n\
         final class {published}Operation extends {published} {{\n  \
         const {published}Operation(this.error);\n  final {failure} error;\n}}\n\n\
         final class {published}Fault extends {published} {{\n  \
         const {published}Fault(this.fault);\n  final {fault} fault;\n}}\n\n\
         final class {published}Cancelled extends {published} {{\n  \
         const {published}Cancelled();\n}}"
    ))
}
