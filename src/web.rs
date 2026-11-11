//! `tunnel http serve`: a folder answered over HTTP/1.1 on each tunnel stream,
//! read-only and without any TCP listener on the serving machine.
//!
//! Every request path is resolved inside the folder: `..`, other roots and
//! links leading outside it are not found, and neither are names starting
//! with a dot unless `--hidden`. By default a folder shows its listing, and
//! HTML, SVG and XML open without scripts (`Content-Security-Policy: sandbox`):
//! the other machine's browser runs pages on its own `127.0.0.1`, whose
//! cookies every local port shares. `--site` serves `index.html` and
//! `404.html`, and lets pages run their scripts.
use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::Metadata,
    io::SeekFrom,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Empty, Full, StreamBody, combinators::BoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body as _, Bytes, Frame},
    header::{self, HeaderValue},
};
use hyper_util::rt::{TokioIo, TokioTimer};
use iroh::endpoint::{RecvStream, SendStream};
use n0_future::StreamExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

type Body = BoxBody<Bytes, std::io::Error>;

/// Bytes kept as they are in the links of a listing and in redirects.
const PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// What `tunnel http serve` offers.
pub struct Site {
    /// The folder, canonical: every served path stays below it.
    root: PathBuf,
    /// The folder as shown in the announcement.
    shown: PathBuf,
    /// `--site`: `index.html`, scripts and `404.html`.
    site: bool,
    /// `--hidden`: names starting with a dot are served.
    hidden: bool,
}

/// A file or folder that may be served.
struct Found {
    path: PathBuf,
    meta: Metadata,
}

/// The answer to a request, decided by blocking file system calls.
enum Plan {
    NotFound,
    /// A folder asked without its final `/`: the URL its relative links need.
    Redirect(String),
    Listing(String),
    File(Found, StatusCode),
}

/// What a `Range` header asks of a file.
#[derive(Debug, PartialEq)]
enum Wanted {
    All,
    /// First and last byte, inclusive.
    Part(u64, u64),
    Unsatisfiable,
}

