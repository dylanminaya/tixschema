//! Kotlin type generation with `kotlinx.serialization` annotations: one `kotlin_definition()` per
//! `#[model_schema]` item in a `{snake}_kotlin` module, dispatched from `exec_model_schema` the way
//! the Dart backend is. Unlike Dart, Kotlin has no built-in JSON codec — the compiler plugin
//! `kotlinx.serialization` reads the annotations this module writes and generates the encoder and
//! decoder itself, so most shapes carry nothing beyond `@Serializable`/`@SerialName`. The four
//! shapes the plugin cannot express declaratively (an adjacently- or externally-tagged enum, an
//! untagged enum, and a tuple) carry a small generated `KSerializer` beside them instead.
//!
//! Fully independent of the `typescript`/`zod`/`jsonschema` module-and-delegate machinery, exactly
//! as `features::dart` is: it reads its own borrow of the item ahead of the
//! `process_struct`/`process_enum`/`process_type_alias` dispatch and carries no factory-cache or
//! forward-reference deferral of its own, Kotlin resolving a reference across the whole file
//! regardless of declaration order just as Dart does.

use core::cell::{Cell, RefCell};
use std::collections::HashMap;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Fields, Ident, Item, ItemEnum, ItemStruct, ItemType, Variant};

use crate::features::model_schema_prop::parse_model_schema_prop_attributes;
use crate::features::serde::parse_serde_key_omission;
use crate::field_type::{
    FieldDef, FieldDefType, VariantKind, classify_variant, get_field_def, is_plain_enum,
    is_sequence_wrapper,
};
use crate::rename_rule::{RenameRule, resolve_rename_rule};
use crate::utils::{
    compute_alias_export_name, compute_item_export_name, to_snake_case, type_parameters_in_scope,
};

#[cfg(feature = "serde")]
use crate::features::serde::{parse_serde_field_attributes, parse_serde_type_attributes};

/// One field this module has decided belongs on the wire: its Rust name (camel-cased into the
/// Kotlin property spelling by [`kotlin_property_name`]), its wire name, and the [`FieldDef`]
/// describing its type.
struct KotlinField {
    field_def: FieldDef,
    rust_name: String,
    wire_name: String,
}

/// The container attributes read off any enum, in the shape `process_enum` itself dispatches on —
/// `tag`/`content`/`untagged` default to "written none of them" without the `serde` feature, which
/// is what leaves an all-unit enum publishing the plain enum-class shape regardless.
struct EnumTagAttrs {
    content: Option<String>,
    rename_all: Option<String>,
    rename_all_fields: Option<String>,
    tag: Option<String>,
    untagged: bool,
}

/// One tagged/untagged variant's payload, classified once so every builder below reads it the same
/// way. A `TupleMultiple` payload is folded into one `Tuple` [`FieldDef`], matching how a slot list
/// renders everywhere else in this module.
enum VariantPayload {
    Named(Vec<KotlinField>),
    Unit,
    Value(Box<FieldDef>),
}

thread_local! {
    /// The Kotlin class/enum name each Rust ident publishes — the one thing a reference to a
    /// sibling item needs, since the reference's own field carries the type arguments. Independent
    /// of the three-surface `ALIAS_INFO` registry in `crate::utils`, for the same reason
    /// `DART_NAMES` is: Kotlin has no forward-reference or cycle problem to share bookkeeping over.
    static KOTLIN_NAMES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// The auxiliary top-level declarations a Tuple-shaped field earns — a wrapper `data class` and
    /// the `KSerializer` that reads and writes it as a JSON array (see [`tuple_wrapper_typename`]).
    /// Drained by [`kotlin_module_tokens`], the one choke point every dispatch path returns
    /// through, so an item's own text always carries whatever its own fields queued.
    static KOTLIN_TUPLE_AUX: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Monotonic across the whole compilation, matching how `KOTLIN_NAMES` persists across
    /// invocations — never reset, since every tuple wrapper this module ever writes lands in the
    /// same consuming file and must not collide with any other item's.
    static KOTLIN_TUPLE_COUNTER: Cell<u32> = const { Cell::new(0) };
}

/// The Kotlin tokens `item` earns, given the `name = "..."` override an author declared on it.
/// Dispatches on the item's own shape; an item this module has nothing to say about earns nothing.
pub fn kotlin_schema_dispatch(item: &Item, name_override: Option<&str>) -> TokenStream {
    if let Item::Struct(item_struct) = item {
        struct_kotlin_tokens(item_struct, name_override)
    } else if let Item::Enum(item_enum) = item {
        enum_kotlin_tokens(item_enum, name_override)
    } else if let Item::Type(item_type) = item {
        alias_kotlin_tokens(item_type, name_override)
    } else {
        TokenStream::new()
    }
}

/// The `u64`/`usize` width `field` is, or at any depth reaches — a `SiblingType`'s own generic
/// arguments, a `Map`'s key and value, a `Tuple`'s elements — or `None` for a field with no such
/// width anywhere. Kotlin's widest unsigned width is `UInt`; `u64` and `usize` have no counterpart
/// at all, the same refusal the Swift target makes for the same reason.
pub fn kotlin_refused_width(field: &FieldDef) -> Option<&'static str> {
    match &field.field_type {
        FieldDefType::U64 => Some("u64"),
        FieldDefType::Usize => Some("usize"),
        FieldDefType::SiblingType(_, generics) => generics.iter().find_map(kotlin_refused_width),
        FieldDefType::Map(key, value) => {
            kotlin_refused_width(key).or_else(|| kotlin_refused_width(value))
        }
        FieldDefType::Tuple(elements) => elements.iter().find_map(kotlin_refused_width),
        FieldDefType::TypeParam(_)
        | FieldDefType::Unknown
        | FieldDefType::StringLiteral(_)
        | FieldDefType::BooleanLiteral(_)
        | FieldDefType::NumberLiteral(_)
        | FieldDefType::Boolean
        | FieldDefType::Char
        | FieldDefType::String
        | FieldDefType::U8
        | FieldDefType::U16
        | FieldDefType::U32
        | FieldDefType::I8
        | FieldDefType::I16
        | FieldDefType::I32
        | FieldDefType::I64
        | FieldDefType::Isize
        | FieldDefType::F32
        | FieldDefType::F64 => None,
        #[cfg(feature = "object_id")]
        FieldDefType::ObjectId => None,
        #[cfg(feature = "chrono")]
        FieldDefType::NaiveDate
        | FieldDefType::NaiveTime
        | FieldDefType::NaiveDateTime
        | FieldDefType::DateTime => None,
    }
}

