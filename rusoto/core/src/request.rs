//! AWS API requests.
//!
//! Wraps the `hyper` library to send PUT, POST, DELETE and GET requests.

//extern crate lazy_static;

use std::env;
use std::error::Error;
use std::fmt;
use std::io;
use std::io::Error as IoError;
use std::mem;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::tls::HttpsConnector;
use bytes::Bytes;
use futures::{Async, Future, Poll, Stream};
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use hyper::body::Payload;
use hyper::client::connect::Connect;
use hyper::client::Builder as HyperBuilder;
use hyper::client::HttpConnector;
use hyper::client::ResponseFuture as HyperResponseFuture;
use hyper::Error as HyperError;
use hyper::{Body, Client as HyperClient, Request as HyperRequest, Response as HyperResponse};
use tokio_timer::Timeout;

use log::Level::Debug;

use crate::signature::{SignedRequest, SignedRequestPayload};
use crate::stream::ByteStream;

// Pulls in the statically generated rustc version.
include!(concat!(env!("OUT_DIR"), "/user_agent_vars.rs"));

// Use a lazy static to cache the default User-Agent header
// because it never changes once it's been computed.
lazy_static! {
    static ref DEFAULT_USER_AGENT: String = format!(
        "rusoto/{} rust/{} {}",
        env!("CARGO_PKG_VERSION"),
        RUST_VERSION,
        env::consts::OS
    );
}

/// Stores the response from a HTTP request.
pub struct HttpResponse {
    /// Status code of HTTP Request
    pub status: StatusCode,
    /// Contents of Response
    pub body: ByteStream,
    /// Response headers
    pub headers: HeaderMap<String>,
}

/// Stores the buffered response from a HTTP request.
#[derive(PartialEq)]
pub struct BufferedHttpResponse {
    /// Status code of HTTP Request
    pub status: StatusCode,
    /// Contents of Response
    pub body: Bytes,
    /// Response headers
    pub headers: HeaderMap<String>,
}

impl BufferedHttpResponse {
    ///! Best effort to turn response body into more readable &str.
    pub fn body_as_str(&self) -> &str {
        match std::str::from_utf8(&self.body) {
            Ok(msg) => msg,
            _ => "unknown error",
        }
    }
}

/// Best effort based Debug implementation to make generic error's body more readable.
impl fmt::Debug for BufferedHttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match std::str::from_utf8(&self.body) {
            Ok(msg) => write!(
                f,
                "BufferedHttpResponse {{status: {:?}, body: {:?}, headers: {:?} }}",
                self.status, msg, self.headers
            ),
            _ => write!(
                f,
                "BufferedHttpResponse {{ status: {:?}, body: {:?}, headers: {:?} }}",
                self.status, self.body, self.headers
            ),
        }
    }
}

/// Future returned from `HttpResponse::buffer`.
pub struct BufferedHttpResponseFuture {
    status: StatusCode,
    headers: HeaderMap<String>,
    future: ::futures::stream::Concat2<ByteStream>,
}

impl Future for BufferedHttpResponseFuture {
    type Item = BufferedHttpResponse;
    type Error = HttpDispatchError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.future
            .poll()
            .map_err(std::convert::Into::into)
            .map(|r#async| {
                r#async.map(|body| BufferedHttpResponse {
                    status: self.status,
                    headers: mem::replace(&mut self.headers, Default::default()),
                    body,
                })
            })
    }
}

impl HttpResponse {
    /// Buffer the full response body in memory, resulting in a `BufferedHttpResponse`.
    pub fn buffer(self) -> BufferedHttpResponseFuture {
        BufferedHttpResponseFuture {
            status: self.status,
            headers: self.headers,
            future: self.body.concat2(),
        }
    }

    fn from_hyper(hyper_response: HyperResponse<Body>) -> HttpResponse {
        let status = hyper_response.status();
        let headers = hyper_response
            .headers()
            .iter()
            .map(|(h, v)| {
                let value_string = v.to_str().unwrap().to_owned();
                (h.clone(), value_string)
            })
            .collect();
        let body = hyper_response
            .into_body()
            .map(hyper::Chunk::into_bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err));