impl Site {
    pub fn open(path: &Path, site: bool, hidden: bool) -> Result<Self> {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!("{} is not a folder", path.display()),
            Err(_) => match address_like(path) {
                Some(address) => bail!(
                    "{} is not a folder; to forward a running web server, use `tunnel tcp serve --to {address}`",
                    path.display()
                ),
                None => bail!("{}: no such folder", path.display()),
            },
        }
        let root = std::fs::canonicalize(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self {
            root,
            shown: std::path::absolute(path).unwrap_or_else(|_| path.to_owned()),
            site,
            hidden,
        })
    }

    /// The announcement line: what is served and how.
    pub fn describe(&self) -> String {
        let dotfiles = if self.hidden {
            "dotfiles served"
        } else {
            "dotfiles hidden"
        };
        if self.site {
            format!(
                "Site: {} (read-only; {dotfiles}; its pages run their scripts in the visitor's browser)",
                self.shown.display()
            )
        } else {
            format!(
                "Files: {} (read-only; {dotfiles}; HTML, SVG and XML open without scripts)",
                self.shown.display()
            )
        }
    }

    /// Whether `name` may be part of a served path.
    fn allowed(&self, name: &OsStr) -> bool {
        let mut parts = Path::new(name).components();
        let single = matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        );
        let text = name.to_string_lossy();
        // `file:stream` names an alternate data stream on Windows.
        single
            && !text.contains('\0')
            && !(cfg!(windows) && text.contains(':'))
            && (self.hidden || !text.starts_with('.'))
    }

    /// The file or folder that the decoded request path `names` leads to.
    fn resolve(&self, names: &[OsString]) -> Option<Found> {
        let mut path = self.root.clone();
        for name in names {
            if !self.allowed(name) {
                return None;
            }
            path.push(name);
        }
        // A link may lead outside the folder or to a hidden name.
        let path = std::fs::canonicalize(&path).ok()?;
        let inside = path.strip_prefix(&self.root).ok()?;
        if !inside
            .components()
            .all(|part| self.allowed(part.as_os_str()))
        {
            return None;
        }
        let meta = std::fs::metadata(&path).ok()?;
        (meta.is_dir() || meta.is_file()).then_some(Found { path, meta })
    }

    fn plan(&self, path: &str) -> Plan {
        let Some(names) = names(path) else {
            return self.missing();
        };
        let Some(found) = self.resolve(&names) else {
            return self.missing();
        };
        if found.meta.is_file() {
            return Plan::File(found, StatusCode::OK);
        }
        if !path.ends_with('/') {
            return Plan::Redirect(folder_url(&names));
        }
        if self.site {
            let mut index = names;
            index.push("index.html".into());
            return match self.resolve(&index) {
                Some(found) if found.meta.is_file() => Plan::File(found, StatusCode::OK),
                _ => self.missing(),
            };
        }
        match self.listing(&found.path, &names) {
            Ok(html) => Plan::Listing(html),
            Err(_) => self.missing(),
        }
    }

    /// The answer for a missing page: a site's own `404.html`, if it has one.
    fn missing(&self) -> Plan {
        if self.site
            && let Some(found) = self.resolve(&["404.html".into()])
            && found.meta.is_file()
        {
            return Plan::File(found, StatusCode::NOT_FOUND);
        }
        Plan::NotFound
    }

    /// The listing of `folder`, reached by `names`: what may be served in it,
    /// folders first.
    fn listing(&self, folder: &Path, names: &[OsString]) -> std::io::Result<String> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(folder)? {
            let name = entry?.file_name();
            let mut path = names.to_vec();
            path.push(name.clone());
            if let Some(found) = self.resolve(&path) {
                entries.push((name, found.meta));
            }
        }
        entries.sort_by_cached_key(|(name, meta)| {
            (!meta.is_dir(), name.to_string_lossy().to_lowercase())
        });
        let mut title = String::from("/");
        for name in names {
            title += &format!("{}/", name.to_string_lossy());
        }
        let title = escape(&format!("Index of {title}"));
        let mut html = format!(
            "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>{title}</title><style>{STYLE}</style></head>\n<body><h1>{title}</h1>\n\
             <table><thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead><tbody>\n"
        );
        if !names.is_empty() {
            html += "<tr><td><a href=\"../\">../</a></td><td></td><td></td></tr>\n";
        }
        for (name, meta) in entries {
            let (slash, size) = if meta.is_dir() {
                ("/", String::new())
            } else {
                ("", crate::progress::size(meta.len()))
            };
            let modified = meta
                .modified()
                .map(|time| {
                    chrono::DateTime::<chrono::Local>::from(time)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            // `./` keeps a name such as `javascript:x` a relative link.
            let _ = writeln!(
                html,
                "<tr><td><a href=\"./{}{slash}\">{}{slash}</a></td><td class=\"n\">{size}</td><td>{modified}</td></tr>",
                encode(&name),
                escape(&name.to_string_lossy()),
            );
        }
        html += "</tbody></table></body></html>\n";
        Ok(html)
    }

    /// A file's response: the whole file or the part a `Range` asks for.
    async fn file(
        &self,
        request: &Request<impl Sized>,
        found: Found,
        status: StatusCode,
    ) -> Response<Body> {
        let len = found.meta.len();
        let content_type = content_type(&found.path);
        let modified = found.meta.modified().ok();
        let last_modified = modified.map(httpdate::fmt_http_date);
        let header = |name| {
            request
                .headers()
                .get(name)
                .and_then(|value: &HeaderValue| value.to_str().ok())
        };
        let mut wanted = Wanted::All;
        if status == StatusCode::OK {
            if let (Some(modified), Some(since)) = (
                modified,
                header(header::IF_MODIFIED_SINCE).and_then(|v| httpdate::parse_http_date(v).ok()),
            ) && unchanged(modified, since)
            {
                let mut response = empty_response(StatusCode::NOT_MODIFIED);
                set(
                    &mut response,
                    header::LAST_MODIFIED,
                    last_modified.as_deref(),
                );
                return response;
            }
            // A range of another version of the file would corrupt it.
            if header(header::IF_RANGE).is_none_or(|tag| Some(tag) == last_modified.as_deref()) {
                wanted = wanted_range(header(header::RANGE), len);
            }
        }
        let (status, first, count) = match wanted {
            Wanted::All => (status, 0, len),
            Wanted::Part(first, last) => (StatusCode::PARTIAL_CONTENT, first, last - first + 1),
            Wanted::Unsatisfiable => {
                let mut response = text(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "416 Range Not Satisfiable\n",
                );
                set(
                    &mut response,
                    header::CONTENT_RANGE,
                    Some(&format!("bytes */{len}")),
                );
                return response;
            }
        };
        let body = match open_part(&found.path, first, count).await {
            Ok(body) => body,
            Err(_) => return text(StatusCode::NOT_FOUND, NOT_FOUND),
        };
        let mut response = Response::new(body);
        *response.status_mut() = status;
        set(&mut response, header::CONTENT_TYPE, Some(content_type));
        set(
            &mut response,
            header::CONTENT_LENGTH,
            Some(&count.to_string()),
        );
        set(&mut response, header::ACCEPT_RANGES, Some("bytes"));
        set(
            &mut response,
            header::LAST_MODIFIED,
            last_modified.as_deref(),
        );
        // The file may change on the serving machine: browsers ask again.
        set(&mut response, header::CACHE_CONTROL, Some("no-cache"));
        set(
            &mut response,
            header::X_CONTENT_TYPE_OPTIONS,
            Some("nosniff"),
        );
        if status == StatusCode::PARTIAL_CONTENT {
            let last = first + count - 1;
            set(
                &mut response,
                header::CONTENT_RANGE,
                Some(&format!("bytes {first}-{last}/{len}")),
            );
        }
        if !self.site && scriptable(content_type) {
            set(
                &mut response,
                header::CONTENT_SECURITY_POLICY,
                Some("sandbox"),
            );
        }
        response
    }
}

