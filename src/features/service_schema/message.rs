//! How the client and the service name the one message an operation receives.
//!
//! Read by the client and the dispatcher and by nothing else, so it is gated with them: only a
//! build with the Zod surface publishes either, and a name and a schema nobody asks for is dead
//! code.
//!
//! Both sides take the message as a single object — `getBalance(req)` — where Rust unpacks an
//! argument list, because that is what a TypeScript caller of the hand-written client types today.
//! Which type that is, and which schema validates it, is read off the parsed operation rather than
//! decided again on each side, so the two cannot disagree about what crosses.

use crate::field_type::get_field_def;
use crate::rename_rule::RenameRule;
use crate::service_schema::parse::{
    HttpShape, OperationDef, OperationInputs, ScalarKind, option_inner, scalar_kind, vec_inner,
};
use syn::Type;

/// The schema the message validates against: the one `#[model_schema()]` published for it, read
/// through the same field walk every other reference to the type goes through rather than by
/// pasting a suffix onto a name.
pub fn schema(operation: &OperationDef) -> String {
    match &operation.inputs {
        OperationInputs::Named(declared) => get_field_def("req", declared, "").zod_type(),
        OperationInputs::Empty | OperationInputs::Generated(_) => operation
            .generated_message_ident()
            .map_or_else(String::new, |declared| {
                let named: syn::Type = syn::parse_quote! { #declared };
                get_field_def("req", &named, "").zod_type()
            }),
    }
}

/// The message's TypeScript name: the type the operation named, or the one the macro declared for
/// an operation that named none.
pub fn typename(operation: &OperationDef) -> String {
    match &operation.inputs {
        OperationInputs::Named(declared) => {
            get_field_def("req", declared, "").typescript_typename()
        }
        OperationInputs::Empty | OperationInputs::Generated(_) => operation
            .generated_message_ident()
            .map_or_else(|| "unknown".to_owned(), |declared| declared.to_string()),
    }
}

/// The arguments a bound header and a bound part add after the message, in declaration order,
/// named and typed the same way for the client's method and the implementation's.
pub fn binding_params(shape: &HttpShape) -> Vec<(String, String)> {
    let mut params = Vec::new();
    for header in &shape.header_in {
        let name = RenameRule::CamelCase.apply_to_field(&header.parameter.to_string());
        params.push((
            name.clone(),
            get_field_def(&name, &header.ty, "").typescript_typename(),
        ));
    }
    for part in &shape.multipart_parts {
        let name = RenameRule::CamelCase.apply_to_field(&part.parameter.to_string());
        params.push((
            name.clone(),
            get_field_def(&name, &part.ty, "").typescript_typename(),
        ));
    }
    params
}

/// Mirrors the Rust `decode_expr`: a `Vec<...>` splits `raw` on `,` and coerces each piece the
/// same way; otherwise a boolean text, a numeric coercion, or the text itself. Read by the REST
/// server's own query and path decoding and by the dispatcher's own header and part decoding, so
/// both emitters coerce a raw wire string through one rule.
pub fn decode_ts_expr(ty: &Type, raw: &str, prefix: &str) -> String {
    let base = option_inner(ty).unwrap_or(ty);
    if let Some(inner) = vec_inner(base) {
        let element = decode_ts_expr(inner, "piece", prefix);
        return format!("({raw}).split(\",\").map((piece: string) => {element})");
    }
    match scalar_kind(base) {
        ScalarKind::Bool => {
            format!("({raw} === \"true\" ? true : {raw} === \"false\" ? false : {raw})")
        }
        ScalarKind::Number => format!("{prefix}HttpCoerceNumber({raw})"),
        ScalarKind::Text => raw.to_owned(),
    }
}
