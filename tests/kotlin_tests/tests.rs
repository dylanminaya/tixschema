//! Tests for the Kotlin type and `kotlinx.serialization` codec backend (`kotlin` feature).
//!
//! Each `#[model_schema]` item earns `kotlin_definition()` inside a `{snake_case}_kotlin` module
//! beside it (never a direct inherent `impl` — see `features::kotlin::kotlin_module_tokens`).

use std::collections::HashMap;

#[cfg(feature = "object_id")]
use mongodb::bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use tixschema::model_schema;

// ---------------------------------------------------------------------------------------------
// Struct: a renamed key and an optional field — the design's own worked example.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRequest {
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,
}

// ---------------------------------------------------------------------------------------------
// The width table.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Widths {
    pub arch_signed: isize,
    pub double: f64,
    pub flag: bool,
    pub items: Vec<i32>,
    pub large_signed: i64,
    pub letter: char,
    pub map: HashMap<String, i32>,
    pub medium_signed: i32,
    pub medium_unsigned: u32,
    pub single: f32,
    pub small_signed: i16,
    pub small_unsigned: u16,
    pub text: String,
    pub tiny_signed: i8,
    pub tiny_unsigned: u8,
}

// ---------------------------------------------------------------------------------------------
// Plain enum.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Inactive,
    Pending,
}

// ---------------------------------------------------------------------------------------------
// Internally tagged enum.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WindowError {
    NotFound,
    RateLimited { retry_after_ms: i64 },
}

// ---------------------------------------------------------------------------------------------
// Adjacently tagged enum (`tag` + `content`).
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum DynamicValue {
    Flag(bool),
    Nothing,
    Number(i64),
}

// ---------------------------------------------------------------------------------------------
// Externally tagged enum (serde's own default).
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalTagged {
    Ping { nonce: i32 },
    Pong { nonce: i32 },
}

// ---------------------------------------------------------------------------------------------
// Untagged enum.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DateValue {
    Epoch(i64),
    Iso(String),
}

// ---------------------------------------------------------------------------------------------
// Tuple field.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldsTuple {
    pub coords: (String, i64),
}

// ---------------------------------------------------------------------------------------------
// Non-string map keys: a numeric key and a plain-enum key.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Slot {
    North,
    South,
}

#[model_schema()]
pub type SlotAliasKey = Slot;

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapKeys {
    pub by_number: HashMap<i32, String>,
    pub by_slot: HashMap<Slot, String>,
}

// ---------------------------------------------------------------------------------------------
// Dates and `as_number`, `ObjectId`.
// ---------------------------------------------------------------------------------------------

#[cfg(feature = "chrono")]
#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dates {
    pub plain_date: chrono::NaiveDate,
    #[model_schema_prop(as_number)]
    pub zoned_ms: chrono::DateTime<chrono::Utc>,
}

#[cfg(feature = "object_id")]
#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HasId {
    pub id: ObjectId,
}

// ---------------------------------------------------------------------------------------------
// Branded newtype and a non-branded bare tuple struct — both `@JvmInline value class`.
// ---------------------------------------------------------------------------------------------

#[model_schema()]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CorrelationId(pub String);

// ---------------------------------------------------------------------------------------------
// The `name` override moving a type's published Kotlin name.
// ---------------------------------------------------------------------------------------------

#[model_schema(name = "RenamedWidget")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Widget {
    pub label: String,
}

// ---------------------------------------------------------------------------------------------
// A generic struct: `kotlinx.serialization` covers this through its own compiler plugin, at the
// bare type parameter — no factory or converter argument for this module to thread through.
// ---------------------------------------------------------------------------------------------

#[model_schema(default_types(T = String))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wrapper<T> {
    pub value: T,
}

