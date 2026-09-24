//! The `http_rest` dispatcher for `MediaUploadService`, in a module of its own.

use crate::media_upload_service_schema;
use crate::named_multipart_service::UploadMediaError;

media_upload_service_http_rest_dispatcher!();
