//! A `body = "multipart"` operation whose one ordinary argument is a `Named` message the author
//! declared — `UploadMediaRequest`, mirroring the repo's own established convention of declaring an
//! operation's scalar fields as one hand-declared `#[model_schema]` struct rather than bare
//! arguments — driven through the `http_rest` dispatcher by hand: `mime` and `filename` travel as
//! text parts alongside the file itself, no path placeholder involved.

#![cfg(feature = "serde")]

use crate::named_multipart_http_rest_transport;
use core::future::{Future, ready};
use core::pin::pin;
use core::task::{Context as PollContext, Poll, Waker};
use serde::{Deserialize, Serialize};
use std::io::{self, Read};
use std::sync::Mutex;
use tixschema::{model_schema, service_schema};

/// What one call recorded: the decoded message and the file part's whole content, drained through
/// `BodySource::pull`.
type Reached = (UploadMediaRequest, Vec<u8>);

#[model_schema()]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UploadMediaRequest {
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[model_schema()]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaDescriptor {
    pub mime: String,
}

#[model_schema()]
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", tag = "errorCode")]
pub enum UploadMediaError {
    TooLarge,
}

#[service_schema(transports = ["http_rest"])]
pub trait MediaUploadService<Ctx> {
    #[service_schema_op(http(
        method = "POST",
        path = "/media",
        body = "multipart",
        part("file" = file),
        error_status(TooLarge = 413),
    ))]
    async fn upload(
        &self,
        ctx: &Ctx,
        req: UploadMediaRequest,
        file: Box<dyn media_upload_service_schema::BodySource + Send>,
    ) -> Result<MediaDescriptor, UploadMediaError>;
}

/// A fixed-size in-memory reader, standing in for a real chunked upload body.
struct ByteSource {
    remaining: Vec<u8>,
}

impl Read for ByteSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let take = buf.len().min(self.remaining.len());
        let rest = self.remaining.split_off(take);
        buf[..take].copy_from_slice(&self.remaining);
        self.remaining = rest;
        Ok(take)
    }
}

/// Records what the implementation actually received.
pub struct MediaUploadBackEnd {
    reached: Mutex<Vec<Reached>>,
}

impl MediaUploadService<()> for MediaUploadBackEnd {
    async fn upload(
        &self,
        _ctx: &(),
        req: UploadMediaRequest,
        mut file: Box<dyn media_upload_service_schema::BodySource + Send>,
    ) -> Result<MediaDescriptor, UploadMediaError> {
        ready(()).await;
        let mut drained = Vec::new();
        let mut buf = [0_u8; 64];
        loop {
            let read = file.pull(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            drained.extend_from_slice(&buf[..read]);
        }
        let mime = req.mime.clone();
        self.reached.lock().unwrap().push((req, drained));
        if mime == "toolarge" {
            return Err(UploadMediaError::TooLarge);
        }
        Ok(MediaDescriptor { mime })
    }
}

impl MediaUploadBackEnd {
    fn new() -> Self {
        Self {
            reached: Mutex::new(Vec::new()),
        }
    }