const NOT_FOUND: &str = "404 Not Found\n";
const STYLE: &str = ":root{color-scheme:light dark}body{font-family:system-ui,sans-serif;margin:2rem auto;max-width:60rem;padding:0 1rem}table{border-collapse:collapse;width:100%}th,td{padding:.3rem .8rem;text-align:left}td.n{text-align:right;white-space:nowrap}tbody tr:nth-child(odd){background:#8881}a{text-decoration:none}a:hover{text-decoration:underline}";

/// Answers HTTP/1.1 on one tunnel stream until the browser closes its
/// connection or leaves it idle beyond `timeout`.
pub async fn serve(
    site: &Arc<Site>,
    mut send: SendStream,
    mut recv: RecvStream,
    timeout: Duration,
) -> Result<()> {
    let io = TokioIo::new(tokio::io::join(&mut recv, &mut send));
    let service = hyper::service::service_fn(|request| {
        let site = site.clone();
        async move { Ok::<_, Infallible>(respond(site, request).await) }
    });
    let served = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(timeout)
        .serve_connection(io, service)
        .await;
    // Browsers drop connections at will, for instance when a video seeks.
    if let Err(error) = served {
        detail!("http: connection ended: {error}");
    }
    // Deliver the last response before the connection closes.
    let _ = send.finish();
    let _ = tokio::time::timeout(timeout, send.stopped()).await;
    Ok(())
}

