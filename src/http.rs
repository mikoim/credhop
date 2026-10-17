//! HTTPS requests for `credhop usage`, behind a trait so tests can fake them.

use std::fmt;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;

/// The requests the providers make. Each parses the JSON response and turns a
/// non-2xx status into a [`StatusError`].
pub trait Http: Sync {
    /// GETs `url` with extra `headers`.
    fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value>;
    /// POSTs `body` as JSON.
    fn post_json(&self, url: &str, body: &Value) -> Result<Value>;
    /// POSTs `form` as `application/x-www-form-urlencoded`.
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Value>;
}

/// A non-2xx response. The body is never kept: it may hold credentials.
#[derive(Debug)]
pub struct StatusError {
    pub status: u16,
    pub host: String,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} from {}", self.status, self.host)
    }
}

impl std::error::Error for StatusError {}

/// The host of `url` for messages: `https://api.anthropic.com/api/x` → `api.anthropic.com`.
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// [`Http`] over HTTPS with ureq.
pub struct Ureq {
    agent: ureq::Agent,
}

impl Default for Ureq {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .https_only(true)
            .user_agent(concat!("credhop/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self { agent }
    }
}

impl Http for Ureq {
    fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
        let mut request = self.agent.get(url);
        for &(name, value) in headers {
            request = request.header(name, value);
        }
        read_json(url, request.call())
    }

    fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let body = body.to_string();
        let response = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(body.as_str());
        read_json(url, response)
    }

    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Value> {
        read_json(url, self.agent.post(url).send_form(form.iter().copied()))
    }
}

/// Parses a ureq response as JSON, naming only the host in errors.
fn read_json(
    url: &str,
    response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Value> {
    let host = host(url);
    let mut response = response.map_err(|e| match e {
        ureq::Error::StatusCode(status) => anyhow::Error::new(StatusError {
            status,
            host: host.to_owned(),
        }),
        e => anyhow!("request to {host} failed: {e}"),
    })?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow!("request to {host} failed: {e}"))?;
    serde_json::from_str(&body).map_err(|_| anyhow!("unexpected response from {host}"))
}

/// An [`Http`] for tests: canned answers and a record of the requests.
#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use serde_json::Value;

    use super::{Http, StatusError, host};

    /// What a request carried besides its headers.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Payload {
        None,
        Json(Value),
        Form(Vec<(String, String)>),
    }

    /// A request [`FakeHttp`] received.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Request {
        pub method: &'static str,
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub payload: Payload,
    }

    impl Request {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, value)| value.as_str())
        }
    }

    type Respond = dyn Fn(&Request) -> Result<Value, u16> + Send + Sync;

    /// Answers each request with `respond`, a JSON body or an error status, and records it.
    pub struct FakeHttp {
        respond: Box<Respond>,
        requests: Mutex<Vec<Request>>,
    }

    impl FakeHttp {
        pub fn new(
            respond: impl Fn(&Request) -> Result<Value, u16> + Send + Sync + 'static,
        ) -> Self {
            Self {
                respond: Box::new(respond),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// The requests received so far, in order.
        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        fn answer(
            &self,
            method: &'static str,
            url: &str,
            headers: &[(&str, &str)],
            payload: Payload,
        ) -> anyhow::Result<Value> {
            let request = Request {
                method,
                url: url.to_owned(),
                headers: headers
                    .iter()
                    .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
                payload,
            };
            let response = (self.respond)(&request);
            self.requests.lock().unwrap().push(request);
            response.map_err(|status| {
                StatusError {
                    status,
                    host: host(url).to_owned(),
                }
                .into()
            })
        }
    }

    impl Http for FakeHttp {
        fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> anyhow::Result<Value> {
            self.answer("GET", url, headers, Payload::None)
        }

        fn post_json(&self, url: &str, body: &Value) -> anyhow::Result<Value> {
            self.answer("POST", url, &[], Payload::Json(body.clone()))
        }

        fn post_form(&self, url: &str, form: &[(&str, &str)]) -> anyhow::Result<Value> {
            let form = form
                .iter()
                .map(|&(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            self.answer("POST", url, &[], Payload::Form(form))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeHttp, Payload};
    use super::*;
    use serde_json::json;

    #[test]
    fn host_strips_the_scheme_and_the_path() {
        assert_eq!(
            host("https://api.anthropic.com/api/oauth/usage"),
            "api.anthropic.com"
        );
        assert_eq!(host("https://chatgpt.com?x=1"), "chatgpt.com");
        assert_eq!(host("chatgpt.com"), "chatgpt.com");
    }

    #[test]
    fn status_error_names_only_the_status_and_the_host() {
        let error = StatusError {
            status: 401,
            host: "chatgpt.com".to_owned(),
        };
        assert_eq!(error.to_string(), "HTTP 401 from chatgpt.com");
    }

    #[test]
    fn ureq_transport_errors_name_only_the_host() {
        let err = Ureq::default()
            .get_json("http://127.0.0.1:9/usage", &[])
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("request to 127.0.0.1:9 failed: "),
            "{err}"
        );
    }

    #[test]
    fn fake_http_answers_and_records_requests() {
        let http = FakeHttp::new(|request| match request.method {
            "GET" => Ok(json!({"ok": true})),
            _ => Err(400),
        });

        let body = http
            .get_json("https://a.example/x", &[("Authorization", "Bearer t")])
            .unwrap();
        let err = http
            .post_form("https://b.example/y", &[("k", "v")])
            .unwrap_err();

        assert_eq!(body, json!({"ok": true}));
        let status = err.downcast_ref::<StatusError>().unwrap();
        assert_eq!((status.status, status.host.as_str()), (400, "b.example"));
        let requests = http.requests();
        assert_eq!(requests[0].header("Authorization"), Some("Bearer t"));
        assert_eq!(
            requests[1].payload,
            Payload::Form(vec![("k".to_owned(), "v".to_owned())])
        );
    }
}