/// Registers the Kotlin name `rust_ident` publishes under, for a later sibling reference to read
/// back.
fn register_kotlin_name(rust_ident: &str, export_name: &str) {
    KOTLIN_NAMES.with(|names| {
        names
            .borrow_mut()
            .insert(rust_ident.to_owned(), export_name.to_owned());
    });
}

/// The Kotlin name registered for `rust_ident`, or `None` for a type declared below the one
/// asking — which falls back to its own Rust ident, harmless since a renamed item always
/// re-publishes that ident too, as a `typealias`.
fn lookup_kotlin_name(rust_ident: &str) -> Option<String> {
    KOTLIN_NAMES.with(|names| names.borrow().get(rust_ident).cloned())
}

/// Whether `attrs` carries a bare `#[serde(transparent)]` — the same test `model_schema.rs` and
/// `features::dart` use to tell a branded newtype from an ordinary tuple struct, duplicated here
/// for the same reason `features::dart` duplicates it rather than reaching for a `pub(crate)`
/// widening: a dozen lines of plain `syn` parsing with no feature dependency of its own.
fn has_serde_transparent(attrs: &[syn::Attribute]) -> bool {
    for attr in attrs {
        if attr.path().is_ident("serde") {
            let mut found = false;
            let _: syn::Result<()> = attr.parse_nested_meta(|nested| {
                if nested.path.is_ident("transparent") {
                    found = true;
                }
                Ok(())
            });
            if found {
                return true;
            }
        }
    }
    false
}

/// Whether `fields` is a tuple shape (unnamed) with exactly one slot — the shape a branded newtype
/// and a bare-value (non-branded) newtype struct share on the wire, both writing the slot's value
/// alone.
fn is_single_slot(fields: &Fields) -> bool {
    matches!(fields, Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1)
}

/// The `FieldDef` of a single-slot tuple shape's one field, its type parameters already erased.
fn single_slot_field(fields: &Fields, type_parameters: &[String]) -> FieldDef {
    let Fields::Unnamed(unnamed) = fields else {
        return get_field_def("value", &syn::parse_quote!(()), "");
    };
    let Some(slot) = unnamed.unnamed.first() else {
        return get_field_def("value", &syn::parse_quote!(()), "");
    };
    let mut field_def = field_def_with_prop_meta("value", &slot.ty, &slot.attrs);
    field_def.erase_type_parameters(type_parameters);
    field_def
}

/// One field's `#[model_schema_prop(...)]` metadata, folded into the `FieldDef` `get_field_def`
/// built for it — `get_field_def` reads the Rust type alone, so a field carrying `nullable` or
/// `as_number` needs this filled in separately, exactly as `process_field` does for the other three
/// surfaces.
fn field_def_with_prop_meta(name: &str, ty: &syn::Type, attrs: &[syn::Attribute]) -> FieldDef {
    let mut field_def = get_field_def(name, ty, "");
    field_def.model_schema_prop_meta = Some(parse_model_schema_prop_attributes(attrs));
    field_def
}

/// The `#[serde(rename = "...")]` a field or variant earns, honored only where the `serde` feature
/// reads serde attributes at all.
#[cfg(feature = "serde")]
fn rename_override(attrs: &[syn::Attribute]) -> Option<String> {
    parse_serde_field_attributes(attrs).rename
}

#[cfg(not(feature = "serde"))]
const fn rename_override(_attrs: &[syn::Attribute]) -> Option<String> {
    None
}

/// The wire name a field with Rust name `rust_name` and its own `rename` writes under, once
/// `rule` — the container's own `rename_all`, [`RenameRule::None`] without the `serde` feature —
/// has had its say. An explicit rename always wins over the container's rule, matching serde.
fn wire_field_name(rust_name: &str, rename: Option<&str>, rule: RenameRule) -> String {
    rename.map_or_else(|| rule.apply_to_field(rust_name), ToOwned::to_owned)
}

/// A container's own `rename_all`, or [`RenameRule::None`] without the `serde` feature to read it
/// with — shared by a struct and an enum, whose variant names read the very same attribute.
#[cfg(feature = "serde")]
fn container_rename_rule(attrs: &[syn::Attribute]) -> RenameRule {
    let meta = parse_serde_type_attributes(attrs);
    resolve_rename_rule(meta.rename_all.as_deref())
}

#[cfg(not(feature = "serde"))]
fn container_rename_rule(_attrs: &[syn::Attribute]) -> RenameRule {
    resolve_rename_rule(None)
}

/// The container attributes an enum's own dispatch reads, or every field left at its default
/// without the `serde` feature to read one with.
#[cfg(feature = "serde")]
fn enum_tag_attrs(attrs: &[syn::Attribute]) -> EnumTagAttrs {
    let meta = parse_serde_type_attributes(attrs);
    EnumTagAttrs {
        content: meta.content,
        rename_all: meta.rename_all,
        rename_all_fields: meta.rename_all_fields,
        tag: meta.tag,
        untagged: meta.untagged,
    }
}

