//! `relay-icon` and `relay-thumb` URL schemes: shell icons and thumbnails as
//! PNG. Icons load as plain images and share the webview's cache; the grid
//! fetches thumbnails and paints them itself. URL format in
//! `relay_explorer::image_url`.

use tauri::http::{Request, Response, StatusCode};
use tauri::{Runtime, UriSchemeContext, UriSchemeResponder};

pub fn handle_icon<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    serve(ctx, request, responder, false);
}

pub fn handle_thumb<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    serve(ctx, request, responder, true);
}

fn not_found() -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = StatusCode::NOT_FOUND;
    allow_fetch(&mut response);
    response
}

/// Thumbnails are fetched (and decoded to a bitmap) from the page's own
/// origin, which is a different origin from the scheme's.
fn allow_fetch(response: &mut Response<Vec<u8>>) {
    response.headers_mut().insert(
        tauri::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        tauri::http::HeaderValue::from_static("*"),
    );
}

#[cfg(windows)]
fn serve<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
    thumbnail: bool,
) {
    use relay_explorer::image_url;
    use tauri::Manager;

    let uri = request.uri();
    let Some(req) = image_url::parse(uri.path(), uri.query()) else {
        return responder.respond(not_found());
    };
    let state = ctx.app_handle().state::<super::ExplorerState>();
    let shell = match state.shell() {
        Ok(shell) => shell.clone(),
        Err(err) => {
            log::warn!("explorer shell: {err}");
            return responder.respond(not_found());
        }
    };
    let key = if thumbnail {
        None
    } else {
        image_url::icon_cache_key(&req.path, req.flags)
    };
    shell.image(req.path, req.size, thumbnail, key, move |bytes| {
        responder.respond(match bytes {
            Some(bytes) => png(bytes.as_ref().clone()),
            None => not_found(),
        });
    });
}

#[cfg(windows)]
fn png(bytes: Vec<u8>) -> Response<Vec<u8>> {
    use tauri::http::header;

    let mut response = Response::new(bytes);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, "image/png".parse().expect("static"));
    headers.insert(
        header::CACHE_CONTROL,
        "max-age=31536000, immutable".parse().expect("static"),
    );
    allow_fetch(&mut response);
    response
}

#[cfg(not(windows))]
fn serve<R: Runtime>(
    _ctx: UriSchemeContext<'_, R>,
    _request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
    _thumbnail: bool,
) {
    responder.respond(not_found());
}