        HttpResponse {
            status,
            headers,
            body: ByteStream::new(body),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
/// An error produced when sending the request, such as a timeout error.
pub enum HttpDispatchError {
    /// Request timed out
    Timeout,
    /// Error from client future
    ClientFutureError (String),
    /// Internal error in deadline implementation.
    DeadlineError (String),
    /// Error from hyper
    HyperError (String),
    /// Io error
    IoError (String),
}

impl Error for HttpDispatchError {
    fn description(&self) -> &str {
        match self {
            Self::Timeout => "Request timed out",
            Self::ClientFutureError(s)
            | Self::DeadlineError(s)
            | Self::HyperError (s)
            | Self::IoError (s) => &s,
        }
    }
}

impl fmt::Display for HttpDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

impl From<HyperError> for HttpDispatchError {
    fn from(err: HyperError) -> HttpDispatchError {
        HttpDispatchError::HyperError (err.to_string())
    }
}

impl From<IoError> for HttpDispatchError {
    fn from(err: IoError) -> HttpDispatchError {
        HttpDispatchError::IoError (err.to_string())
    }
}

/// Trait for implementing HTTP Request/Response
pub trait DispatchSignedRequest {
    /// The future response value.
    type Future: Future<Item = HttpResponse, Error = HttpDispatchError> + 'static;
    /// Dispatch Request, and then return a Response
    fn dispatch(&self, request: SignedRequest, timeout: Option<Duration>) -> Self::Future;
}

impl<D: DispatchSignedRequest> DispatchSignedRequest for Rc<D> {
    type Future = D::Future;
    fn dispatch(&self, request: SignedRequest, timeout: Option<Duration>) -> Self::Future {
        D::dispatch(&*self, request, timeout)
    }
}

impl<D: DispatchSignedRequest> DispatchSignedRequest for Arc<D> {
    type Future = D::Future;
    fn dispatch(&self, request: SignedRequest, timeout: Option<Duration>) -> Self::Future {
        D::dispatch(&*self, request, timeout)
    }
}

/// A future that will resolve to an `HttpResponse`.
pub struct HttpClientFuture(ClientFutureInner);

enum ClientFutureInner {
    Hyper(HyperResponseFuture),
    HyperWithTimeout(Timeout<HyperResponseFuture>),
    Error(String),
}

impl Future for HttpClientFuture {
    type Item = HttpResponse;
    type Error = HttpDispatchError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0 {
            ClientFutureInner::Error(ref message) => Err(HttpDispatchError::ClientFutureError (message.clone())),
            ClientFutureInner::Hyper(ref mut hyper_future) => {
                Ok(hyper_future.poll()?.map(HttpResponse::from_hyper))
            }
            ClientFutureInner::HyperWithTimeout(ref mut deadline_future) => {
                match deadline_future.poll() {
                    Err(deadline_err) => {
                        if deadline_err.is_elapsed() {
                            Err(HttpDispatchError::Timeout)
                        } else if deadline_err.is_inner() {
                            Err(deadline_err.into_inner().unwrap().into())
                        } else {
                            Err(HttpDispatchError::DeadlineError (deadline_err.to_string()))
                        }
                    }
                    Ok(Async::NotReady) => Ok(Async::NotReady),
                    Ok(Async::Ready(hyper_res)) => {
                        Ok(Async::Ready(HttpResponse::from_hyper(hyper_res)))
                    }
                }
            }
        }
    }
}

struct HttpClientPayload {
    inner: Option<SignedRequestPayload>,
}

impl Payload for HttpClientPayload {
    type Data = io::Cursor<Bytes>;
    type Error = io::Error;

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        match self.inner {
            None => Ok(Async::Ready(None)),
            Some(SignedRequestPayload::Buffer(ref mut buffer)) => {
                if buffer.is_empty() {
                    Ok(Async::Ready(None))
                } else {
                    Ok(Async::Ready(Some(io::Cursor::new(buffer.split_off(0)))))
                }
            }
            Some(SignedRequestPayload::Stream(ref mut stream)) => match stream.poll()? {
                Async::NotReady => Ok(Async::NotReady),
                Async::Ready(None) => Ok(Async::Ready(None)),
                Async::Ready(Some(buffer)) => Ok(Async::Ready(Some(io::Cursor::new(buffer)))),
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        match self.inner {
            None => true,
            Some(SignedRequestPayload::Buffer(ref buffer)) => buffer.is_empty(),
            Some(SignedRequestPayload::Stream(_)) => false,
        }
    }

    fn content_length(&self) -> Option<u64> {
        match self.inner {
            None => Some(0),
            Some(SignedRequestPayload::Buffer(ref buffer)) => Some(buffer.len() as u64),
            Some(SignedRequestPayload::Stream(ref stream)) => stream.size_hint().map(|s| s as u64),
        }
    }
}

/// Http client for use with AWS services.
pub struct HttpClient<C = HttpsConnector<HttpConnector>> {
    inner: HyperClient<C, HttpClientPayload>,
}

impl HttpClient {
    /// Create a tls-enabled http client.
    pub fn new() -> Result<Self, TlsError> {
        #[cfg(feature = "native-tls")]
        let connector = match HttpsConnector::new(4) {
            Ok(connector) => connector,
            Err(tls_error) => {
                return Err(TlsError {
                    message: format!("Couldn't create NativeTlsClient: {}", tls_error),
                })
            }
        };

        #[cfg(feature = "rustls")]
        let connector = HttpsConnector::new(4);

        Ok(Self::from_connector(connector))
    }