async fn respond<B>(site: Arc<Site>, request: Request<B>) -> Response<Body> {
    let method = request.method().clone();
    let mut response = if !local_host(request.headers().get(header::HOST)) {
        text(
            StatusCode::MISDIRECTED_REQUEST,
            "421 Misdirected Request: open this folder at 127.0.0.1 or localhost\n",
        )
    } else if method == Method::GET || method == Method::HEAD {
        let path = request.uri().path().to_owned();
        let planner = site.clone();
        let plan = tokio::task::spawn_blocking(move || planner.plan(&path))
            .await
            .unwrap_or(Plan::NotFound);
        match plan {
            Plan::NotFound => text(StatusCode::NOT_FOUND, NOT_FOUND),
            Plan::Redirect(location) => {
                let mut response = text(StatusCode::MOVED_PERMANENTLY, "301 Moved Permanently\n");
                set(&mut response, header::LOCATION, Some(&location));
                response
            }
            Plan::Listing(html) => {
                let mut response = Response::new(full(html));
                set(
                    &mut response,
                    header::CONTENT_TYPE,
                    Some("text/html; charset=utf-8"),
                );
                set(&mut response, header::CACHE_CONTROL, Some("no-cache"));
                set(
                    &mut response,
                    header::X_CONTENT_TYPE_OPTIONS,
                    Some("nosniff"),
                );
                set(
                    &mut response,
                    header::CONTENT_SECURITY_POLICY,
                    Some("default-src 'none'; style-src 'unsafe-inline'"),
                );
                response
            }
            Plan::File(found, status) => site.file(&request, found, status).await,
        }
    } else {
        let mut response = text(
            StatusCode::METHOD_NOT_ALLOWED,
            "405 Method Not Allowed: this folder is read-only\n",
        );
        set(&mut response, header::ALLOW, Some("GET, HEAD"));
        response
    };
    detail!(
        "http: {method} {} {}",
        request.uri().path(),
        response.status().as_u16()
    );
    if method == Method::HEAD {
        // The headers of a GET, without its body.
        let (mut parts, body) = response.into_parts();
        if let Some(len) = body.size_hint().exact() {
            parts.headers.insert(header::CONTENT_LENGTH, len.into());
        }
        response = Response::from_parts(parts, empty());
    }
    response
}

/// Whether `Host` names an address or `localhost`, never a domain: a page
/// whose domain was rebound to the visitor's loopback sends that domain, and
/// must not read the folder as its own origin. Without `Host`, no domain was
/// named.
fn local_host(host: Option<&HeaderValue>) -> bool {
    let Some(host) = host else { return true };
    let Ok(host) = host.to_str() else {
        return false;
    };
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(ip, port)| {
            ip.parse::<Ipv6Addr>().is_ok() && (port.is_empty() || port.starts_with(':'))
        });
    }
    let name = host.split_once(':').map_or(host, |(name, _)| name);
    name.eq_ignore_ascii_case("localhost") || name.parse::<Ipv4Addr>().is_ok()
}

/// The address a browser opens for a local listener: a wildcard bind is
/// reached on loopback.
pub fn url(mut local: SocketAddr) -> String {
    if local.ip().is_unspecified() {
        local.set_ip(if local.is_ipv4() {
            Ipv4Addr::LOCALHOST.into()
        } else {
            Ipv6Addr::LOCALHOST.into()
        });
    }
    format!("http://{local}/")
}

/// `HOST:PORT`, a port or an http URL where a folder is expected, as the
/// `--to` value it stands for.
fn address_like(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let bare = ["http://", "https://"]
        .iter()
        .find_map(|scheme| text.strip_prefix(scheme))
        .unwrap_or(text)
        .trim_end_matches('/');
    if bare.parse::<u16>().is_ok_and(|port| port > 0) {
        return Some(format!("127.0.0.1:{bare}"));
    }
    crate::streams::validate_address(bare, false).ok()?;
    Some(bare.to_owned())
}

/// The decoded names of a request path; empty names (`//`) are skipped. An
/// encoded `/` stays inside its name, which then cannot be served.
fn names(path: &str) -> Option<Vec<OsString>> {
    path.split('/')
        .filter(|name| !name.is_empty())
        .map(|name| {
            let bytes: Vec<u8> = percent_decode_str(name).collect();
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                Some(OsString::from_vec(bytes))
            }
            #[cfg(not(unix))]
            {
                String::from_utf8(bytes).ok().map(OsString::from)
            }
        })
        .collect()
}

fn encode(name: &OsStr) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        percent_encoding::percent_encode(name.as_bytes(), PATH).to_string()
    }
    #[cfg(not(unix))]
    {
        percent_encoding::utf8_percent_encode(&name.to_string_lossy(), PATH).to_string()
    }
}

