//! A bodyless `GET` operation whose one ordinary argument is a `Named` message the author declared
//! — `DownloadRequest`, mirroring the shape a single-struct request takes once it carries more than
//! the path spends (`document_id` binds `{document_id}`; `verbose` does not, and the path leaves it
//! nowhere to go but the query string). A service of its own, so the fix is proven independent of
//! `DocumentService`'s own macro-generated (`Generated`) query fields.

#![cfg(feature = "serde")]

use crate::named_query_http_rest_transport;
use core::future::{Future, ready};
use core::pin::pin;
use core::task::{Context as PollContext, Poll, Waker};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tixschema::{model_schema, service_schema};

#[model_schema()]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DownloadRequest {
    pub document_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbose: Option<bool>,
}

#[model_schema()]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaDescriptor {
    pub document_id: String,
}

#[model_schema()]
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", tag = "errorCode")]
pub enum DownloadError {
    NotFound,
}

/// Writes down every `DownloadRequest` the dispatcher actually built, so a test can say what the
/// implementation received rather than only what status came back.
pub struct MediaBackEnd {
    reached: Mutex<Vec<DownloadRequest>>,
}

#[service_schema(transports = ["http_rest"])]
pub trait MediaService<Ctx> {
    #[service_schema_op(http(
        method = "GET",
        path = "/media/{document_id}",
        error_status(NotFound = 404),
    ))]
    async fn download(
        &self,
        ctx: &Ctx,
        req: DownloadRequest,
    ) -> Result<MediaDescriptor, DownloadError>;
}

impl MediaService<()> for MediaBackEnd {
    async fn download(
        &self,
        _ctx: &(),
        req: DownloadRequest,
    ) -> Result<MediaDescriptor, DownloadError> {
        ready(()).await;
        let document_id = req.document_id.clone();
        self.reached.lock().unwrap().push(req);
        if document_id == "missing" {
            return Err(DownloadError::NotFound);
        }
        Ok(MediaDescriptor { document_id })
    }
}

impl MediaBackEnd {
    fn new() -> Self {
        Self {
            reached: Mutex::new(Vec::new()),
        }
    }

    fn reached(&self) -> Vec<DownloadRequest> {
        self.reached.lock().unwrap().clone()
    }
}

/// An owner-installed `FaultHandler`, exercising `OutgoingResponse::new` and its own `headers()`
/// accessor directly - the same construction path every other body kind's own override uses.
struct RecordingFaultHandler;

impl named_query_http_rest_transport::FaultHandler for RecordingFaultHandler {
    fn on_fault(
        &self,
        fault: &media_service_schema::ServiceFault,
    ) -> named_query_http_rest_transport::OutgoingResponse {
        named_query_http_rest_transport::OutgoingResponse::new(
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

/// The bug this proves fixed: `document_id` binds the one path placeholder; `verbose` is not
/// placeholder-bound, so on a bodyless method it can only come from the query string. Before the
/// fix, a `Named` message's own object carried nothing but the placeholder-bound fields, so
/// `verbose` always deserialized as `None` regardless of `?verbose=true`.
#[test]
fn a_named_message_s_unbound_field_binds_off_the_query_string() {
    let service = MediaBackEnd::new();
    let request = named_query_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/media/quarterly-report".to_owned(),
        "verbose=true".to_owned(),
        Vec::new(),
        Vec::new(),
    );
    let response = poll_once(named_query_http_rest_transport::dispatch(
        &service,
        &(),
        &request,
        &named_query_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 200, "got: {:?}", response.body());
    assert_eq!(
        service.reached(),
        vec![DownloadRequest {
            document_id: "quarterly-report".to_owned(),
            verbose: Some(true),
        }]
    );
}

#[test]
fn the_same_field_decodes_as_none_when_the_query_carries_nothing() {
    let service = MediaBackEnd::new();
    let request = named_query_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/media/quarterly-report".to_owned(),
        String::new(),
        Vec::new(),
        Vec::new(),
    );
    let response = poll_once(named_query_http_rest_transport::dispatch(
        &service,
        &(),
        &request,
        &named_query_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 200, "got: {:?}", response.body());
    assert_eq!(
        service.reached(),
        vec![DownloadRequest {
            document_id: "quarterly-report".to_owned(),
            verbose: None,
        }]
    );
}

/// The route table still lists the one route correctly — the fix touches only how the message is
/// assembled, not what an adapter iterates to register a handler.
#[test]
fn the_route_table_lists_the_one_route() {
    let routes = named_query_http_rest_transport::ROUTES;
    assert_eq!(
        routes.len(),
        1,
        "got: {:?}",
        routes
            .iter()
            .map(named_query_http_rest_transport::Route::path)
            .collect::<Vec<_>>()
    );
    assert_eq!(routes[0].method(), "GET");
    assert_eq!(routes[0].path(), "/media/{document_id}");
    assert_eq!(routes[0].operation(), "download");
    assert_eq!(routes[0].ok_status(), 200);
    assert_eq!(routes[0].error_statuses(), &[404]);
}

/// `IncomingRequest` reads back everything it was built with — the same accessors every other
/// body kind in this harness already reaches for.
#[test]
fn an_incoming_request_reads_back_its_body_headers_and_query() {
    let request = named_query_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/media/quarterly-report".to_owned(),
        "verbose=true".to_owned(),
        vec![("x-trace".to_owned(), "abc".to_owned())],
        b"ignored".to_vec(),
    );
    assert_eq!(request.body(), b"ignored");
    assert_eq!(request.query(), "verbose=true");
    assert_eq!(
        request.headers(),
        &[("x-trace".to_owned(), "abc".to_owned())]
    );
    assert_eq!(request.header("x-trace"), Some("abc"));
}

/// An owner-installed `FaultHandler` still reaches `OutgoingResponse::new` and its own `headers()`.
#[test]
fn an_installed_fault_handler_still_builds_an_outgoing_response_by_hand() {
    let request = named_query_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/nowhere".to_owned(),
        String::new(),
        Vec::new(),
        Vec::new(),
    );
    let response = poll_once(named_query_http_rest_transport::dispatch(
        &MediaBackEnd::new(),
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