    /// Create a tls-enabled http client.
    pub fn new_with_config(config: HttpConfig) -> Result<Self, TlsError> {
        #[cfg(feature = "native-tls")]
        let connector = match HttpsConnector::new(4) {
            Ok(connector) => connector,
            Err(tls_error) => {
                return Err(TlsError {
                    message: format!("Couldn't create NativeTlsClient: {}", tls_error),
                })
            }
        };

        #[cfg(feature = "rustls")]
        let connector = HttpsConnector::new(4);

        Ok(Self::from_connector_with_config(connector, config))
    }
}

impl<C> HttpClient<C>
where
    C: Connect,
    C::Future: 'static,
{
    /// Allows for a custom connector to be used with the HttpClient
    pub fn from_connector(connector: C) -> Self {
        let inner = HyperClient::builder().build(connector);
        HttpClient { inner }
    }

    /// Allows for a custom connector to be used with the HttpClient
    /// with extra configuration options
    pub fn from_connector_with_config(connector: C, config: HttpConfig) -> Self {
        let mut builder = HyperClient::builder();
        config
            .read_buf_size
            .map(|sz| builder.http1_read_buf_exact_size(sz));
        let inner = builder.build(connector);

        HttpClient { inner }
    }

    /// Alows for a custom builder and connector to be used with the HttpClient
    pub fn from_builder(builder: HyperBuilder, connector: C) -> Self {
        let inner = builder.build(connector);
        HttpClient { inner }
    }
}

/// Configuration options for the HTTP Client
pub struct HttpConfig {
    read_buf_size: Option<usize>,
}

impl HttpConfig {
    /// Create a new HttpConfig
    pub fn new() -> HttpConfig {
        HttpConfig {
            read_buf_size: None,
        }
    }
    /// Sets the size of the read buffer for inbound data
    /// A larger buffer size might result in better performance
    /// by requiring fewer copies out of the socket buffer.
    pub fn read_buf_size(&mut self, sz: usize) {
        self.read_buf_size = Some(sz);
    }
}

impl Default for HttpConfig {
    /// Create a new HttpConfig. Same as HttpConfig::new().
    fn default() -> HttpConfig {
        HttpConfig::new()
    }
}

impl<C> DispatchSignedRequest for HttpClient<C>
where
    C: Connect + 'static,
    C::Future: 'static,
{
    type Future = HttpClientFuture;

    fn dispatch(&self, request: SignedRequest, timeout: Option<Duration>) -> Self::Future {
        let hyper_method = match request.method().as_ref() {
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "DELETE" => Method::DELETE,
            "GET" => Method::GET,
            "HEAD" => Method::HEAD,
            v => {
                return HttpClientFuture(ClientFutureInner::Error(format!(
                    "Unsupported HTTP verb {}",
                    v
                )))
            }
        };

        // translate the headers map to a format Hyper likes
        let mut hyper_headers = HeaderMap::new();
        for h in request.headers().iter() {
            let header_name = match h.0.parse::<HeaderName>() {
                Ok(name) => name,
                Err(err) => {
                    return HttpClientFuture(ClientFutureInner::Error(format!(
                        "error parsing header name: {}",
                        err
                    )));
                }
            };
            for v in h.1.iter() {
                let header_value = match HeaderValue::from_bytes(v) {
                    Ok(value) => value,
                    Err(err) => {
                        return HttpClientFuture(ClientFutureInner::Error(format!(
                            "error parsing header value: {}",
                            err
                        )));
                    }
                };
                hyper_headers.append(&header_name, header_value);
            }
        }

        // Add a default user-agent header if one is not already present.
        if !hyper_headers.contains_key("user-agent") {
            hyper_headers.insert("user-agent", DEFAULT_USER_AGENT.parse().unwrap());
        }

        let mut final_uri = format!(
            "{}://{}{}",
            request.scheme(),
            request.hostname(),
            request.canonical_path()
        );
        if !request.canonical_query_string().is_empty() {
            final_uri = final_uri + &format!("?{}", request.canonical_query_string());
        }

        if log_enabled!(Debug) {
            let payload = match request.payload {
                Some(SignedRequestPayload::Buffer(ref payload_bytes)) => {
                    String::from_utf8(payload_bytes.as_ref().to_owned())
                        .unwrap_or_else(|_| String::from("<non-UTF-8 data>"))
                }
                Some(SignedRequestPayload::Stream(ref stream)) => {
                    format!("<stream size_hint={:?}>", stream.size_hint())
                }
                None => "".to_owned(),
            };

            debug!(
                "Full request: \n method: {}\n final_uri: {}\n payload: {}\nHeaders:\n",
                hyper_method, final_uri, payload
            );
            for (h, v) in hyper_headers.iter() {
                debug!("{}:{:?}", h.as_str(), v);
            }
        }

        let mut http_request_builder = HyperRequest::builder();
        http_request_builder.method(hyper_method);
        http_request_builder.uri(final_uri);

        let body = HttpClientPayload {
            inner: request.payload,
        };
        let mut http_request = match http_request_builder.body(body) {
            Ok(request) => request,
            Err(err) => {
                return HttpClientFuture(ClientFutureInner::Error(format!(
                    "error building request: {}",
                    err
                )));
            }
        };

        *http_request.headers_mut() = hyper_headers;

        let inner = match timeout {
            None => ClientFutureInner::Hyper(self.inner.request(http_request)),
            Some(duration) => {
                let future = Timeout::new(self.inner.request(http_request), duration);
                ClientFutureInner::HyperWithTimeout(future)
            }
        };

        HttpClientFuture(inner)
    }
}

#[derive(Debug, PartialEq)]
/// An error produced when the user has an invalid TLS client
pub struct TlsError {
    message: String,
}

impl Error for TlsError {
    fn description(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::SignedRequest;
    use crate::Region;

    #[test]
    fn http_client_is_send_and_sync() {
        fn is_send_and_sync<T: Send + Sync>() {}

        is_send_and_sync::<HttpClient>();
    }

    #[test]
    fn http_client_future_is_send() {
        fn is_send<T: Send>() {}

        is_send::<HttpClientFuture>();
    }

    #[test]
    fn custom_region_http() {
        let a_region = Region::Custom {
            endpoint: "http://localhost".to_owned(),
            name: "eu-west-3".to_owned(),
        };
        let request = SignedRequest::new("POST", "sqs", &a_region, "/");
        assert_eq!("http", request.scheme());
        assert_eq!("localhost", request.hostname());
    }

    #[test]
    fn custom_region_https() {
        let a_region = Region::Custom {
            endpoint: "https://localhost".to_owned(),
            name: "eu-west-3".to_owned(),
        };
        let request = SignedRequest::new("POST", "sqs", &a_region, "/");
        assert_eq!("https", request.scheme());
        assert_eq!("localhost", request.hostname());
    }

    #[test]
    fn custom_region_with_port() {
        let a_region = Region::Custom {
            endpoint: "https://localhost:8000".to_owned(),
            name: "eu-west-3".to_owned(),
        };
        let request = SignedRequest::new("POST", "sqs", &a_region, "/");
        assert_eq!("https", request.scheme());
        assert_eq!("localhost:8000", request.hostname());
    }

    #[test]
    fn custom_region_no_scheme() {
        let a_region = Region::Custom {
            endpoint: "localhost".to_owned(),
            name: "eu-west-3".to_owned(),
        };
        let request = SignedRequest::new("POST", "sqs", &a_region, "/");
        assert_eq!("https", request.scheme());
        assert_eq!("localhost", request.hostname());
    }

    #[test]
    fn from_io_error_preserves_error_message() {
        let io_error = ::std::io::Error::new(::std::io::ErrorKind::Other, "my error message");
        let error = HttpDispatchError::from(io_error);
        assert_eq!(error.to_string(), "my error message")
    }
}