#[cfg(not(feature = "serde"))]
const fn enum_tag_attrs(_attrs: &[syn::Attribute]) -> EnumTagAttrs {
    EnumTagAttrs {
        content: None,
        rename_all: None,
        rename_all_fields: None,
        tag: None,
        untagged: false,
    }
}

/// `rust_name` cased the way a Kotlin property is: `conversation_id` -> `conversationId`. Reuses
/// serde's own `camelCase` rule (`rename_rule.rs`) rather than a second implementation, since the
/// two rules coincide exactly on a `snake_case` Rust identifier.
fn kotlin_property_name(rust_name: &str) -> String {
    RenameRule::CamelCase.apply_to_field(rust_name)
}

/// Whether `field` carries `#[model_schema_prop(nullable)]` — the flag that keeps an `Option<T>`
/// property's key always written, so it earns no `= null` default.
fn is_nullable_flag(field: &FieldDef) -> bool {
    field
        .model_schema_prop_meta
        .as_ref()
        .is_some_and(|meta| meta.nullable)
}

/// Whether `field` carries `#[model_schema_prop(as_number)]`.
#[cfg(feature = "chrono")]
fn has_as_number(field: &FieldDef) -> bool {
    field
        .model_schema_prop_meta
        .as_ref()
        .is_some_and(|meta| meta.as_number)
}

/// Walks a named-field struct's or a struct-shaped enum variant's fields into [`KotlinField`]s,
/// dropping any field a serde attribute takes off the wire in both directions. A field a serde
/// attribute drops from serialization only (`skip_serializing_if`, on a field that is not itself
/// `Option<T>`) is pushed a nullable level so it renders `T? = null` — kotlinx has no separate
/// "absent key" spelling from "explicit null", so the two collapse the same way Dart's own
/// `Map<String, dynamic>` makes them collapse.
fn collect_kotlin_fields(
    fields: &Fields,
    rule: RenameRule,
    type_parameters: &[String],
) -> Vec<KotlinField> {
    let Fields::Named(named) = fields else {
        return Vec::new();
    };
    let mut collected = Vec::new();
    for field in &named.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        let rust_name = ident.to_string();
        let omission = parse_serde_key_omission(&field.attrs);
        if omission.absent_from_wire() {
            continue;
        }
        let wire_name = wire_field_name(&rust_name, rename_override(&field.attrs).as_deref(), rule);
        let mut field_def = field_def_with_prop_meta(&rust_name, &field.ty, &field.attrs);
        field_def.erase_type_parameters(type_parameters);
        if omission.omits_key && !field_def.is_optional() {
            field_def.nullable_levels.push(field_def.array_depth);
        }
        collected.push(KotlinField {
            field_def,
            rust_name,
            wire_name,
        });
    }
    collected
}

/// The Kotlin type before the outer `?` an [`FieldDef::is_optional`] field carries: the scalar
/// match, then one `List<…>` per array level, an inner level written `?` where
/// [`FieldDef::is_nullable_at`] says so. Mirrors `dart_base`.
fn kotlin_base(field: &FieldDef) -> String {
    let scalar = match &field.field_type {
        FieldDefType::Unknown => "JsonElement".to_owned(),
        FieldDefType::TypeParam(name) => name.clone(),
        FieldDefType::Tuple(elements) => tuple_wrapper_typename(elements),
        FieldDefType::SiblingType(name, generics) => {
            if let [element] = generics.as_slice()
                && is_sequence_wrapper(name)
            {
                return kotlin_base(&field.collection_element_field(element));
            }
            let class_name = lookup_kotlin_name(name).unwrap_or_else(|| name.clone());
            if generics.is_empty() {
                class_name
            } else {
                let arguments = generics
                    .iter()
                    .map(kotlin_typename)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{class_name}<{arguments}>")
            }
        }
        FieldDefType::Map(key, value) => {
            format!("Map<{}, {}>", kotlin_typename(key), kotlin_typename(value))
        }
        FieldDefType::Boolean => "Boolean".to_owned(),
        FieldDefType::Char | FieldDefType::String | FieldDefType::StringLiteral(_) => {
            "String".to_owned()
        }
        FieldDefType::BooleanLiteral(_) => "Boolean".to_owned(),
        FieldDefType::NumberLiteral(_) => "Double".to_owned(),
        FieldDefType::U8 => "UByte".to_owned(),
        FieldDefType::U16 => "UShort".to_owned(),
        FieldDefType::U32 => "UInt".to_owned(),
        // Refused under `check_kotlin_width_field` in `model_schema.rs`; a filler rendering keeps
        // this dispatch total for the compile_error! tokens emitted alongside it to stand on.
        FieldDefType::U64 | FieldDefType::Usize => "Long".to_owned(),
        FieldDefType::I8 => "Byte".to_owned(),
        FieldDefType::I16 => "Short".to_owned(),
        FieldDefType::I32 => "Int".to_owned(),
        FieldDefType::I64 | FieldDefType::Isize => "Long".to_owned(),
        FieldDefType::F32 => "Float".to_owned(),
        FieldDefType::F64 => "Double".to_owned(),
        #[cfg(feature = "object_id")]
        FieldDefType::ObjectId => "ObjectId".to_owned(),
        #[cfg(feature = "chrono")]
        FieldDefType::NaiveDate | FieldDefType::NaiveTime | FieldDefType::NaiveDateTime => {
            "String".to_owned()
        }
        #[cfg(feature = "chrono")]
        FieldDefType::DateTime => {
            if has_as_number(field) {
                "Long".to_owned()
            } else {
                "String".to_owned()
            }
        }
    };
    (0..field.array_depth).fold(scalar, |wrapped, level| {
        let item = if field.is_nullable_at(level) {
            format!("{wrapped}?")
        } else {
            wrapped
        };
        format!("List<{item}>")
    })
}