#[test]
fn test_every_declared_type_is_constructible() {
    let window_request = WindowRequest {
        conversation_id: "abc".to_owned(),
        limit: Some(10_i32),
    };
    assert_eq!(window_request.limit, Some(10_i32));

    let widths = Widths {
        arch_signed: 0,
        double: 0.0,
        flag: true,
        items: Vec::new(),
        large_signed: 0,
        letter: 'x',
        map: HashMap::new(),
        medium_signed: 0,
        medium_unsigned: 0,
        single: 0.0,
        small_signed: 0,
        small_unsigned: 0,
        text: String::new(),
        tiny_signed: 0,
        tiny_unsigned: 0,
    };
    assert_eq!(widths.text, "");

    let statuses = [Status::Active, Status::Inactive, Status::Pending];
    assert_eq!(statuses.len(), 3);

    let errors = [
        WindowError::NotFound,
        WindowError::RateLimited {
            retry_after_ms: 500,
        },
    ];
    assert_eq!(errors.len(), 2);

    let dynamic_values = [
        DynamicValue::Flag(true),
        DynamicValue::Nothing,
        DynamicValue::Number(7),
    ];
    assert_eq!(dynamic_values.len(), 3);

    let external_tagged = [
        ExternalTagged::Ping { nonce: 1_i32 },
        ExternalTagged::Pong { nonce: 2_i32 },
    ];
    assert_eq!(external_tagged.len(), 2);

    let date_values = [DateValue::Epoch(0), DateValue::Iso(String::new())];
    assert_eq!(date_values.len(), 2);

    let holds_tuple = HoldsTuple {
        coords: ("a".to_owned(), 7),
    };
    assert_eq!(holds_tuple.coords.1, 7);

    let slots = [Slot::North, Slot::South];
    assert_eq!(slots.len(), 2);

    let alias_key: SlotAliasKey = Slot::North;
    assert_eq!(alias_key, Slot::North);

    let map_keys = MapKeys {
        by_number: HashMap::from([(1_i32, "one".to_owned())]),
        by_slot: HashMap::from([(Slot::North, "n".to_owned())]),
    };
    assert_eq!(map_keys.by_number.len(), 1);

    let correlation_id = CorrelationId("abc-123".to_owned());
    assert_eq!(correlation_id.0, "abc-123");

    let widget = Widget {
        label: "a widget".to_owned(),
    };
    assert_eq!(widget.label, "a widget");

    let wrapper = Wrapper {
        value: "wrapped".to_owned(),
    };
    assert_eq!(wrapper.value, "wrapped");
}

#[test]
#[cfg(feature = "chrono")]
fn test_chrono_type_is_constructible() {
    let dates = Dates {
        plain_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 21).unwrap(),
        zoned_ms: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    };
    assert_eq!(dates.plain_date.to_string(), "2026-09-21");
}

#[test]
#[cfg(feature = "object_id")]
fn test_object_id_type_is_constructible() {
    let has_id = HasId {
        id: ObjectId::new(),
    };
    assert_ne!(has_id.id.to_hex(), String::new());
}

#[test]
fn test_struct_rename_and_optional() {
    let kotlin = window_request_kotlin::kotlin_definition();
    assert!(kotlin.contains("@Serializable"), "got: {kotlin}");
    assert!(
        kotlin.contains("data class WindowRequest("),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("@SerialName(\"conversation_id\") val conversationId: String"),
        "got: {kotlin}"
    );
    assert!(kotlin.contains("val limit: Int? = null"), "got: {kotlin}");
    // The wire spelling already matches the Kotlin property, so no annotation is written for it.
    assert!(!kotlin.contains("@SerialName(\"limit\")"), "got: {kotlin}");
}

#[test]
fn test_width_table() {
    let kotlin = widths_kotlin::kotlin_definition();
    for expected in [
        "val flag: Boolean",
        "val tinySigned: Byte",
        "val smallSigned: Short",
        "val mediumSigned: Int",
        "val largeSigned: Long",
        "val archSigned: Long",
        "val tinyUnsigned: UByte",
        "val smallUnsigned: UShort",
        "val mediumUnsigned: UInt",
        "val single: Float",
        "val double: Double",
        "val text: String",
        "val letter: String",
        "val items: List<Int>",
        "val map: Map<String, Int>",
    ] {
        assert!(
            kotlin.contains(expected),
            "missing {expected}, got: {kotlin}"
        );
    }
}

// The four tests below assert wire spellings and dispatch forms that only `#[serde(...)]`
// attributes decide — `rename_all`, `tag`, `content`, `untagged`. Without the `serde` feature none
// of those are read, so every enum here would fall back to the externally-tagged treatment
// regardless of what it declares (the same fallback the other three surfaces make).

