//! The `http_rest` dispatcher for `MediaService`, in a module of its own — the same placement
//! rules apply to this transport's macro as to `DocumentService`'s own.

use crate::named_query_service::DownloadError;

media_service_http_rest_dispatcher!();