    fn reached(&self) -> Vec<Reached> {
        self.reached.lock().unwrap().clone()
    }
}

/// An owner-installed `FaultHandler`, exercising `OutgoingResponse::new` and its own `headers()`
/// accessor directly - unaffected by the extra `parts` argument a multipart operation's own
/// dispatcher takes.
struct RecordingFaultHandler;

impl named_multipart_http_rest_transport::FaultHandler for RecordingFaultHandler {
    fn on_fault(
        &self,
        fault: &media_upload_service_schema::ServiceFault,
    ) -> named_multipart_http_rest_transport::OutgoingResponse {
        named_multipart_http_rest_transport::OutgoingResponse::new(
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

/// The bug this proves fixed: with a `Named` message aliased directly as the multipart operation's
/// whole message, the dispatcher used to fall back to parsing the (empty, for a multipart request)
/// JSON body, answering 400 on every call regardless of well-formed parts.
#[test]
fn a_named_message_s_scalar_fields_bind_off_the_remaining_text_parts() {
    let service = MediaUploadBackEnd::new();
    let request = named_multipart_http_rest_transport::IncomingRequest::new(
        "POST".to_owned(),
        "/media".to_owned(),
        String::new(),
        Vec::new(),
        Vec::new(),
    );
    let parts = vec![
        (
            "mime".to_owned(),
            named_multipart_http_rest_transport::IncomingPart::Text("image/png".to_owned()),
        ),
        (
            "filename".to_owned(),
            named_multipart_http_rest_transport::IncomingPart::Text("cat.png".to_owned()),
        ),
        (
            "file".to_owned(),
            named_multipart_http_rest_transport::IncomingPart::File(Box::new(ByteSource {
                remaining: b"file-bytes".to_vec(),
            })),
        ),
    ];
    let response = poll_once(named_multipart_http_rest_transport::dispatch(
        &service,
        &(),
        &request,
        parts,
        &named_multipart_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(response.status(), 200, "got: {:?}", response.body());
    assert_eq!(
        service.reached(),
        vec![(
            UploadMediaRequest {
                mime: "image/png".to_owned(),
                filename: Some("cat.png".to_owned()),
            },
            b"file-bytes".to_vec()
        )]
    );
}

#[test]
fn an_absent_optional_scalar_part_decodes_as_none() {
    let service = MediaUploadBackEnd::new();
    let request = named_multipart_http_rest_transport::IncomingRequest::new(
        "POST".to_owned(),
        "/media".to_owned(),
        String::new(),
        Vec::new(),
        Vec::new(),
    );
    let parts = vec![
        (
            "mime".to_owned(),
            named_multipart_http_rest_transport::IncomingPart::Text("image/jpeg".to_owned()),
        ),
        (
            "file".to_owned(),
            named_multipart_http_rest_transport::IncomingPart::File(Box::new(ByteSource {
                remaining: b"bytes".to_vec(),
            })),
        ),
    ];
    poll_once(named_multipart_http_rest_transport::dispatch(
        &service,
        &(),
        &request,
        parts,
        &named_multipart_http_rest_transport::DefaultFaultHandler,
    ))
    .unwrap();
    assert_eq!(
        service.reached(),
        vec![(
            UploadMediaRequest {
                mime: "image/jpeg".to_owned(),
                filename: None,
            },
            b"bytes".to_vec()
        )]
    );
}

/// The route table lists the one multipart route, its statuses included.
#[test]
fn the_route_table_lists_the_one_route() {
    let routes = named_multipart_http_rest_transport::ROUTES;
    assert_eq!(
        routes.len(),
        1,
        "got: {:?}",
        routes
            .iter()
            .map(named_multipart_http_rest_transport::Route::path)
            .collect::<Vec<_>>()
    );
    assert_eq!(routes[0].method(), "POST");
    assert_eq!(routes[0].path(), "/media");
    assert_eq!(routes[0].operation(), "upload");
    assert_eq!(routes[0].ok_status(), 200);
    assert_eq!(routes[0].error_statuses(), &[413]);
}

/// `IncomingRequest` reads back everything it was built with, exercised here for a multipart
/// operation's own dispatcher expansion - the same accessors every other body kind reaches for.
#[test]
fn an_incoming_request_reads_back_its_body_headers_and_query() {
    let request = named_multipart_http_rest_transport::IncomingRequest::new(
        "POST".to_owned(),
        "/media".to_owned(),
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

/// An owner-installed `FaultHandler` still reaches `OutgoingResponse::new` and its own `headers()`
/// on a multipart service's dispatcher, unaffected by the extra `parts` argument `dispatch` takes.
#[test]
fn an_installed_fault_handler_still_builds_an_outgoing_response_by_hand() {
    let request = named_multipart_http_rest_transport::IncomingRequest::new(
        "GET".to_owned(),
        "/nowhere".to_owned(),
        String::new(),
        Vec::new(),
        Vec::new(),
    );
    let response = poll_once(named_multipart_http_rest_transport::dispatch(
        &MediaUploadBackEnd::new(),
        &(),
        &request,
        Vec::new(),
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