/// The Kotlin type `field` renders as: [`kotlin_base`] plus the `?` an [`FieldDef::is_optional`]
/// field carries — one nullable spelling for both a bare `Option<T>` and
/// `#[model_schema_prop(nullable)]`, the two differing only in whether the property also earns a
/// `= null` default (see [`property_declaration`]).
pub fn kotlin_typename(field: &FieldDef) -> String {
    let base = kotlin_base(field);
    if field.is_optional() {
        format!("{base}?")
    } else {
        base
    }
}

/// A reference to `field`'s own `KSerializer`, built from the same type text [`kotlin_typename`]
/// renders — `kotlinx.serialization`'s reified top-level `serializer<T>()` resolves any nameable
/// type this way, including a `List<…>`/`Map<…, …>` composition and a nullable type, so this
/// module needs no per-shape serializer dispatch of its own.
fn kotlin_serializer_expr(field: &FieldDef) -> String {
    format!("serializer<{}>()", kotlin_typename(field))
}

/// The Kotlin type name for a Tuple-shaped field: queues a wrapper `data class` plus the
/// `KSerializer` that reads and writes it as a JSON array — the shape `kotlinx.serialization`
/// carries no annotation for (a `Pair`/`Triple` writes `{"first":…,"second":…}`, the wrong shape) —
/// and returns the wrapper's own name. Monotonically numbered rather than derived from the
/// enclosing field's name, so two fields named alike in two different items never collide once
/// every item's text is concatenated into one file.
fn tuple_wrapper_typename(elements: &[FieldDef]) -> String {
    let index = KOTLIN_TUPLE_COUNTER.with(|counter| {
        let next = counter.get() + 1;
        counter.set(next);
        next
    });
    let wrapper_name = format!("KotlinTuple{index}");
    let serializer_name = format!("{wrapper_name}Serializer");
    let params = elements
        .iter()
        .enumerate()
        .map(|(slot, element)| format!("val slot{slot}: {}", kotlin_typename(element)))
        .collect::<Vec<_>>()
        .join(", ");
    let encode_entries = elements
        .iter()
        .enumerate()
        .map(|(slot, element)| {
            format!(
                "add(output.json.encodeToJsonElement({}, value.slot{slot}))",
                kotlin_serializer_expr(element)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let decode_entries = elements
        .iter()
        .enumerate()
        .map(|(slot, element)| {
            format!(
                "input.json.decodeFromJsonElement({}, array[{slot}])",
                kotlin_serializer_expr(element)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let class_text = format!(
        "@Serializable(with = {serializer_name}::class) data class {wrapper_name}({params}) \
         object {serializer_name} : KSerializer<{wrapper_name}> {{ \
         override val descriptor: SerialDescriptor = buildClassSerialDescriptor(\"{wrapper_name}\"); \
         override fun serialize(encoder: Encoder, value: {wrapper_name}) {{ \
         val output = encoder as JsonEncoder; \
         output.encodeJsonElement(buildJsonArray {{ {encode_entries} }}) }}; \
         override fun deserialize(decoder: Decoder): {wrapper_name} {{ \
         val input = decoder as JsonDecoder; val array = input.decodeJsonElement().jsonArray; \
         return {wrapper_name}({decode_entries}) }} }}"
    );
    KOTLIN_TUPLE_AUX.with(|aux| aux.borrow_mut().push(class_text));
    wrapper_name
}

/// The `{ident}_kotlin` module ident an item's own `kotlin_definition()` publishes from.
fn kotlin_module_ident(rust_ident: &str, span: proc_macro2::Span) -> Ident {
    Ident::new(&format!("{}_kotlin", to_snake_case(rust_ident)), span)
}

/// The module `{ident}_kotlin` publishes `kotlin_definition()` from — never a direct inherent
/// `impl {ident}`, for the same reason `features::dart`'s `dart_module_tokens` gives: a Rust type
/// alias resolves to its target under the orphan/coherence rules, so a module name is the one
/// spelling every shape can publish under safely. The one choke point every dispatch path in this
/// module returns through, so it also drains whatever [`KOTLIN_TUPLE_AUX`] the item's own fields
/// queued and prepends it ahead of the item's own declaration.
fn kotlin_module_tokens(
    rust_ident: &str,
    span: proc_macro2::Span,
    kotlin_source: &str,
) -> TokenStream {
    let aux = KOTLIN_TUPLE_AUX.with(RefCell::take);
    let full_source = if aux.is_empty() {
        kotlin_source.to_owned()
    } else {
        format!("{} {kotlin_source}", aux.join(" "))
    };
    let module_ident = kotlin_module_ident(rust_ident, span);
    quote! {
        pub mod #module_ident {
            pub fn kotlin_definition() -> String {
                #full_source.to_owned()
            }
        }
    }
}

/// The `typealias {rust_ident}{generic_params} = {export_name}{generic_params};` a renamed item
/// re-publishes under its own Rust ident — the alias a reference declared above the rename still
/// resolves through. Empty for an item that already publishes under its own ident.
fn ident_typealias(rust_ident: &str, export_name: &str, generic_params: &str) -> String {
    if rust_ident == export_name {
        String::new()
    } else {
        format!(" typealias {rust_ident}{generic_params} = {export_name}{generic_params};")
    }
}

/// `<T, U>` for the type parameters `generics` declares, or the empty string for none.
fn kotlin_generic_params(generics: &syn::Generics) -> String {
    let parameters = type_parameters_in_scope(generics);
    if parameters.is_empty() {
        String::new()
    } else {
        format!("<{}>", parameters.join(", "))
    }
}

/// One property declaration inside a constructor's parameter list: `@SerialName("...")` only where
/// the wire spelling differs from the Kotlin property, then `val name: Type`, then `= null` for a
/// field whose key may be absent — a bare `Option<T>` without `#[model_schema_prop(nullable)]`.
fn property_declaration(field: &KotlinField) -> String {
    let prop_name = kotlin_property_name(&field.rust_name);
    let annotation = if prop_name == field.wire_name {
        String::new()
    } else {
        format!("@SerialName(\"{}\") ", field.wire_name)
    };
    let ty = kotlin_typename(&field.field_def);
    let default = if field.field_def.is_optional() && !is_nullable_flag(&field.field_def) {
        " = null"
    } else {
        ""
    };
    format!("{annotation}val {prop_name}: {ty}{default}")
}

/// The constructor parameter list a set of [`KotlinField`]s earns, joined by `, `.
fn data_class_params(fields: &[KotlinField]) -> String {
    fields
        .iter()
        .map(property_declaration)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The Kotlin tokens a named-field struct earns: a `data class`, plus a `typealias` under its own
/// Rust ident when `name = "..."` moved its published name elsewhere.
fn struct_kotlin_tokens(item_struct: &ItemStruct, name_override: Option<&str>) -> TokenStream {
    let type_parameters = type_parameters_in_scope(&item_struct.generics);
    if has_serde_transparent(&item_struct.attrs) && is_single_slot(&item_struct.fields) {
        let value_field = single_slot_field(&item_struct.fields, &type_parameters);
        return value_class_tokens(
            &item_struct.ident,
            &item_struct.generics,
            name_override,
            &value_field,
        );
    }
    if matches!(item_struct.fields, Fields::Unnamed(_)) {
        return tuple_struct_kotlin_tokens(item_struct, name_override);
    }

    let rust_ident = item_struct.ident.to_string();
    let export_name = compute_item_export_name(&rust_ident, name_override);
    register_kotlin_name(&rust_ident, &export_name);

    let rule = container_rename_rule(&item_struct.attrs);
    let fields = collect_kotlin_fields(&item_struct.fields, rule, &type_parameters);
    let generic_params = kotlin_generic_params(&item_struct.generics);
    let alias = ident_typealias(&rust_ident, &export_name, &generic_params);
    let body = if fields.is_empty() {
        format!("class {export_name}{generic_params}")
    } else {
        format!(
            "data class {export_name}{generic_params}({})",
            data_class_params(&fields)
        )
    };
    let kotlin_source = format!("@Serializable {body}{alias}");

    kotlin_module_tokens(&rust_ident, item_struct.ident.span(), &kotlin_source)
}

/// The Kotlin tokens for a value that carries no shape of its own on the wire beyond one wrapped
/// value: a branded newtype or a non-branded single-slot ("bare value") tuple struct. Both publish
/// as a `@JvmInline value class`, which `kotlinx.serialization` serializes identically to the
/// wrapped value alone — the bare wire form serde itself writes for `#[serde(transparent)]`.
fn value_class_tokens(
    ident: &Ident,
    generics: &syn::Generics,
    name_override: Option<&str>,
    value_field: &FieldDef,
) -> TokenStream {
    let rust_ident = ident.to_string();
    let export_name = compute_item_export_name(&rust_ident, name_override);
    register_kotlin_name(&rust_ident, &export_name);

    let generic_params = kotlin_generic_params(generics);
    let value_type = kotlin_typename(value_field);
    let alias = ident_typealias(&rust_ident, &export_name, &generic_params);
    let kotlin_source = format!(
        "@JvmInline @Serializable value class {export_name}{generic_params}(val value: {value_type}){alias}"
    );

    kotlin_module_tokens(&rust_ident, ident.span(), &kotlin_source)
}

/// The Kotlin tokens for a non-branded, multi-slot tuple struct: a `data class` over one property
/// per slot, with the same generated array `KSerializer` a Tuple-shaped field earns — the struct's
/// own name stands in for what [`tuple_wrapper_typename`] would otherwise synthesize.
fn tuple_struct_kotlin_tokens(
    item_struct: &ItemStruct,
    name_override: Option<&str>,
) -> TokenStream {
    let type_parameters = type_parameters_in_scope(&item_struct.generics);
    let Fields::Unnamed(unnamed) = &item_struct.fields else {
        return TokenStream::new();
    };
    let rust_ident = item_struct.ident.to_string();
    let export_name = compute_item_export_name(&rust_ident, name_override);
    register_kotlin_name(&rust_ident, &export_name);

    let slots: Vec<FieldDef> = unnamed
        .unnamed
        .iter()
        .filter(|slot| !parse_serde_key_omission(&slot.attrs).absent_from_wire())
        .map(|slot| {
            let mut field_def = field_def_with_prop_meta("slot", &slot.ty, &slot.attrs);
            field_def.erase_type_parameters(&type_parameters);
            field_def
        })
        .collect();

    let generic_params = kotlin_generic_params(&item_struct.generics);
    let serializer_name = format!("{export_name}Serializer");
    let params = slots
        .iter()
        .enumerate()
        .map(|(slot, element)| format!("val slot{slot}: {}", kotlin_typename(element)))
        .collect::<Vec<_>>()
        .join(", ");
    let encode_entries = slots
        .iter()
        .enumerate()
        .map(|(slot, element)| {
            format!(
                "add(output.json.encodeToJsonElement({}, value.slot{slot}))",
                kotlin_serializer_expr(element)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let decode_entries = slots
        .iter()
        .enumerate()
        .map(|(slot, element)| {
            format!(
                "input.json.decodeFromJsonElement({}, array[{slot}])",
                kotlin_serializer_expr(element)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let alias = ident_typealias(&rust_ident, &export_name, &generic_params);
    let kotlin_source = format!(
        "@Serializable(with = {serializer_name}::class) \
         data class {export_name}{generic_params}({params}) \
         object {serializer_name} : KSerializer<{export_name}{generic_params}> {{ \
         override val descriptor: SerialDescriptor = buildClassSerialDescriptor(\"{export_name}\"); \
         override fun serialize(encoder: Encoder, value: {export_name}{generic_params}) {{ \
         val output = encoder as JsonEncoder; \
         output.encodeJsonElement(buildJsonArray {{ {encode_entries} }}) }}; \
         override fun deserialize(decoder: Decoder): {export_name}{generic_params} {{ \
         val input = decoder as JsonDecoder; val array = input.decodeJsonElement().jsonArray; \
         return {export_name}({decode_entries}) }} }}{alias}"
    );

    kotlin_module_tokens(&rust_ident, item_struct.ident.span(), &kotlin_source)
}

/// The Kotlin tokens a `type X = ...;` alias earns: a `typealias` pointing at the target's own
/// rendering — never a wrapper of its own, unlike Dart (which has no bare structural alias to
/// point at): Kotlin's `typealias` is exactly that spelling.
fn alias_kotlin_tokens(item_type: &ItemType, name_override: Option<&str>) -> TokenStream {
    let rust_ident = item_type.ident.to_string();
    let export_name = compute_alias_export_name(&rust_ident, name_override);
    register_kotlin_name(&rust_ident, &export_name);

    let type_parameters = type_parameters_in_scope(&item_type.generics);
    let mut target = get_field_def(&export_name, &item_type.ty, "");
    target.erase_type_parameters(&type_parameters);
    let generic_params = kotlin_generic_params(&item_type.generics);
    let target_type = kotlin_typename(&target);
    let alias = ident_typealias(&rust_ident, &export_name, &generic_params);
    let kotlin_source = format!("typealias {export_name}{generic_params} = {target_type};{alias}");

    kotlin_module_tokens(&rust_ident, item_type.ident.span(), &kotlin_source)
}

/// The Kotlin tokens an enum earns: an `enum class` for an all-unit, untagged-by-default
/// declaration; a `sealed interface` for every tagged or untagged shape otherwise — mirroring
/// exactly the shape `process_enum` itself dispatches a declaration to.
fn enum_kotlin_tokens(item_enum: &ItemEnum, name_override: Option<&str>) -> TokenStream {
    let rust_ident = item_enum.ident.to_string();
    let export_name = compute_item_export_name(&rust_ident, name_override);
    register_kotlin_name(&rust_ident, &export_name);
    let tag_attrs = enum_tag_attrs(&item_enum.attrs);
    let writes_bare_variant_names =
        tag_attrs.tag.is_none() && tag_attrs.content.is_none() && !tag_attrs.untagged;
    let kotlin_source = if is_plain_enum(item_enum) && writes_bare_variant_names {
        plain_enum_kotlin_source(item_enum, &export_name)
    } else if tag_attrs.untagged {
        untagged_enum_kotlin_source(item_enum, &export_name, &tag_attrs)
    } else if tag_attrs.tag.is_some() && tag_attrs.content.is_none() {
        internal_tagged_enum_kotlin_source(item_enum, &export_name, &tag_attrs)
    } else {
        dispatched_tagged_enum_kotlin_source(item_enum, &export_name, &tag_attrs)
    };
    kotlin_module_tokens(&rust_ident, item_enum.ident.span(), &kotlin_source)
}

/// One tagged/untagged variant's payload — see [`VariantPayload`]. A `TupleMultiple` payload is
/// folded into one `Tuple` [`FieldDef`], matching how a slot list renders everywhere else in this
/// module.
fn variant_payload(
    variant: &Variant,
    field_rule: RenameRule,
    type_parameters: &[String],
) -> VariantPayload {
    match classify_variant(variant) {
        VariantKind::Unit => VariantPayload::Unit,
        VariantKind::Named => VariantPayload::Named(collect_kotlin_fields(
            &variant.fields,
            field_rule,
            type_parameters,
        )),
        VariantKind::TupleSingle => {
            let slot = variant.fields.iter().next();
            VariantPayload::Value(Box::new(slot.map_or_else(
                || get_field_def("value", &syn::parse_quote!(()), ""),
                |field| {
                    let mut field_def = field_def_with_prop_meta("value", &field.ty, &field.attrs);
                    field_def.erase_type_parameters(type_parameters);
                    field_def
                },
            )))
        }
        VariantKind::TupleMultiple => {
            let slots: Vec<FieldDef> = variant
                .fields
                .iter()
                .map(|field| {
                    let mut field_def = field_def_with_prop_meta("slot", &field.ty, &field.attrs);
                    field_def.erase_type_parameters(type_parameters);
                    field_def
                })
                .collect();
            VariantPayload::Value(Box::new(FieldDef {
                absent_from_wire: false,
                array_depth: 0,
                array_lengths: Vec::new(),
                docs: String::new(),
                field_type: FieldDefType::Tuple(slots),
                model_schema_prop_meta: None,
                name: "value".to_owned(),
                nullable_levels: Vec::new(),
                omits_value: false,
                #[cfg(feature = "jsonschema")]
                type_span: proc_macro2::Span::call_site(),
            }))
        }
    }
}

/// One variant's own subclass declaration, implementing `export_name`, plus the plain reference to
/// its own type (generics included) other builders name it by. `bare_value` selects a
/// `@JvmInline value class` for a scalar `Value` payload — the bare wire form an untagged member
/// needs, since its own default serializer is what the base's generated dispatcher calls directly
/// — while a tagged form (whose own dispatcher unwraps the payload itself, or whose polymorphic
/// dispatch merges the payload's own fields) wraps it in an ordinary `data class` property instead.
fn variant_subclass(
    export_name: &str,
    subclass_name: &str,
    generic_params: &str,
    serial_name: Option<&str>,
    bare_value: bool,
    payload: &VariantPayload,
) -> String {
    let annotation =
        serial_name.map_or_else(String::new, |wire| format!("@SerialName(\"{wire}\") "));
    match payload {
        VariantPayload::Unit => {
            format!(
                "{annotation}@Serializable data object {subclass_name} : {export_name}{generic_params}"
            )
        }
        VariantPayload::Named(fields) => {
            format!(
                "{annotation}@Serializable data class {subclass_name}{generic_params}({}) : {export_name}{generic_params}",
                data_class_params(fields)
            )
        }
        VariantPayload::Value(field_def) if bare_value => {
            format!(
                "{annotation}@JvmInline @Serializable value class {subclass_name}(val value: {}) : {export_name}{generic_params}",
                kotlin_typename(field_def)
            )
        }
        VariantPayload::Value(field_def) => {
            format!(
                "{annotation}@Serializable data class {subclass_name}(val value: {}) : {export_name}{generic_params}",
                kotlin_typename(field_def)
            )
        }
    }
}

/// The Kotlin tokens an internally-tagged enum (`tag = "..."`, no `content`) earns: a
/// `@JsonClassDiscriminator`-annotated `sealed interface`, one subclass per variant, each always
/// carrying `@SerialName` — the subclass name is compound (`{Base}{Variant}`) and never coincides
/// with the wire tag, so the annotation is what makes it the discriminator value the plugin's own
/// sealed-hierarchy dispatch reads.
fn internal_tagged_enum_kotlin_source(
    item_enum: &ItemEnum,
    export_name: &str,
    tag_attrs: &EnumTagAttrs,
) -> String {
    let tag_key = tag_attrs.tag.as_deref().unwrap_or("type");
    let variant_rule = resolve_rename_rule(tag_attrs.rename_all.as_deref());
    let field_rule = resolve_rename_rule(tag_attrs.rename_all_fields.as_deref());
    let type_parameters = type_parameters_in_scope(&item_enum.generics);
    let generic_params = kotlin_generic_params(&item_enum.generics);

    let subclasses: Vec<String> = item_enum
        .variants
        .iter()
        .map(|variant| {
            let variant_rust_name = variant.ident.to_string();
            let wire_tag = rename_override(&variant.attrs)
                .unwrap_or_else(|| variant_rule.apply_to_variant(&variant_rust_name));
            let subclass_name = format!("{export_name}{variant_rust_name}");
            let payload = variant_payload(variant, field_rule, &type_parameters);
            variant_subclass(
                export_name,
                &subclass_name,
                &generic_params,
                Some(&wire_tag),
                false,
                &payload,
            )
        })
        .collect();

    let base = format!(
        "@OptIn(ExperimentalSerializationApi::class) @Serializable @JsonClassDiscriminator(\"{tag_key}\") \
         sealed interface {export_name}{generic_params}"
    );
    format!("{base} {}", subclasses.join(" "))
}

/// The Kotlin tokens an adjacently-tagged (`tag = "...", content = "..."`) or externally-tagged
/// (serde's own default) enum earns: a plain `sealed interface`, one subclass per variant with no
/// annotation the dispatcher needs, and a generated `KSerializer` that reads and writes the tag and
/// content object by hand — the shape the codec spike proved needs one.
fn dispatched_tagged_enum_kotlin_source(
    item_enum: &ItemEnum,
    export_name: &str,
    tag_attrs: &EnumTagAttrs,
) -> String {
    let variant_rule = resolve_rename_rule(tag_attrs.rename_all.as_deref());
    let field_rule = resolve_rename_rule(tag_attrs.rename_all_fields.as_deref());
    let type_parameters = type_parameters_in_scope(&item_enum.generics);
    let generic_params = kotlin_generic_params(&item_enum.generics);
    let content_key = tag_attrs.content.as_deref();

    let mut subclasses = Vec::new();
    let mut serialize_arms = Vec::new();
    let mut deserialize_arms = Vec::new();
    for variant in &item_enum.variants {
        let variant_rust_name = variant.ident.to_string();
        let wire_tag = rename_override(&variant.attrs)
            .unwrap_or_else(|| variant_rule.apply_to_variant(&variant_rust_name));
        let subclass_name = format!("{export_name}{variant_rust_name}");
        let payload = variant_payload(variant, field_rule, &type_parameters);
        subclasses.push(variant_subclass(
            export_name,
            &subclass_name,
            &generic_params,
            None,
            false,
            &payload,
        ));

        let content_expr = match &payload {
            VariantPayload::Unit => None,
            VariantPayload::Named(_) => Some(format!(
                "output.json.encodeToJsonElement(serializer<{subclass_name}>(), value)"
            )),
            VariantPayload::Value(field_def) => Some(format!(
                "output.json.encodeToJsonElement({}, value.value)",
                kotlin_serializer_expr(field_def)
            )),
        };
        // Adjacent tagging reads its content out of a `Map` index, which is nullable in Kotlin;
        // external tagging destructures the object's one entry, already non-null. Only a Unit
        // payload's own arm never forces it, since a Unit variant carries no content key at all.
        let data_ref = if content_key.is_some() {
            "data!!"
        } else {
            "data"
        };
        let decode_expr = match &payload {
            VariantPayload::Unit => subclass_name.clone(),
            VariantPayload::Named(_) => {
                format!(
                    "input.json.decodeFromJsonElement(serializer<{subclass_name}>(), {data_ref})"
                )
            }
            VariantPayload::Value(field_def) => format!(
                "{subclass_name}(input.json.decodeFromJsonElement({}, {data_ref}))",
                kotlin_serializer_expr(field_def)
            ),
        };

        serialize_arms.push(match (&content_key, &content_expr) {
            (Some(content), Some(expr)) => format!(
                "is {subclass_name} -> buildJsonObject {{ put(\"{}\", \"{wire_tag}\"); put(\"{content}\", {expr}) }}",
                tag_attrs.tag.as_deref().unwrap_or("type"),
            ),
            (None, Some(expr)) => {
                format!("is {subclass_name} -> buildJsonObject {{ put(\"{wire_tag}\", {expr}) }}")
            }
            (Some(_), None) => format!(
                "is {subclass_name} -> buildJsonObject {{ put(\"{}\", \"{wire_tag}\") }}",
                tag_attrs.tag.as_deref().unwrap_or("type"),
            ),
            (None, None) => format!("is {subclass_name} -> JsonPrimitive(\"{wire_tag}\")"),
        });

        deserialize_arms.push(format!("\"{wire_tag}\" -> {decode_expr}"));
    }

    let deserialize_body = content_key.map_or_else(
        || {
            format!(
                "val obj = input.decodeJsonElement().jsonObject; \
                 val (tag, data) = obj.entries.single(); \
                 return when (tag) {{ {} else -> error(\"unknown tag \" + tag) }}",
                deserialize_arms.join("; "),
            )
        },
        |content| {
            format!(
                "val obj = input.decodeJsonElement().jsonObject; \
                 val tag = obj.getValue(\"{tag_key}\").jsonPrimitive.content; \
                 val data = obj[\"{content}\"]; \
                 return when (tag) {{ {} else -> error(\"unknown tag \" + tag) }}",
                deserialize_arms.join("; "),
                tag_key = tag_attrs.tag.as_deref().unwrap_or("type"),
            )
        },
    );

    let serializer_name = format!("{export_name}Serializer");
    let base = format!("sealed interface {export_name}{generic_params}");
    let serializer_object = format!(
        "object {serializer_name} : KSerializer<{export_name}{generic_params}> {{ \
         override val descriptor: SerialDescriptor = buildClassSerialDescriptor(\"{export_name}\"); \
         override fun serialize(encoder: Encoder, value: {export_name}{generic_params}) {{ \
         val output = encoder as JsonEncoder; \
         output.encodeJsonElement(when (value) {{ {} }}) }}; \
         override fun deserialize(decoder: Decoder): {export_name}{generic_params} {{ \
         val input = decoder as JsonDecoder; {deserialize_body} }} }}",
        serialize_arms.join("; "),
    );

    format!("{base} {} {serializer_object}", subclasses.join(" "))
}

/// The Kotlin tokens a `#[serde(untagged)]` enum earns: a plain `sealed interface` and a generated
/// `KSerializer` that tries each variant's own serializer in turn, returning the first that decodes
/// — mirroring Dart's own try-each-variant fallback (Kotlin has no structural-shape combinator
/// general enough for an arbitrary mix of member shapes, only a serializer-selecting one).
fn untagged_enum_kotlin_source(
    item_enum: &ItemEnum,
    export_name: &str,
    tag_attrs: &EnumTagAttrs,
) -> String {
    let field_rule = resolve_rename_rule(tag_attrs.rename_all_fields.as_deref());
    let type_parameters = type_parameters_in_scope(&item_enum.generics);
    let generic_params = kotlin_generic_params(&item_enum.generics);

    let mut subclasses = Vec::new();
    let mut serialize_arms = Vec::new();
    let mut deserialize_chain = String::from("runCatching { error(\"unreachable\") as Nothing }");
    for variant in &item_enum.variants {
        let variant_rust_name = variant.ident.to_string();
        let subclass_name = format!("{export_name}{variant_rust_name}");
        let payload = variant_payload(variant, field_rule, &type_parameters);
        let bare_value = matches!(payload, VariantPayload::Value(_));
        subclasses.push(variant_subclass(
            export_name,
            &subclass_name,
            &generic_params,
            None,
            bare_value,
            &payload,
        ));
        serialize_arms.push(format!(
            "is {subclass_name} -> output.json.encodeToJsonElement(serializer<{subclass_name}>(), value)"
        ));
        deserialize_chain = format!(
            "{deserialize_chain}.recoverCatching {{ input.json.decodeFromJsonElement(serializer<{subclass_name}>(), element) as {export_name}{generic_params} }}"
        );
    }

    let serializer_name = format!("{export_name}Serializer");
    let base = format!("sealed interface {export_name}{generic_params}");
    let serializer_object = format!(
        "object {serializer_name} : KSerializer<{export_name}{generic_params}> {{ \
         override val descriptor: SerialDescriptor = buildClassSerialDescriptor(\"{export_name}\"); \
         override fun serialize(encoder: Encoder, value: {export_name}{generic_params}) {{ \
         val output = encoder as JsonEncoder; \
         output.encodeJsonElement(when (value) {{ {} }}) }}; \
         override fun deserialize(decoder: Decoder): {export_name}{generic_params} {{ \
         val input = decoder as JsonDecoder; val element = input.decodeJsonElement(); \
         return {deserialize_chain}.getOrElse {{ error(\"No variant of {export_name} matched\") }} }} }}",
        serialize_arms.join("; "),
    );

    format!("{base} {} {serializer_object}", subclasses.join(" "))
}

/// One plain-enum variant's Kotlin member name (its Rust ident, verbatim — Kotlin enum constants
/// read naturally in `PascalCase` too) and wire value.
fn plain_enum_member(variant: &Variant, rule: RenameRule) -> (String, String) {
    let rust_name = variant.ident.to_string();
    let wire = rename_override(&variant.attrs).unwrap_or_else(|| rule.apply_to_variant(&rust_name));
    (rust_name, wire)
}

/// The Kotlin tokens a plain (all-unit, string-wire) enum earns: a serializable `enum class`,
/// `@SerialName` only where the wire spelling differs from the Rust ident.
fn plain_enum_kotlin_source(item_enum: &ItemEnum, export_name: &str) -> String {
    let rule = container_rename_rule(&item_enum.attrs);
    let members = item_enum
        .variants
        .iter()
        .map(|variant| {
            let (name, wire) = plain_enum_member(variant, rule);
            if name == wire {
                name
            } else {
                format!("@SerialName(\"{wire}\") {name}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("@Serializable enum class {export_name} {{ {members} }}")
}
