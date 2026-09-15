//! Serialization goldens (binding-spec §3): the EmitUnpopulated shape of the
//! response codec, pinned byte-exact. The authoritative cross-backend pin is
//! the differential harness; these lock the Rust side against drift.
//!
//! Shape rules in play (protojson + EmitUnpopulated, as implemented by
//! prost-reflect's `skip_default_fields(false)`):
//! * presence-tracked fields (proto3 `optional`, singular messages, oneof
//!   members) that are unset stay OMITTED — even under EmitUnpopulated;
//! * plain (non-optional) scalars emit their default; 64-bit integers emit as
//!   strings; unset repeated fields emit as [].

use admin_api::proto::dict::service::v1::{Language, ListLanguageResponse};

#[test]
fn zero_value_presence_only_message_omits_everything() {
    // Language's every field is `optional` — unset presence fields are
    // omitted, so the zero value serializes to the empty object.
    let bytes = rushwind_http_binding::codec::serialize_response(
        admin_api::pool(),
        "dict.service.v1.Language",
        &Language::default(),
    )
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes), "{}");
}

#[test]
fn zero_value_plain_fields_emit_defaults() {
    // ListLanguageResponse: `repeated Language items` and plain `uint64
    // total` — unset repeated emits [], the unset 64-bit scalar emits its
    // default AS A STRING per protojson.
    let bytes = rushwind_http_binding::codec::serialize_response(
        admin_api::pool(),
        "dict.service.v1.ListLanguageResponse",
        &ListLanguageResponse::default(),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "{\"items\":[],\"total\":\"0\"}"
    );
}