/// The URL of a folder, built from its names: never `//host`.
fn folder_url(names: &[OsString]) -> String {
    let mut url = String::from("/");
    for name in names {
        url += &encode(name);
        url.push('/');
    }
    url
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped += "&amp;",
            '<' => escaped += "&lt;",
            '>' => escaped += "&gt;",
            '"' => escaped += "&quot;",
            '\'' => escaped += "&#39;",
            c => escaped.push(c),
        }
    }
    escaped
}

/// Whether a file modified at `modified` is the version a browser has from
/// `since`; HTTP dates count whole seconds.
fn unchanged(modified: SystemTime, since: SystemTime) -> bool {
    let seconds = |time: SystemTime| {
        time.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok()
    };
    matches!((seconds(modified), seconds(since)), (Some(m), Some(s)) if m <= s)
}

/// The part of a file of `len` bytes that `range` asks for. A form other than
/// a single `bytes=` range gets the whole file, as HTTP allows.
fn wanted_range(range: Option<&str>, len: u64) -> Wanted {
    let Some(spec) = range.and_then(|range| range.trim().strip_prefix("bytes=")) else {
        return Wanted::All;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return Wanted::All;
    };
    if last.contains(',') {
        return Wanted::All;
    }
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // The last `n` bytes.
        return match last.parse::<u64>() {
            Ok(0) => Wanted::Unsatisfiable,
            Ok(_) if len == 0 => Wanted::Unsatisfiable,
            Ok(n) => Wanted::Part(len.saturating_sub(n), len - 1),
            Err(_) => Wanted::All,
        };
    }
    let Ok(first) = first.parse::<u64>() else {
        return Wanted::All;
    };
    let last = match last {
        "" => u64::MAX,
        last => match last.parse::<u64>() {
            Ok(last) if last >= first => last,
            _ => return Wanted::All,
        },
    };
    if first >= len {
        return Wanted::Unsatisfiable;
    }
    Wanted::Part(first, last.min(len - 1))
}

fn content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "xhtml" => "application/xhtml+xml",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" | "md" | "log" | "toml" | "yaml" | "yml" | "ini" | "conf" | "rs" | "py" | "sh" => {
            "text/plain; charset=utf-8"
        }
        "csv" => "text/csv; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        // With nosniff, the browser offers to download it.
        _ => "application/octet-stream",
    }
}

/// Types whose scripts a browser runs when it opens them.
fn scriptable(content_type: &str) -> bool {
    [
        "text/html",
        "application/xhtml+xml",
        "application/xml",
        "image/svg+xml",
    ]
    .iter()
    .any(|scriptable| content_type.starts_with(scriptable))
}

async fn open_part(path: &Path, first: u64, count: u64) -> std::io::Result<Body> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(SeekFrom::Start(first)).await?;
    let chunks = ReaderStream::with_capacity(file.take(count), 64 * 1024)
        .map(|chunk| chunk.map(Frame::data));
    Ok(BodyExt::boxed(StreamBody::new(chunks)))
}

fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

fn empty_response(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(empty());
    *response.status_mut() = status;
    response
}

fn text(status: StatusCode, body: &'static str) -> Response<Body> {
    let mut response = Response::new(full(body));
    *response.status_mut() = status;
    set(
        &mut response,
        header::CONTENT_TYPE,
        Some("text/plain; charset=utf-8"),
    );
    set(
        &mut response,
        header::X_CONTENT_TYPE_OPTIONS,
        Some("nosniff"),
    );
    response
}

