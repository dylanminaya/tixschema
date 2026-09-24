//! A declared error carrying a header — `error_header_out("content-range")` on a `GET` whose
//! `RangeNotSatisfiable` variant carries a `content-range`-renamed field, mirroring a `416` answer
//! that must carry `Content-Range: bytes */<size>` (pangeo-ufr's own repro: MinIO already hands the
//! header back, but the dispatcher had nowhere to put it on a declared error). `NotFound` carries no
//! such field, so its own answer proves an operation's declared headers are read per-variant rather
//! than assumed present.

#![cfg(feature = "serde")]

use crate::error_header_http_rest_transport;
use core::future::{Future, ready};
use core::pin::pin;
use core::task::{Context as PollContext, Poll, Waker};
use serde::{Deserialize, Serialize};
use tixschema::{model_schema, service_schema};

#[model_schema()]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RangeDescriptor {
    pub id: String,
}

#[model_schema()]
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "errorCode")]
pub enum RangeError {
    #[serde(rename = "not-found")]
    NotFound,
    #[serde(rename = "range-not-satisfiable")]
    RangeNotSatisfiable {
        #[serde(rename = "content-range")]
        content_range: String,
    },
}

#[service_schema(transports = ["http_rest"])]
pub trait RangeService<Ctx> {
    #[service_schema_op(http(
        method = "GET",
        path = "/ranges/{id}",
        error_status(NotFound = 404, RangeNotSatisfiable = 416),
        error_header_out("content-range"),
    ))]
    async fn get_range(&self, ctx: &Ctx, id: String) -> Result<RangeDescriptor, RangeError>;
}

pub struct RangeBackEnd;

impl RangeService<()> for RangeBackEnd {
    async fn get_range(&self, _ctx: &(), id: String) -> Result<RangeDescriptor, RangeError> {
        ready(()).await;
        match id.as_str() {
            "missing" => Err(RangeError::NotFound),
            "oversized" => Err(RangeError::RangeNotSatisfiable {
                content_range: "bytes */2097152".to_owned(),
            }),
            _ => Ok(RangeDescriptor { id }),
        }
    }
}

/// An owner-installed `FaultHandler`, exercising `OutgoingResponse::new` and its own `headers()`
/// accessor directly.
struct RecordingFaultHandler;

impl error_header_http_rest_transport::FaultHandler for RecordingFaultHandler {
    fn on_fault(
        &self,
        fault: &range_service_schema::ServiceFault,
    ) -> error_header_http_rest_transport::OutgoingResponse {
        error_header_http_rest_transport::OutgoingResponse::new(
            499,
            vec![("x-fault-kind".to_owned(), format!("{}", fault.kind()))],
            format!("handled: {}", fault.detail()).into_bytes(),
        )
    }
}

fn poll_once<Answered>(answering: Answered) -> Option<Answered::Output>
where
    Answered: Future,
{
    let mut pinned = pin!(answering);
    let mut polling = PollContext::from_waker(Waker::noop());
    match pinned.as_mut().poll(&mut polling) {
        Poll::Ready(answer) => Some(answer),
        Poll::Pending => None,
    }
}

fn request(id: &str) -> error_header_http_rest_transport::IncomingRequest {
    error_header_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        format!("/ranges/{id}"),
        String::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// The bug this proves fixed: a declared error used to answer through `json_response` with a
/// hard-coded empty `Vec::new()` for headers, unconditionally - `error_header_out` had no path to
/// reach the response at all.
#[test]
fn a_declared_error_s_own_field_answers_as_the_declared_header() {
    let response = poll_once(error_header_http_rest_transport::dispatch(
        &RangeBackEnd,
        &(),
        &request("oversized"),
        &error_header_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 416, "got: {:?}", response.body());
    assert_eq!(
        response.headers(),
        &[
            ("content-range".to_owned(), "bytes */2097152".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ]
    );
}

/// A variant that carries no field under the declared header's name answers exactly as an
/// undeclared operation always has: no such header at all.
#[test]
fn a_variant_carrying_no_such_field_answers_with_no_header() {
    let response = poll_once(error_header_http_rest_transport::dispatch(
        &RangeBackEnd,
        &(),
        &request("missing"),
        &error_header_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 404, "got: {:?}", response.body());
    assert_eq!(
        response.headers(),
        &[("content-type".to_owned(), "application/json".to_owned())]
    );
}

#[test]
fn a_success_answers_with_no_error_header() {
    let response = poll_once(error_header_http_rest_transport::dispatch(
        &RangeBackEnd,
        &(),
        &request("ok-id"),
        &error_header_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 200, "got: {:?}", response.body());
    assert_eq!(
        response.headers(),
        &[("content-type".to_owned(), "application/json".to_owned())]
    );
}

/// The route table lists the one route, its statuses included.
#[test]
fn the_route_table_lists_the_one_route() {
    let routes = error_header_http_rest_transport::ROUTES;
    assert_eq!(
        routes.len(),
        1,
        "got: {:?}",
        routes
            .iter()
            .map(error_header_http_rest_transport::Route::path)
            .collect::<Vec<_>>()
    );
    assert_eq!(routes[0].method(), "GET");
    assert_eq!(routes[0].path(), "/ranges/{id}");
    assert_eq!(routes[0].operation(), "get-range");
    assert_eq!(routes[0].ok_status(), 200);
    let mut error_statuses = routes[0].error_statuses().to_vec();
    error_statuses.sort_unstable();
    assert_eq!(error_statuses, vec![404, 416]);
}

/// `IncomingRequest` reads back everything it was built with.
#[test]
fn an_incoming_request_reads_back_its_body_headers_and_query() {
    let request = error_header_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/ranges/oversized".to_owned(),
        "unused=1".to_owned(),
        vec![("x-trace".to_owned(), "abc".to_owned())],
        b"ignored".to_vec(),
    );
    assert_eq!(request.body(), b"ignored");
    assert_eq!(request.query(), "unused=1");
    assert_eq!(
        request.headers(),
        &[("x-trace".to_owned(), "abc".to_owned())]
    );
    assert_eq!(request.header("x-trace"), Some("abc"));
}

/// An owner-installed `FaultHandler` still reaches `OutgoingResponse::new` and its own `headers()`.
#[test]
fn an_installed_fault_handler_still_builds_an_outgoing_response_by_hand() {
    let request = error_header_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/nowhere".to_owned(),
        String::new(),
        Vec::new(),
        Vec::new(),
    );
    let response = poll_once(error_header_http_rest_transport::dispatch(
        &RangeBackEnd,
        &(),
        &request,
        &RecordingFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 499);
    assert_eq!(
        response.headers(),
        &[("x-fault-kind".to_owned(), "unknown operation".to_owned())]
    );
    assert_eq!(
        response.body(),
        b"handled: the service answers to no operation by that name"
    );
}