#[test]
#[cfg(feature = "serde")]
fn test_plain_enum() {
    let kotlin = status_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("@Serializable enum class Status"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("@SerialName(\"active\") Active"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("@SerialName(\"inactive\") Inactive"),
        "got: {kotlin}"
    );
}

#[test]
#[cfg(feature = "serde")]
fn test_internal_tagged_enum() {
    let kotlin = window_error_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("@JsonClassDiscriminator(\"kind\")"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("sealed interface WindowError"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains(
            "@SerialName(\"NotFound\") @Serializable data object WindowErrorNotFound : WindowError"
        ),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains(
            "@SerialName(\"RateLimited\") @Serializable data class WindowErrorRateLimited(@SerialName(\"retry_after_ms\") val retryAfterMs: Long) : WindowError"
        ),
        "got: {kotlin}"
    );
}

#[test]
#[cfg(feature = "serde")]
fn test_adjacent_tagged_enum() {
    let kotlin = dynamic_value_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("sealed interface DynamicValue"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("object DynamicValueSerializer : KSerializer<DynamicValue>"),
        "got: {kotlin}"
    );
    assert!(kotlin.contains("put(\"type\", \"Flag\")"), "got: {kotlin}");
    assert!(kotlin.contains("put(\"value\","), "got: {kotlin}");
}

#[test]
fn test_external_tagged_enum() {
    let kotlin = external_tagged_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("sealed interface ExternalTagged"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("object ExternalTaggedSerializer : KSerializer<ExternalTagged>"),
        "got: {kotlin}"
    );
    assert!(kotlin.contains("obj.entries.single()"), "got: {kotlin}");
}

#[test]
#[cfg(feature = "serde")]
fn test_untagged_enum() {
    let kotlin = date_value_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("sealed interface DateValue"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("object DateValueSerializer : KSerializer<DateValue>"),
        "got: {kotlin}"
    );
    assert!(kotlin.contains("runCatching {"), "got: {kotlin}");
    assert!(kotlin.contains("recoverCatching {"), "got: {kotlin}");
    assert!(
        kotlin.contains("@JvmInline @Serializable value class DateValueEpoch(val value: Long)"),
        "got: {kotlin}"
    );
}

#[test]
fn test_tuple_field() {
    let kotlin = holds_tuple_kotlin::kotlin_definition();
    assert!(kotlin.contains("data class KotlinTuple"), "got: {kotlin}");
    assert!(kotlin.contains("buildJsonArray {"), "got: {kotlin}");
    assert!(kotlin.contains("val slot0: String"), "got: {kotlin}");
    assert!(kotlin.contains("val slot1: Long"), "got: {kotlin}");
}

#[test]
fn test_non_string_map_keys() {
    let kotlin = map_keys_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("val byNumber: Map<Int, String>"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("val bySlot: Map<Slot, String>"),
        "got: {kotlin}"
    );
}

#[test]
fn test_alias() {
    let kotlin = slot_alias_key_kotlin::kotlin_definition();
    // An alias with no `name` override still moves off its own Rust ident (the `Type` suffix),
    // and re-publishes under that ident so an earlier reference still resolves.
    assert!(
        kotlin.contains("typealias SlotAliasKeyType = Slot;"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("typealias SlotAliasKey = SlotAliasKeyType;"),
        "got: {kotlin}"
    );
}

#[test]
#[cfg(feature = "chrono")]
fn test_dates() {
    let kotlin = dates_kotlin::kotlin_definition();
    assert!(kotlin.contains("val plainDate: String"), "got: {kotlin}");
    assert!(kotlin.contains("val zonedMs: Long"), "got: {kotlin}");
}

#[test]
#[cfg(feature = "object_id")]
fn test_object_id_bare() {
    let kotlin = has_id_kotlin::kotlin_definition();
    assert!(kotlin.contains("val id: ObjectId"), "got: {kotlin}");
}

#[test]
fn test_branded_newtype() {
    let kotlin = correlation_id_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("@JvmInline @Serializable value class CorrelationId(val value: String)"),
        "got: {kotlin}"
    );
}

#[test]
fn test_name_override() {
    let kotlin = widget_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("data class RenamedWidget(val label: String)"),
        "got: {kotlin}"
    );
    assert!(
        kotlin.contains("typealias Widget = RenamedWidget;"),
        "got: {kotlin}"
    );
}

#[test]
fn test_generic_struct() {
    let kotlin = wrapper_kotlin::kotlin_definition();
    assert!(
        kotlin.contains("data class Wrapper<T>(val value: T)"),
        "got: {kotlin}"
    );
}