fn set(response: &mut Response<Body>, name: header::HeaderName, value: Option<&str>) {
    if let Some(value) = value.and_then(|value| HeaderValue::from_str(value).ok()) {
        response.headers_mut().insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(root: &Path, site: bool, hidden: bool) -> Site {
        Site::open(root, site, hidden).unwrap()
    }

    fn tree() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("hello.txt"), "hello from A").unwrap();
        std::fs::write(root.path().join(".env"), "secret").unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/index.html"), "<p>sub</p>").unwrap();
        root
    }

    fn served(site: &Site, path: &str) -> Option<String> {
        match site.plan(path) {
            Plan::File(found, _) => Some(std::fs::read_to_string(found.path).unwrap()),
            _ => None,
        }
    }

    #[test]
    fn paths_stay_inside_the_folder_and_hide_dotfiles() {
        let root = tree();
        let files = site(root.path(), false, false);
        assert_eq!(served(&files, "/hello.txt").unwrap(), "hello from A");
        assert_eq!(served(&files, "//sub//index.html").unwrap(), "<p>sub</p>");
        for path in [
            "/../hello.txt",
            "/sub/../../hello.txt",
            "/%2e%2e/hello.txt",
            "/sub%2f..%2fhello.txt",
            "/./hello.txt",
            "/.env",
            "/%2eenv",
            "/missing",
        ] {
            assert!(matches!(files.plan(path), Plan::NotFound), "{path}");
        }
        let hidden = site(root.path(), false, true);
        assert_eq!(served(&hidden, "/.env").unwrap(), "secret");
        assert!(matches!(hidden.plan("/../hello.txt"), Plan::NotFound));
    }

    #[cfg(unix)]
    #[test]
    fn links_out_of_the_folder_or_to_dotfiles_are_not_found() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "outside").unwrap();
        let root = tree();
        std::os::unix::fs::symlink(outside.path().join("passwd"), root.path().join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("outdir")).unwrap();
        std::os::unix::fs::symlink(root.path().join(".env"), root.path().join("env")).unwrap();
        std::os::unix::fs::symlink(root.path().join("hello.txt"), root.path().join("link"))
            .unwrap();
        let files = site(root.path(), false, false);
        for path in ["/out", "/outdir/passwd", "/outdir/", "/env"] {
            assert!(matches!(files.plan(path), Plan::NotFound), "{path}");
        }
        assert_eq!(served(&files, "/link").unwrap(), "hello from A");
        let Plan::Listing(html) = files.plan("/") else {
            panic!("no listing");
        };
        assert!(html.contains("./link\"") && !html.contains("./out\"") && !html.contains("env"));
    }

    #[test]
    fn folders_list_their_entries_or_open_their_site() {
        let root = tree();
        let files = site(root.path(), false, false);
        let Plan::Redirect(location) = files.plan("/sub") else {
            panic!("no redirect");
        };
        assert_eq!(location, "/sub/");
        let Plan::Listing(html) = files.plan("/") else {
            panic!("no listing");
        };
        assert!(html.contains("<a href=\"./sub/\">sub/</a>"), "{html}");
        assert!(html.contains("<a href=\"./hello.txt\">hello.txt</a>"));
        assert!(html.find("./sub/").unwrap() < html.find("./hello.txt").unwrap());
        assert!(!html.contains("../") && !html.contains(".env"));
        let Plan::Listing(html) = files.plan("/sub/") else {
            panic!("no listing");
        };
        assert!(html.contains("Index of /sub/") && html.contains("<a href=\"../\">"));

        let web = site(root.path(), true, false);
        assert_eq!(served(&web, "/sub/").unwrap(), "<p>sub</p>");
        // No index.html: no listing either.
        assert!(matches!(web.plan("/"), Plan::NotFound));
        std::fs::write(root.path().join("404.html"), "lost").unwrap();
        let Plan::File(found, StatusCode::NOT_FOUND) = web.plan("/") else {
            panic!("no 404 page");
        };
        assert_eq!(std::fs::read_to_string(found.path).unwrap(), "lost");
        assert!(matches!(files.plan("/missing"), Plan::NotFound));
    }

    #[cfg(unix)]
    #[test]
    fn listed_names_are_escaped_and_encoded() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("<b>&\"x' y.txt"), "").unwrap();
        std::fs::write(root.path().join("javascript:alert(1)"), "").unwrap();
        let Plan::Listing(html) = site(root.path(), false, false).plan("/") else {
            panic!("no listing");
        };
        assert!(html.contains(
            "<a href=\"./%3Cb%3E%26%22x%27%20y.txt\">&lt;b&gt;&amp;&quot;x&#39; y.txt</a>"
        ));
        assert!(html.contains("href=\"./javascript%3Aalert%281%29\""));
    }

    #[test]
    fn ranges_follow_http() {
        assert_eq!(wanted_range(None, 10), Wanted::All);
        assert_eq!(wanted_range(Some("bytes=0-4"), 10), Wanted::Part(0, 4));
        assert_eq!(wanted_range(Some("bytes=5-"), 10), Wanted::Part(5, 9));
        assert_eq!(wanted_range(Some("bytes=5-100"), 10), Wanted::Part(5, 9));
        assert_eq!(wanted_range(Some("bytes=-3"), 10), Wanted::Part(7, 9));
        assert_eq!(wanted_range(Some("bytes=-30"), 10), Wanted::Part(0, 9));
        assert_eq!(wanted_range(Some("bytes=10-"), 10), Wanted::Unsatisfiable);
        assert_eq!(wanted_range(Some("bytes=-0"), 10), Wanted::Unsatisfiable);
        assert_eq!(wanted_range(Some("bytes=0-"), 0), Wanted::Unsatisfiable);
        for ignored in [
            "bytes=4-2",
            "bytes=0-1,4-5",
            "items=0-1",
            "bytes=x-1",
            "bytes=1",
        ] {
            assert_eq!(wanted_range(Some(ignored), 10), Wanted::All, "{ignored}");
        }
    }

    #[test]
    fn addresses_given_as_a_folder_point_to_tunnel_tcp() {
        for (given, address) in [
            ("127.0.0.1:3000", Some("127.0.0.1:3000")),
            ("3000", Some("127.0.0.1:3000")),
            ("http://localhost:8000/", Some("localhost:8000")),
            ("[::1]:8080", Some("[::1]:8080")),
            ("./site", None),
            ("0", None),
        ] {
            assert_eq!(
                address_like(Path::new(given)).as_deref(),
                address,
                "{given}"
            );
        }
        let error = Site::open(Path::new("127.0.0.1:3000"), false, false)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("use `tunnel tcp serve --to 127.0.0.1:3000`"),
            "{error}"
        );
        let root = tree();
        let error = Site::open(&root.path().join("hello.txt"), false, false)
            .err()
            .unwrap()
            .to_string();
        assert!(error.ends_with("hello.txt is not a folder"), "{error}");
    }

    #[test]
    fn only_addresses_and_localhost_are_accepted_hosts() {
        let host = |value: &str| local_host(Some(&HeaderValue::from_str(value).unwrap()));
        for value in [
            "127.0.0.1:8080",
            "localhost:8080",
            "LOCALHOST",
            "[::1]:8080",
            "[::1]",
            "192.168.1.10:8080",
        ] {
            assert!(host(value), "{value}");
        }
        for value in [
            "evil.example:8080",
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.nip.io:8080",
            "[::1]x",
            "",
        ] {
            assert!(!host(value), "{value}");
        }
        assert!(local_host(None));
    }

    #[test]
    fn only_scriptable_types_are_sandboxed_and_urls_reach_loopback() {
        assert!(scriptable(content_type(Path::new("a.HTML"))));
        assert!(scriptable(content_type(Path::new("a.svg"))));
        for path in ["a.png", "a.pdf", "a.mp4", "a.txt", "a.bin"] {
            assert!(!scriptable(content_type(Path::new(path))), "{path}");
        }
        assert_eq!(
            url("0.0.0.0:8080".parse().unwrap()),
            "http://127.0.0.1:8080/"
        );
        assert_eq!(url("[::]:8080".parse().unwrap()), "http://[::1]:8080/");
        assert_eq!(url("[::1]:80".parse().unwrap()), "http://[::1]:80/");
    }

    #[tokio::test]
    async fn a_part_of_a_large_file_streams_whole() {
        let root = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let path = root.path().join("big.bin");
        std::fs::write(&path, &bytes).unwrap();
        let body = open_part(&path, 70_000, 200_000).await.unwrap();
        let streamed = body.collect().await.unwrap().to_bytes();
        assert_eq!(&streamed[..], &bytes[70_000..270_000]);
    }
}
